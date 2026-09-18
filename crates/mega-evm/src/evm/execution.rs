//! The Satin handler and the frame lifecycle of [`MegaEvm`].
//!
//! [`MegaHandler`] runs a transaction through op-revm's [`OpHandler`] and overrides the phases
//! `MegaETH` extends. [`MegaEvm`] implements revm's [`EvmTr`] and [`InspectorEvmTr`] itself, so the
//! frame lifecycle (`frame_init`, `frame_run`, `frame_return_result`) is `MegaETH`'s own.

#[cfg(not(feature = "std"))]
use alloc as std;
use op_revm::{
    handler::{IsTxError, OpHandler},
    OpHaltReason, OpTransactionError,
};
use std::vec::Vec;

use op_revm::precompiles::OpPrecompiles;
use revm::{
    context::{
        result::FromStringError, transaction::TransactionType, ContextError, ContextTr, FrameStack,
        JournalTr, Transaction,
    },
    context_interface::{
        cfg::gas::GasTracker,
        journaled_state::{account::JournaledAccountTr, entry::JournalEntry},
    },
    handler::{
        evm::{ContextDbError, FrameInitResult, FrameTr},
        instructions::InstructionProvider,
        EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, Handler, ItemOrResult,
        PreExecutionOutput,
    },
    inspector::{
        handler::{frame_start, inspect_instructions},
        InspectorEvmTr, InspectorHandler, JournalExt,
    },
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallScheme, FrameInput,
        InitialAndFloorGas, InstructionResult, InterpreterAction,
    },
    primitives::{Address, Bytes, CALL_STACK_LIMIT},
    Database, Inspector, Journal,
};

use crate::{
    evm::inspector::frame_end_checked, synthetic_frame_result, ExternalEnvTypes, LimitCheck,
    MegaContext, MegaEvm, MegaInstructions,
};

/// The Satin handler.
///
/// It wraps op-revm's [`OpHandler`] and delegates every phase `MegaETH` does not extend to it.
#[derive(Debug)]
pub struct MegaHandler<EVM, ERROR, FRAME> {
    op: OpHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> MegaHandler<EVM, ERROR, FRAME> {
    /// Creates a handler.
    pub fn new() -> Self {
        Self { op: OpHandler::new() }
    }
}

impl<EVM, ERROR, FRAME> Default for MegaHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB, EVM, ERROR, FRAME, ExtEnvs> Handler for MegaHandler<EVM, ERROR, FRAME>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    EVM: EvmTr<Context = MegaContext<DB, ExtEnvs>, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError> + FromStringError + IsTxError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = OpHaltReason;

    /// revm's pre-execution, then the write records of the EIP-7702 authorities it applied.
    ///
    /// When those records would cross a limit, the limit is enforced before the writes it
    /// guards: the authorizations are taken back with the gas they charged, and the transaction,
    /// latched, is stopped at its first frame.
    fn pre_execution(
        &self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<PreExecutionOutput>, Self::Error> {
        self.load_accounts(evm)?;
        let checkpoint = evm.ctx().journal_mut().checkpoint();
        let gas_before = *gas;
        let Some(eip7702_refund) = self.apply_eip7702_auth_list(evm, gas)? else {
            evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            return Ok(None);
        };
        if record_applied_authorities(evm.ctx_mut(), checkpoint.journal_i).exceeded_limit() {
            evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            *gas = gas_before;
            let checkpoint = evm.ctx().journal_mut().checkpoint();
            return Ok(Some(PreExecutionOutput { eip7702_refund: 0, checkpoint }));
        }
        Ok(Some(PreExecutionOutput { eip7702_refund, checkpoint }))
    }

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        self.op.validate_env(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        self.op.validate_against_state_and_deduct_caller(evm, init_and_floor_gas)
    }

    /// Settles the outermost frame: pops its lane and, when the transaction is latched, turns its
    /// result into the latched stop; then settles its gas into the transaction's as op-revm does
    /// (op-revm replaces revm's settlement, so revm's never runs here): a stopped transaction
    /// settles like an EIP-8037 revert, its unspent regular gas and reservoir back to the sender.
    /// Keeps the history gas the transaction spent.
    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut FrameResult,
        parent_gas: &mut GasTracker,
    ) -> Result<(), Self::Error> {
        evm.ctx_mut().additional_limit.on_last_frame_return(frame_result);
        self.op.last_frame_result(evm, frame_result, parent_gas)?;
        let history = frame_result.gas().history_gas_spent().max(0) as u64;
        evm.ctx_mut().additional_limit.set_history_gas_spent(history);
        Ok(())
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        self.op.reimburse_caller(evm, exec_result)
    }

    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        eip7702_refund: i64,
    ) -> Result<(), Self::Error> {
        self.op.refund(evm, exec_result, eip7702_refund)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        self.op.reward_beneficiary(evm, exec_result)
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: FrameResult,
        result_gas: revm::context::result::ResultGas,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error> {
        self.op.execution_result(evm, result, result_gas)
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error> {
        self.op.catch_error(evm, error)
    }
}

impl<DB, EVM, ERROR, ExtEnvs> InspectorHandler for MegaHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    MegaContext<DB, ExtEnvs>: ContextTr<Journal = Journal<DB>>,
    Journal<DB>: JournalExt,
    EVM: InspectorEvmTr<
        Context = MegaContext<DB, ExtEnvs>,
        Frame = EthFrame<EthInterpreter>,
        Inspector: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    >,
    // Implied by `EvmTrError<EVM>`, but the `ContextTr<Journal = Journal<DB>>` bound above stops
    // the compiler from normalizing the context's database type down to `DB`.
    ERROR: EvmTrError<EVM>
        + From<DB::Error>
        + From<ContextError<DB::Error>>
        + From<OpTransactionError>
        + FromStringError
        + IsTxError,
{
    type IT = EthInterpreter;
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> EvmTr for MegaEvm<DB, INSP, ExtEnvs> {
    type Context = MegaContext<DB, ExtEnvs>;
    type Instructions = MegaInstructions<DB, ExtEnvs>;
    type Precompiles = OpPrecompiles;
    type Frame = EthFrame<EthInterpreter>;

    #[inline]
    fn all(
        &self,
    ) -> (&Self::Context, &Self::Instructions, &Self::Precompiles, &FrameStack<Self::Frame>) {
        self.inner.all()
    }

    #[inline]
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    /// Starts a frame, in this order:
    ///
    /// 1. the latch: a latched transaction's frame is answered with the stop;
    /// 2. the depth guard: a `CALL` or `STATICCALL` past the call-stack limit is answered with
    ///    `CallTooDeep` before anything could intercept it;
    /// 3. system contract interception ([`MegaEvm::intercept`]);
    /// 4. the keyless deployment rewrite ([`MegaEvm::rewrite_keyless`]);
    /// 5. the frame's lane is pushed and the writes its start makes are counted; a limit they cross
    ///    answers the frame with the stop before it runs;
    /// 6. revm builds the frame.
    ///
    /// A frame answered before revm builds it gets an empty lane, so the lanes stay aligned with
    /// the results [`frame_return_result`](EvmTr::frame_return_result) pops.
    #[inline]
    fn frame_init(
        &mut self,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        if let Some(result) = answer_before_building(&mut self.inner.ctx, &frame_init)? {
            self.inner.ctx.additional_limit.push_empty_frame();
            return Ok(ItemOrResult::Result(result));
        }
        if let Some(result) = self.intercept(&frame_init) {
            self.inner.ctx.additional_limit.push_empty_frame();
            return Ok(ItemOrResult::Result(result));
        }
        let frame_init = self.rewrite_keyless(frame_init);
        let ctx = &mut self.inner.ctx;
        let check = ctx.additional_limit.on_frame_init(&frame_init.frame_input, frame_init.depth);
        if check.exceeded_limit() {
            return Ok(ItemOrResult::Result(stop_before_building(ctx, &frame_init, &check)?));
        }
        // The creator of a nested creation, to tell afterwards whether revm bumped its nonce.
        let creator = match &frame_init.frame_input {
            FrameInput::Create(inputs) if frame_init.depth > 0 => {
                Some((inputs.caller(), account_nonce(ctx, inputs.caller())))
            }
            _ => None,
        };
        let outcome = match self.inner.frame_init(frame_init)? {
            ItemOrResult::Item(frame) => Ok(frame.interpreter.input.target_address),
            ItemOrResult::Result(result) => Err(result),
        };
        let ctx = &mut self.inner.ctx;
        match outcome {
            Ok(address) => {
                ctx.additional_limit.set_frame_address(address);
                Ok(ItemOrResult::Item(self.inner.frame_stack.get()))
            }
            Err(result) => {
                if let Some((creator, nonce)) = creator {
                    if account_nonce(ctx, creator) == nonce {
                        ctx.additional_limit.creation_did_not_bump_nonce();
                    }
                }
                Ok(ItemOrResult::Result(result))
            }
        }
    }

    /// Runs the frame on top of the stack, unless the transaction is latched: then the frame
    /// returns the stop without running another instruction (see [`before_frame_run`]).
    #[inline]
    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let evm = &mut self.inner;
        let frame = evm.frame_stack.get();
        let ctx = &mut evm.ctx;
        let action = match before_frame_run(ctx, frame) {
            Some(action) => action,
            None => frame.interpreter.run_plain(
                evm.instruction.instruction_table(),
                evm.instruction.gas_table(),
                ctx,
            ),
        };
        frame.process_next_action(ctx, action).inspect(|next| {
            if next.is_result() {
                frame.set_finished(true);
            }
        })
    }

    /// Pops the returning frame's lane (merged on success, discarded on failure; under a latch the
    /// result is first rewritten to the stop), then returns the result to the caller as revm
    /// does.
    #[inline]
    fn frame_return_result(
        &mut self,
        mut result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        self.inner.ctx.additional_limit.on_frame_return(&mut result);
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP, ExtEnvs> InspectorEvmTr for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
    type Inspector = INSP;

    #[inline]
    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        let evm = &self.inner;
        (&evm.ctx, &evm.instruction, &evm.precompiles, &evm.frame_stack, &evm.inspector)
    }

    #[inline]
    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        let evm = &mut self.inner;
        (
            &mut evm.ctx,
            &mut evm.instruction,
            &mut evm.precompiles,
            &mut evm.frame_stack,
            &mut evm.inspector,
        )
    }

    /// revm's inspected frame start, with the lanes kept aligned: a frame the inspector answers
    /// itself never reaches [`EvmTr::frame_init`], so an empty lane stands in for it.
    #[inline]
    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector) = self.ctx_inspector();
        if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            // The inspector answered the frame. The latch and the depth guard still hold: an
            // answer cannot start a frame of a stopped transaction, nor reach past the call-stack
            // limit.
            if let Some(answer) = answer_before_building(ctx, &frame_init)? {
                output = answer;
            }
            ctx.additional_limit.push_empty_frame();
            frame_end_checked(ctx, inspector, &frame_init.frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }
        let frame_input = frame_init.frame_input.clone();
        let logs_i = ctx.journal().logs().len();
        if let ItemOrResult::Result(mut output) = self.frame_init(frame_init)? {
            let (ctx, inspector) = self.ctx_inspector();
            // Logs the frame journaled without running: the EIP-7708 transfer log, and the logs
            // of a precompile.
            if ctx.journal().logs().len() != logs_i {
                inspect_logs(ctx, inspector, logs_i);
            }
            // Custom precompiles gather their logs outside the journal.
            if let FrameResult::Call(outcome) = &output {
                if outcome.was_precompile_called {
                    for log in outcome.precompile_call_logs.clone() {
                        inspector.log(ctx, log);
                    }
                }
            }
            frame_end_checked(ctx, inspector, &frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }
        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        if ctx.journal().logs().len() != logs_i {
            inspect_logs(ctx, inspector, logs_i);
        }
        inspector.initialize_interp(&mut frame.interpreter, ctx);
        Ok(ItemOrResult::Item(frame))
    }

    /// revm's inspected frame run, with the latch short-circuit of [`EvmTr::frame_run`]: a frame
    /// of a latched transaction returns the stop without a step, and the inspector sees it end.
    #[inline]
    fn inspect_frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector, frame, instructions) = self.ctx_inspector_frame_instructions();
        let action = match before_frame_run(ctx, frame) {
            Some(action) => action,
            None => inspect_instructions(
                ctx,
                &mut frame.interpreter,
                &mut *inspector,
                instructions.instruction_table(),
                instructions.gas_table(),
            ),
        };
        let mut next = frame.process_next_action(ctx, action);
        if let Ok(ItemOrResult::Result(result)) = &mut next {
            frame_end_checked(ctx, inspector, &frame.input, result);
            frame.set_finished(true);
        }
        next
    }
}

/// The action of a frame about to run: the latched stop, returned without running an
/// instruction, when the transaction is latched; `None` otherwise, and the frame runs.
///
/// A frame runs here for the first time or after a child returned into it. Under a latch it is
/// the latter: the child that crossed the limit reverted, and its caller must not resume.
#[inline]
fn before_frame_run<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    frame: &EthFrame<EthInterpreter>,
) -> Option<InterpreterAction> {
    let latched = ctx.additional_limit.latched()?;
    Some(InterpreterAction::new_return(
        InstructionResult::Revert,
        latched.revert_data(),
        frame.interpreter.gas,
    ))
}

/// The result of a frame a limit stopped before it ran: a revert with the stop's
/// [`MegaLimitExceeded`](crate::MegaLimitExceeded) output, as a
/// [`synthetic_frame_result`] that settles like a frame revm ran.
fn stopped_frame_result(frame_init: &FrameInit, check: &LimitCheck) -> FrameResult {
    synthetic_frame_result(&frame_init.frame_input, InstructionResult::Revert, check.revert_data())
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
    /// System contract interception: a `CALL` or `STATICCALL` to a system contract answered by
    /// `MegaETH` instead of the contract's code.
    ///
    /// The extension point of the system contract interceptors; nothing is intercepted yet. An
    /// answer is a [`synthetic_frame_result`](crate::synthetic_frame_result), so it settles like
    /// a frame revm ran.
    // Takes the EVM mutably: an interceptor reads and writes the context.
    #[allow(clippy::needless_pass_by_ref_mut)]
    #[inline]
    const fn intercept(&mut self, _frame_init: &FrameInit) -> Option<FrameResult> {
        None
    }

    /// The keyless deployment rewrite: a keyless deployment call turned into the native creation
    /// it stands for.
    ///
    /// The extension point of native keyless deployment; nothing is rewritten yet.
    // Takes the EVM mutably: the rewrite validates the deployment against the journal.
    #[allow(clippy::needless_pass_by_ref_mut)]
    #[inline]
    const fn rewrite_keyless(&mut self, frame_init: FrameInit) -> FrameInit {
        frame_init
    }
}

/// The answer a frame gets before anything builds or answers it otherwise, in this order: the
/// latched stop of a stopped transaction, then the depth guard's `CallTooDeep`.
fn answer_before_building<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &FrameInit,
) -> Result<Option<FrameResult>, ContextDbError<MegaContext<DB, ExtEnvs>>> {
    if let Some(latched) = ctx.additional_limit.latched().copied() {
        return stop_before_building(ctx, frame_init, &latched).map(Some);
    }
    Ok(call_too_deep(frame_init))
}

/// Answers a frame a limit stops before it is built with the stop.
///
/// A creation still bumps its creator's nonce, as one that starts and reverts does: a nested
/// creation's caller sees an ordinary failed creation, and a creation transaction stopped before
/// its first frame cannot be replayed.
fn stop_before_building<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &FrameInit,
    check: &LimitCheck,
) -> Result<FrameResult, ContextDbError<MegaContext<DB, ExtEnvs>>> {
    if let FrameInput::Create(inputs) = &frame_init.frame_input {
        let _ = ctx.journal_mut().load_account_mut(inputs.caller())?.data.bump_nonce();
    }
    Ok(stopped_frame_result(frame_init, check))
}

/// The depth guard: a `CALL` or `STATICCALL` past the call-stack limit, answered with
/// `CallTooDeep`, its gas untouched and its reservoir carried.
///
/// revm checks the depth when it builds a frame; an interceptor or an inspector answers before
/// revm builds anything, so without the guard a system contract could be reached at any depth.
/// `CALLCODE` and `DELEGATECALL` never reach an interceptor and are left to revm's own check.
fn call_too_deep(frame_init: &FrameInit) -> Option<FrameResult> {
    let FrameInput::Call(inputs) = &frame_init.frame_input else { return None };
    let guarded = matches!(inputs.scheme, CallScheme::Call | CallScheme::StaticCall);
    (guarded && frame_init.depth > CALL_STACK_LIMIT as usize).then(|| {
        synthetic_frame_result(
            &frame_init.frame_input,
            InstructionResult::CallTooDeep,
            Bytes::new(),
        )
    })
}

/// Hands the logs journaled since `logs_i` to the inspector, outside any interpreter.
#[cold]
#[inline(never)]
fn inspect_logs<DB: Database, ExtEnvs: ExternalEnvTypes, INSP>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    logs_i: usize,
) where
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
{
    let logs = ctx.journal().logs()[logs_i..].to_vec();
    for log in logs {
        inspector.log(ctx, log);
    }
}

/// The nonce of an account the journal holds; zero for one it does not.
fn account_nonce<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    address: Address,
) -> u64 {
    ctx.journal_ref().state.get(&address).map_or(0, |account| account.info.nonce)
}

/// Records the account writes of the EIP-7702 authorities applied since journal entry
/// `journal_i`: each applied authorization bumps its authority's nonce once, so the distinct
/// authorities other than the sender are the accounts written. The sender's write is part of the
/// transaction body.
fn record_applied_authorities<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    journal_i: usize,
) -> LimitCheck {
    if ctx.tx().tx_type() != TransactionType::Eip7702 {
        return LimitCheck::WithinLimit;
    }
    let caller = ctx.tx().caller();
    let target = ctx.tx().kind().to().copied();
    let mut authorities: Vec<Address> = ctx.journal_ref().journal()[journal_i..]
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::NonceBump { address } if *address != caller => Some(*address),
            _ => None,
        })
        .collect();
    authorities.sort_unstable();
    authorities.dedup();
    let target_is_authority =
        target.is_some_and(|target| authorities.binary_search(&target).is_ok());
    ctx.additional_limit.record_applied_authorities(
        caller,
        authorities.len() as u64,
        target_is_authority,
    )
}
