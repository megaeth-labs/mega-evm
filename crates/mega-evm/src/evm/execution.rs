//! The Satin handler and the frame lifecycle of [`MegaEvm`].
//!
//! [`MegaHandler`] runs a transaction through op-revm's [`OpHandler`] and overrides the phases
//! `MegaETH` extends. [`MegaEvm`] implements revm's [`EvmTr`] and [`InspectorEvmTr`] itself, so the
//! frame lifecycle (`frame_init`, `frame_run`, `frame_return_result`) is `MegaETH`'s own.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{cell::Cell, num::NonZeroU64};

use op_revm::{
    handler::{IsTxError, OpHandler},
    transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
    OpHaltReason, OpTransactionError,
};
use std::{boxed::Box, vec::Vec};

use alloy_evm::{precompiles::PrecompilesMap, Database};
use revm::{
    context::{
        result::{FromStringError, InvalidTransaction, ResultGas},
        transaction::TransactionType,
        Cfg, ContextError, ContextTr, FrameStack, JournalTr, Transaction,
    },
    context_interface::{
        cfg::{gas::GasTracker, GasId, StateGasCharge, StateGasSite},
        journaled_state::{account::JournaledAccountTr, entry::JournalEntry, JournalCheckpoint},
        Host,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult, FrameTr},
        execution::runtime_oog_unwind,
        instructions::InstructionProvider,
        EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, Handler, ItemOrResult,
        PreExecutionOutput,
    },
    inspector::{
        handler::{frame_start, inspect_instructions},
        InspectorEvmTr, InspectorHandler, JournalExt,
    },
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallInput, CallInputs,
        CallScheme, CallValue, CreateInputs, CreateScheme, FrameInput, InitialAndFloorGas,
        InstructionResult, InterpreterAction, InterpreterResult, SharedMemory,
    },
    primitives::{Address, Bytes, TxKind, CALL_STACK_LIMIT, U256},
    Inspector, Journal,
};

use crate::{
    evm::{history::transaction_body_bytes, inspector::frame_end_checked},
    history_gas, synthetic_frame_result,
    system::keyless::{self, Rewrite},
    write_record_history_gas, Detention, ExternalEnvTypes, JournalInspectTr, LimitCheck, LimitKind,
    MegaContext, MegaEvm, MegaInstructions, VolatileDataAccess,
};

/// The Satin handler.
///
/// It wraps op-revm's [`OpHandler`] and delegates every phase `MegaETH` does not extend to it.
/// One handler runs one transaction, so what a phase learns about it is kept here.
#[derive(Debug)]
pub struct MegaHandler<EVM, ERROR, FRAME> {
    op: OpHandler<EVM, ERROR, FRAME>,
    /// Whether validation found the caller of a deposit-like transaction empty, so executing it
    /// creates that account. Read by the pre-execution phase, which charges for the account
    /// before the transaction runs; by then the caller has been materialised and the fact is no
    /// longer visible in the state.
    deposit_creates_caller: Cell<bool>,
}

impl<EVM, ERROR, FRAME> MegaHandler<EVM, ERROR, FRAME> {
    /// Creates a handler.
    pub fn new() -> Self {
        Self { op: OpHandler::new(), deposit_creates_caller: Cell::new(false) }
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

    /// revm's pre-execution, with the account a deposit-like transaction creates for its caller
    /// charged first, then the write records of the EIP-7702 authorities it applied.
    ///
    /// The caller's account is charged where EIP-2780 charges the recipient's, and before it:
    /// a transaction that creates both pays for both, and a transaction whose recipient is its
    /// own caller pays once, because by the time EIP-2780 looks at the recipient the account
    /// exists.
    ///
    /// When those records would cross a limit, the limit is enforced before the writes it
    /// guards: the authorizations are taken back with the gas they charged, and the transaction,
    /// latched, is stopped at its first frame.
    ///
    /// The two kinds of write record made outside any frame are charged their history here, where
    /// they are made: one per applied authority, and the one the transaction's own frame makes —
    /// the recipient of its value, or the account it creates. Neither is known at validation: an
    /// authority applies or does not, and whether the recipient is already written depends on the
    /// authorities that did. A transaction that cannot pay for them runs out of gas before its
    /// first frame, the way one that cannot pay its authorizations does.
    ///
    /// A transaction whose body crossed the data-size limit is latched before it runs, and its
    /// first frame will be answered with the stop. Nothing after the caller's account is applied
    /// or charged for it: the authorizations would be taken back, and the records made outside a
    /// frame are records the limit rejects. So no charge made for them can run the transaction
    /// out of gas, and the stop that bound first is what it reports. The caller's account is
    /// charged all the same, because it exists whatever the transaction does.
    fn pre_execution(
        &self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<PreExecutionOutput>, Self::Error> {
        self.load_accounts(evm)?;
        let checkpoint = evm.ctx().journal_mut().checkpoint();
        if self.deposit_creates_caller.get() && !charge_created_caller(evm.ctx_mut(), gas) {
            evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            return Ok(None);
        }
        if evm.ctx_ref().additional_limit.latched().is_some() {
            return Ok(Some(PreExecutionOutput { eip7702_refund: 0, checkpoint }));
        }
        let gas_before = *gas;
        let Some(eip7702_refund) = self.apply_eip7702_auth_list(evm, gas)? else {
            evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            return Ok(None);
        };
        let authorities =
            record_applied_authorities(evm.ctx_mut(), checkpoint.journal_i, gas.state_gas_spent());
        if authorities.check.exceeded_limit() {
            evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            *gas = gas_before;
            let checkpoint = evm.ctx().journal_mut().checkpoint();
            return Ok(Some(PreExecutionOutput { eip7702_refund: 0, checkpoint }));
        }
        if !charge_records_made_outside_a_frame(evm.ctx_mut(), gas, authorities.applied) {
            evm.ctx().journal_mut().checkpoint_revert(checkpoint);
            return Ok(None);
        }
        Ok(Some(PreExecutionOutput { eip7702_refund, checkpoint }))
    }

    /// Decides whether the transaction is a system-address transaction and prepares the common
    /// execution layer for it, then validates a system-address transaction and promotes it to a
    /// deposit, then validates the transaction as op-revm does — as a deposit, when it was
    /// promoted.
    ///
    /// This is the handler's first phase, so the layer is prepared before anything else of the
    /// transaction runs, and a read the database cannot serve fails the transaction through the
    /// handler's error path, which discards what the journal loaded. Only a transaction of the
    /// system shape reads the live system address, from the journal and without warming it; every
    /// other transaction pays the shape test, a comparison or two on its own fields.
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        let ctx = evm.ctx_mut();
        let system_transaction = crate::system::is_live_system_transaction(ctx)?;
        ctx.on_new_tx(system_transaction);
        if system_transaction {
            crate::system::validate_and_promote::<_, _, Self::Error>(ctx)?;
        }
        self.op.validate_env(evm)
    }

    /// Notes whether this transaction's caller is an account executing it creates, then deducts
    /// the caller as op-revm does, which is what creates it.
    ///
    /// Only a deposit-like transaction can have one: every other transaction pays a fee, which
    /// an empty account cannot. The account is charged for in
    /// [`pre_execution`](Handler::pre_execution).
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        self.deposit_creates_caller.set(deposit_creates_caller(evm.ctx_mut())?);
        self.op.validate_against_state_and_deduct_caller(evm, init_and_floor_gas)
    }

    /// revm's intrinsic gas, with the history gas of the transaction's body added to the
    /// EIP-8037 intrinsic state-gas slot.
    ///
    /// The body's bytes are fixed before the transaction runs, so their price is part of what a
    /// gas limit has to cover for the transaction to be valid at all: a limit that falls short is
    /// rejected before inclusion rather than included as an out-of-gas that burns the whole limit.
    /// revm made that check on its own figure, so the grown figure is checked again here, with the
    /// same error naming the same two numbers.
    ///
    /// The slot it rides in is the state one because that is the pool EIP-8037 pays it from: the
    /// reservoir first, spilling onto the regular budget only past it, so a body does not take the
    /// execution cap away from computation. The cap check revm made stands as it was — it tests
    /// the regular intrinsic gas, which this does not touch — and
    /// [`post_execution`](Handler::post_execution) takes the body back out of the state gas the
    /// result reports.
    ///
    /// A transaction exempt from history gas carries none of it
    /// ([`MegaContext::prices_history`](crate::MegaContext::prices_history)), so its intrinsic gas
    /// is revm's own.
    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        let mut gas = self.op.validate_initial_tx_gas(evm)?;
        if !evm.ctx_ref().prices_history() {
            return Ok(gas);
        }
        let bytes = transaction_body_bytes(evm.ctx_ref().tx());
        // A byte count with no price saturates, which no gas limit covers: the transaction is
        // rejected for not covering its own intrinsic gas.
        let history = history_gas(bytes).unwrap_or(u64::MAX);
        let gas_limit = evm.ctx_ref().tx().gas_limit();
        let initial_gas = gas
            .initial_regular_gas()
            .saturating_add(gas.initial_state_gas_final().saturating_add(history));
        if initial_gas > gas_limit {
            return Err(
                InvalidTransaction::CallGasCostMoreThanGasLimit { gas_limit, initial_gas }.into()
            );
        }
        gas.set_initial_state_gas(gas.initial_state_gas_final() + history);
        evm.ctx_mut().additional_limit.set_intrinsic_history(history, bytes);
        Ok(gas)
    }

    /// revm's first frame, unless the transaction is already latched: then a frame input built on
    /// what the transaction has left, with nothing charged for the frame's start.
    ///
    /// A latched transaction's first frame is answered with the stop before revm builds it, so it
    /// makes none of the writes revm's EIP-2780 runtime charges price for its start — a value
    /// recipient's new account, a created account — and reaches no delegation target. Charging
    /// them would only matter to a gas limit that cannot pay them, which would then report an
    /// out-of-gas in place of the stop that bound first. The input carries no charged flag, so
    /// the stop's settlement gives nothing back that was not charged.
    ///
    /// Once revm has built the first frame, the state gas the transaction has been charged is all
    /// it holds outside its frames: the account a deposit-like transaction creates for its caller,
    /// the applied authorities, and the new account EIP-2780 charges the first frame's start for.
    /// It is held to the state-gas limit there, and a crossing latches the transaction: the frame
    /// is answered with the stop before it runs, and its settlement gives the start's charge back
    /// as it does for any first frame that fails.
    fn first_frame_input(
        &mut self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<FrameInit>, Self::Error> {
        if evm.ctx_ref().additional_limit.latched().is_some() {
            return Ok(Some(unbuilt_first_frame(evm.ctx_ref(), gas)));
        }
        let frame = self.op.first_frame_input(evm, gas)?;
        evm.ctx_mut().additional_limit.on_state_gas_before_frames(gas.state_gas_spent());
        evm.ctx_mut().mark_beneficiary_delegate();
        Ok(frame)
    }

    /// Settles the outermost frame: pops its lane and, when the transaction is latched, turns its
    /// result into the latched stop; then settles its gas into the transaction's as op-revm does
    /// (op-revm replaces revm's settlement, so revm's never runs here): a stopped transaction
    /// settles like an EIP-8037 revert, its unspent regular gas and reservoir back to the sender.
    /// Keeps the history gas the transaction spent: what the body was charged, and what the frames
    /// charged net of what they gave back; and the history bytes it appended, which are what those
    /// charges were made for.
    ///
    /// The outermost frame of a keyless deployment is the creation its `keylessDeploy` call
    /// started, and the call is the transaction's own frame: the creation's result is settled into
    /// the call first, which answers in the `IKeylessDeploy` ABI ([`keyless::settle`]). The
    /// translation is made here, where every first-frame result arrives — a creation answered at
    /// its start never returns through the frame lifecycle. The inspected path settles it earlier,
    /// to tell the inspector the call ended ([`InspectorHandler::inspect_execution`]), and the
    /// settlement here then finds nothing to do.
    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut FrameResult,
        parent_gas: &mut GasTracker,
    ) -> Result<(), Self::Error> {
        keyless::settle::<_, _, Self::Error>(evm.ctx_mut(), frame_result)?;
        evm.ctx_mut().additional_limit.on_last_frame_return(frame_result);
        self.op.last_frame_result(evm, frame_result, parent_gas)?;
        // The write record the transaction's own frame makes was charged before execution; a
        // frame that failed keeps no such write, so the charge goes back the way EIP-8037 gives
        // back the state gas of the account that frame would have created.
        let instruction_result = frame_result.instruction_result();
        if !instruction_result.is_ok() {
            parent_gas.refill_history(evm.ctx_ref().additional_limit.top_level_write_record_gas());
            if instruction_result.is_halt() {
                parent_gas.spend_all();
            }
            *frame_result.gas_mut().tracker_mut() = *parent_gas;
        }
        let priced = evm.ctx_ref().prices_history();
        let layer = &mut evm.ctx_mut().additional_limit;
        let history = layer
            .intrinsic_history_gas()
            .saturating_add_signed(frame_result.gas().history_gas_spent());
        layer.set_history_gas_spent(history);
        layer.settle_history_bytes(priced);
        Ok(())
    }

    /// revm's post-execution, on the intrinsic gas with the body's history taken back out of the
    /// state slot it rode in: the result's state gas is state gas alone, and the history the
    /// transaction spent is reported on its own ledger.
    fn post_execution(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<ResultGas, Self::Error> {
        let history = evm.ctx_ref().additional_limit.intrinsic_history_gas();
        let init_and_floor_gas = init_and_floor_gas
            .with_initial_state_gas(init_and_floor_gas.initial_state_gas_final() - history);
        self.op.post_execution(evm, exec_result, init_and_floor_gas, eip7702_gas_refund)
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

    /// revm's inspected execution, with the end of a keyless deployment's call reported.
    ///
    /// The call is the transaction's own frame, which runs no code, and its creation is the
    /// outermost frame revm runs: revm's loop tells the inspector the creation ended, and nothing
    /// tells it the call did. So the creation is settled into the call here, before
    /// [`last_frame_result`](Handler::last_frame_result) would settle it, and the inspector is
    /// told the call ended with the answer the settlement made. The rest is revm's.
    fn inspect_execution(
        &mut self,
        evm: &mut Self::Evm,
        checkpoint: JournalCheckpoint,
        gas: &mut GasTracker,
    ) -> Result<Option<FrameResult>, Self::Error> {
        let Some(first_frame_input) = self.first_frame_input(evm, gas)? else {
            unwind_runtime_oog(evm.ctx(), checkpoint)?;
            return Ok(None);
        };
        evm.ctx().journal_mut().checkpoint_commit();
        let mut frame_result = self.inspect_run_exec_loop(evm, first_frame_input)?;
        let (ctx, inspector) = evm.ctx_inspector();
        if let Some(call) = keyless::settle::<_, _, Self::Error>(ctx, &mut frame_result)? {
            frame_end_checked(ctx, inspector, &FrameInput::Call(call), &mut frame_result);
        }
        self.last_frame_result(evm, &mut frame_result, gas)?;
        Ok(Some(frame_result))
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> EvmTr for MegaEvm<DB, INSP, ExtEnvs> {
    type Context = MegaContext<DB, ExtEnvs>;
    type Instructions = MegaInstructions<DB, ExtEnvs>;
    type Precompiles = PrecompilesMap;
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
    /// 3. the keyless deployment rewrite ([`keyless::rewrite`]): a `keylessDeploy` call a
    ///    transaction makes becomes the creation it stands for, started below the call, or is
    ///    answered;
    /// 4. system contract interception ([`MegaEvm::intercept`]), which answers the frame or lets it
    ///    start;
    /// 5. the frame's lane is pushed and the writes its start makes are counted; a limit they cross
    ///    answers the frame with the stop before it runs. A start revm refuses on its caller's
    ///    account ([`caller_refuses_start`]) makes no write and journals no transfer log, so it
    ///    gets an empty lane and nothing is counted;
    /// 6. revm builds the frame, or answers it;
    /// 7. the state gas the caller was charged upfront for the frame's start is held to the
    ///    state-gas limit, unless revm refused the frame and so gives it back
    ///    ([`hold_upfront_state_gas`]).
    ///
    /// A frame answered at step 3, 4 or 6 — a `keylessDeploy` call the rewrite refused, an
    /// interceptor's answer, a precompile's, revm's for a call it did not start — is held to the
    /// compute limit before step 7, as a frame that ran would be ([`settle_answer`]). A precompile,
    /// which revm runs at step 6, is run on the gas the compute limit leaves the frame rather than
    /// on all its caller forwarded ([`hold_precompile`]).
    ///
    /// Steps 1 and 2 are the pre-frame check: they answer a frame nothing may start. The rewrite
    /// comes before interception, so a keyless deployment's creation is an ordinary creation from
    /// there on; it pushes the lane of the call it rewrites itself, and the creation's is pushed at
    /// step 5 as its child's. The frame's own writes are counted after the interceptor, because an
    /// intercepted frame's writes are the interceptor's to count.
    ///
    /// A frame answered before revm builds it gets an empty lane, so the lanes stay aligned with
    /// the results [`frame_return_result`](EvmTr::frame_return_result) pops. A creation answered
    /// with a stop still bumps its creator's nonce, as one that starts and reverts does.
    ///
    /// Before any of it, the state gas the caller holds is noted: the state-gas limit counts it
    /// as held outside the frame, with its own entry pushed beside the frame's lane, and adds to
    /// it what the frame charges. An interceptor's answer is held as revm's own is at step 7.
    #[inline]
    fn frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        if let Some(result) = self.answered_before_building(&frame_init)? {
            return Ok(ItemOrResult::Result(result));
        }
        let answer = match keyless::rewrite(&mut self.inner.ctx, &mut frame_init)? {
            Rewrite::Answered(answer) => Some(answer),
            Rewrite::Rewritten | Rewrite::NotKeyless => self.intercept(&frame_init),
        };
        // The frame that starts, as its caller forwarded it: the creation, when the rewrite made
        // one of a `keylessDeploy` call, and the call itself when the rewrite answered it.
        let (depth, gas_limit) = (frame_init.depth, input_gas_limit(&frame_init.frame_input));
        if let Some(mut result) = answer {
            self.inner.ctx.additional_limit.push_empty_frame();
            settle_answer(&mut self.inner.ctx, depth, gas_limit, &mut result);
            hold_upfront_state_gas(&mut self.inner.ctx, Some(&mut result));
            return Ok(ItemOrResult::Result(result));
        }
        let ctx = &mut self.inner.ctx;
        let refused = caller_refuses_start(ctx, &frame_init.frame_input);
        if refused {
            ctx.additional_limit.push_empty_frame();
        } else {
            let check =
                ctx.additional_limit.on_frame_init(&frame_init.frame_input, frame_init.depth);
            if check.exceeded_limit() {
                return Ok(ItemOrResult::Result(stop_before_building(ctx, &frame_init, &check)?));
            }
        }
        // The creator of a creation, to tell afterwards whether revm bumped its nonce: a creation
        // revm answers without the bump made nothing its start counted.
        let creator = match &frame_init.frame_input {
            FrameInput::Create(inputs) => {
                Some((inputs.caller(), account_nonce(ctx, inputs.caller())))
            }
            _ => None,
        };
        #[cfg(debug_assertions)]
        let counted = (
            ctx.additional_limit.frame_start_transfer_log(&frame_init.frame_input),
            refused,
            ctx.journal_ref().logs().len(),
        );
        let withheld = hold_precompile(ctx, &self.inner.precompiles, &mut frame_init);
        let outcome = match self.inner.frame_init(frame_init)? {
            ItemOrResult::Item(frame) => Ok(frame.interpreter.input.target_address),
            ItemOrResult::Result(result) => Err(result),
        };
        let ctx = &mut self.inner.ctx;
        #[cfg(debug_assertions)]
        assert_start_as_counted(ctx, counted, outcome.as_ref().err());
        match outcome {
            Ok(address) => {
                ctx.additional_limit.set_frame_address(address);
                hold_upfront_state_gas(ctx, None);
                Ok(ItemOrResult::Item(self.inner.frame_stack.get()))
            }
            Err(mut result) => {
                if let Some((creator, nonce)) = creator {
                    if account_nonce(ctx, creator) == nonce {
                        ctx.additional_limit.creation_did_not_bump_nonce();
                    }
                }
                if let Some(withheld) = withheld {
                    Detention::restore_forward(result.interpreter_result_mut(), withheld);
                }
                settle_answer(ctx, depth, gas_limit, &mut result);
                hold_upfront_state_gas(ctx, Some(&mut result));
                Ok(ItemOrResult::Result(result))
            }
        }
    }

    /// Runs the frame on top of the stack, unless it has a stop to return: the latched one, or
    /// its own when a failed creation put it over its budget. Then the frame returns the stop
    /// without running another instruction (see [`before_frame_run`]).
    ///
    /// Gas detention holds the frame to the compute limit before it runs, and settles the frame
    /// once it suspends on a child or returns (see [`after_frame_run`]).
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
        // Before `return_create` commits a successful creation. See `on_create_return`.
        let action = meter_deployed_code(ctx, frame, action);
        let mut next = frame.process_next_action(ctx, action);
        after_frame_run(ctx, frame, &mut next);
        // No frame runs an instruction once the transaction is latched, and every site that
        // latches rewrites its own frame's action to the stop, so no frame returns a halt under
        // the latch. Checked on this path alone: on the inspected one an inspector's step hooks
        // can rewrite a running frame's action, and the latch writes over that halt instead.
        debug_assert!(
            !matches!(&next, Ok(ItemOrResult::Result(result))
                if result.instruction_result().is_halt() &&
                    ctx.additional_limit.latched().is_some()),
            "a frame returned a halt under the latch"
        );
        next.inspect(|next| {
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
        let refund = self.inner.ctx.additional_limit.on_frame_return(&mut result);
        let returned = self.inner.frame_return_result(result)?;
        // The history of the records the caller paid for and the frame did not keep, given back
        // after the merge that adopted the frame's pools. `Some` means the outermost frame
        // returned: its caller, when it has one, is the `keylessDeploy` call that started it,
        // which is settled once the transaction's frames are done.
        if returned.is_none() {
            self.inner.frame_stack.get().interpreter.gas.refill_history(refund);
        } else {
            keyless::give_back_history(&mut self.inner.ctx, refund);
        }
        Ok(returned)
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
    /// itself never reaches [`EvmTr::frame_init`], so an empty lane stands in for it, and its
    /// answer is held to the state-gas limit as revm's own is ([`hold_upfront_state_gas`]).
    ///
    /// A keyless deployment is seen as the frames it is made of. The inspector is told the
    /// transaction's `keylessDeploy` call starts; the keyless rewrite then turns it into its
    /// creation, and the inspector is told the creation starts, as the call's child, one journal
    /// depth below it. Every step of the init code follows, then the creation's end, and the
    /// call's end once the creation is settled into it
    /// ([`inspect_execution`](InspectorHandler::inspect_execution)). A `keylessDeploy` call the
    /// rewrite answers is seen as the call it is, `call` and `call_end` paired around the answer.
    ///
    /// A frame nothing may start — a latched transaction's, one past the call-stack limit — is
    /// answered after the inspector's `frame_start` and before the keyless rewrite, at the point
    /// the plain path answers it ([`EvmTr::frame_init`]), and the inspector is told it ended.
    #[inline]
    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector) = self.ctx_inspector();
        if let Some(output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            return answered_by_inspector(ctx, inspector, &frame_init, output)
                .map(ItemOrResult::Result);
        }
        if let Some(mut output) = self.answered_before_building(&frame_init)? {
            let (ctx, inspector) = self.ctx_inspector();
            frame_end_checked(ctx, inspector, &frame_init.frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }
        let (ctx, inspector) = self.ctx_inspector();
        match keyless::rewrite(ctx, &mut frame_init)? {
            Rewrite::NotKeyless => {}
            Rewrite::Answered(output) => {
                return Ok(ItemOrResult::Result(answered_without_running(
                    ctx,
                    inspector,
                    &frame_init,
                    output,
                )));
            }
            Rewrite::Rewritten => {
                if let Some(output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
                    return answered_by_inspector(ctx, inspector, &frame_init, output)
                        .map(ItemOrResult::Result);
                }
            }
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

    /// revm's inspected frame run, with the stop short-circuit of [`EvmTr::frame_run`]: a frame
    /// with a stop to return returns it without a step, and the inspector sees it end.
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
        // The inspected path commits a creation through the same `return_create`.
        let action = meter_deployed_code(ctx, frame, action);
        let mut next = frame.process_next_action(ctx, action);
        after_frame_run(ctx, frame, &mut next);
        if let Ok(ItemOrResult::Result(result)) = &mut next {
            frame_end_checked(ctx, inspector, &frame.input, result);
            frame.set_finished(true);
        }
        next
    }
}

/// Holds the bytecode a creation is about to deposit to the limits, and turns that return into
/// the stop when it crosses one, before revm commits the creation: first the state gas
/// `return_create` will charge for the bytes, then the bytes themselves.
///
/// Only a deposit `return_create` would make its state charge for is held
/// ([`deposit_state_gas`]). A creation that fails before that charge fails there, alone, and the
/// chain keeps none of its code: counted, those bytes and their state gas could cross the
/// transaction's limit and stop every frame above a creation that fails by itself.
fn meter_deployed_code<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &EthFrame<EthInterpreter>,
    mut action: InterpreterAction,
) -> InterpreterAction {
    if frame.data.is_create() {
        if let InterpreterAction::Return(result) = &mut action {
            let address = frame.interpreter.input.target_address;
            if let Some(state_gas) = deposit_state_gas(ctx, address, result) {
                hold_deposit_state_gas(ctx, result, state_gas);
                ctx.additional_limit.on_create_return(result);
            }
        }
    }
    action
}

/// Holds the `state_gas` `return_create` is about to charge for the code a creation deposits to
/// the state-gas limit, with what the creation already holds, and turns the return into the stop
/// when it crosses it.
///
/// A crossing after `return_create` would leave the code deployed: the charge is made inside it,
/// after which it commits the creation's checkpoint. So the limit binds here, just before the
/// charge, and only once [`deposit_state_gas`] found that `return_create` will make it: a charge
/// the creation cannot pay is an out-of-gas whatever the limit, as it is at every other site.
fn hold_deposit_state_gas<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    result: &mut InterpreterResult,
    state_gas: u64,
) {
    if state_gas == 0 {
        return;
    }
    let running = result.gas.state_gas_spent().saturating_add_unsigned(state_gas);
    let check = ctx.additional_limit.check_state_gas(running);
    if check.exceeded_limit() {
        result.result = InstructionResult::Revert;
        result.output = check.revert_data();
    }
}

/// The state gas `return_create` charges for the code a creation at `address` returns, when it
/// reaches that charge and the creation can pay it — zero when it charges none; `None` when it
/// fails the creation at that charge or before it.
///
/// It retraces revm's `return_create` (`crates/handler/src/frame.rs`), step by step and on a copy
/// of the frame's gas, up to and including the state charge:
///
/// 1. a return that is not a success deposits nothing: `if !interpreter_result.result.is_ok()`;
/// 2. code over the code-size limit fails the creation: `interpreter_result.output.len() >
///    max_code_size`;
/// 3. so does code starting with `0xEF`, unless EIP-3541 is off: `!is_eip3541_disabled &&
///    interpreter_result.output.first() == Some(&0xEF)`;
/// 4. the regular deposit cost, `gas_params.code_deposit_cost(len)`, is charged, and a frame that
///    cannot pay it runs out of gas;
/// 5. under EIP-8037, so is the regular cost of hashing the code, `gas_params.keccak256_cost(len)`;
/// 6. under EIP-8037, when the schedule prices deposited code (`code_deposit_state_gas(len) > 0`),
///    the state gas is priced through the hook — a lookup that fails fails the creation — and
///    recorded as `record_state_cost` records it, the reservoir first and then regular gas, and a
///    frame that cannot pay it runs out of gas.
///
/// Only a creation that passes all six is held, so every out-of-gas `return_create` would report
/// is still reported, and the hook is asked for a price only where `return_create` asks it: a
/// lookup that fails here fails there the same way. What `return_create` charges after the state
/// gas — the deposited code's history — is a charge like the history after any other state
/// charge, which the limit holds before.
///
/// `return_create` also gates steps 2, 3 and 4's out-of-gas on EIP-170, London and Homestead,
/// which Satin's base spec, Osaka, enables.
fn deposit_state_gas<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    address: Address,
    result: &InterpreterResult,
) -> Option<u64> {
    let code = &result.output;
    let len = code.len();
    let cfg = ctx.cfg();
    if !result.result.is_ok() ||
        len > cfg.max_code_size() ||
        (!cfg.is_eip3541_disabled() && code.first() == Some(&0xEF))
    {
        return None;
    }
    let mut gas = result.gas;
    let gas_params = cfg.gas_params();
    if !gas.record_regular_cost(gas_params.code_deposit_cost(len)) {
        return None;
    }
    if !cfg.is_amsterdam_eip8037_enabled() {
        return Some(0);
    }
    if !gas.record_regular_cost(gas_params.keccak256_cost(len)) {
        return None;
    }
    if gas_params.code_deposit_state_gas(len) == 0 {
        return Some(0);
    }
    let charge = StateGasCharge::units(
        GasId::code_deposit_state_gas(),
        StateGasSite::account(address),
        len as u64,
    );
    let cost = ctx.state_gas_charge(charge)?;
    gas.record_state_cost(cost).then_some(cost)
}

/// The action of a frame about to run: the stop it returns without running an instruction, when
/// it has one ([`stop_before_run`](crate::AdditionalLimit::stop_before_run)); `None` otherwise,
/// and the frame runs.
///
/// A frame runs here for the first time or after a child returned into it. A stop is always the
/// latter. Under a latch, the child that crossed the limit reverted, and its caller must not
/// resume. Without one, the child was a failed creation whose nonce record put its creator over
/// its budget, and the creator reverts alone.
///
/// Either way gas detention sees the frame first: a frame's first run adds its caller's compute
/// to the transaction's, a resume takes it back, and the frame's spendable gas is held to what the
/// compute limit leaves it — which is how a child's read of volatile data caps every caller it
/// returns into.
#[inline]
fn before_frame_run<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &mut EthFrame<EthInterpreter>,
) -> Option<InterpreterAction> {
    ctx.detention.on_frame_run(&mut frame.interpreter.gas, frame.depth);
    let stop = ctx.additional_limit.stop_before_run()?;
    Some(InterpreterAction::new_return(
        InstructionResult::Revert,
        stop.revert_data(),
        frame.interpreter.gas,
    ))
}

/// Settles gas detention once the frame ran: a frame that suspends on a child keeps its compute
/// for the child's start to add to the transaction's; a frame that returns is classified.
///
/// The frame's result is read here, after revm processed its last action — `return_create`
/// included, whose deposit and hash charges are the creating frame's own compute — and before
/// revm hands it to the caller or, for the transaction's own frame, to
/// [`last_frame_result`](Handler::last_frame_result), which overwrites its gas with the
/// transaction's.
///
/// A frame that ran out of gas on a charge the withheld part of its gas would have paid crossed
/// the compute limit rather than its own gas: its halt becomes the transaction-level stop
/// ([`stop_at_the_compute_limit`]). Every other out-of-gas halts, as it would without the read.
#[inline]
fn after_frame_run<DB: Database, ExtEnvs: ExternalEnvTypes, E>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &EthFrame<EthInterpreter>,
    next: &mut Result<FrameInitOrResult<EthFrame<EthInterpreter>>, E>,
) {
    match next {
        Ok(ItemOrResult::Item(_)) => {
            ctx.detention.on_frame_suspend(&frame.interpreter.gas, frame.depth);
        }
        Ok(ItemOrResult::Result(result)) => {
            let instruction_result = result.instruction_result();
            if let Some(limit) =
                ctx.detention.on_frame_end(instruction_result, result.gas_mut(), frame.depth)
            {
                stop_at_the_compute_limit(ctx, result.interpreter_result_mut(), limit);
            }
        }
        Err(_) => {}
    }
}

/// Holds a frame answered without running to the compute limit: the answer to a frame of
/// `gas_limit` at `depth`, which its caller forwarded.
///
/// An interceptor builds its answer on the whole gas the caller forwarded, the part gas detention
/// withholds from the caller's regular charges included. An answer that spent more than the frame
/// could have run on is answered out of gas and marked as a crossing, and becomes the stop as a
/// frame that ran would ([`Detention::on_answer`]); so does a precompile that ran out of the
/// allowance it was run on ([`hold_precompile`]). An answer that halts otherwise burns what it was
/// given.
fn settle_answer<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    depth: usize,
    gas_limit: u64,
    answer: &mut FrameResult,
) {
    let answer = answer.interpreter_result_mut();
    if let Some(limit) = ctx.detention.on_answer(answer, depth, gas_limit) {
        stop_at_the_compute_limit(ctx, answer, limit);
    }
}

/// Holds a precompile the frame `frame_init` is about to call to what gas detention's limit leaves
/// it, and returns what it took off the forward: revm runs the precompile inside the frame's start,
/// against its gas limit, so a precompile forwarded more than the allowance would otherwise
/// compute past the limit before its answer could be classified.
///
/// The precompile runs on the allowance and sees it as its gas limit; its answer gets the rest
/// back ([`Detention::restore_forward`]) before it is settled. A call to anything else, and a
/// forward within the allowance, run as they are.
fn hold_precompile<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    precompiles: &PrecompilesMap,
    frame_init: &mut FrameInit,
) -> Option<NonZeroU64> {
    let FrameInput::Call(inputs) = &mut frame_init.frame_input else { return None };
    let allowance = ctx.detention.allowance(frame_init.depth, inputs.gas_limit)?;
    let withheld = NonZeroU64::new(inputs.gas_limit.saturating_sub(allowance))?;
    precompiles.get(&inputs.bytecode_address)?;
    inputs.gas_limit = allowance;
    Some(withheld)
}

/// The gas limit of the frame `input` starts.
const fn input_gas_limit(input: &FrameInput) -> u64 {
    match input {
        FrameInput::Call(inputs) => inputs.gas_limit,
        FrameInput::Create(inputs) => inputs.gas_limit(),
        FrameInput::Empty => 0,
    }
}

/// Turns a frame that crossed gas detention's compute `limit` into the transaction-level stop: a
/// revert carrying `MegaLimitExceeded` (kind: compute), with the transaction latched, so no caller
/// resumes. The frame's gas is what detention left it — the withheld part at the crossing — which
/// goes back with the revert, to the caller and in the end to the sender.
///
/// The crossing charge's size is not kept, so the stop reports the limit as what was used
/// ([`LimitCheck::ExceedsLimit`]): the transaction's compute reached it exactly, the spendable gas
/// the frame had counting as spent.
fn stop_at_the_compute_limit<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    result: &mut InterpreterResult,
    limit: u64,
) {
    let stop = ctx.additional_limit.latch(LimitKind::ComputeGas, limit, limit);
    result.result = InstructionResult::Revert;
    result.output = stop.revert_data();
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
    /// The scheme guard is here: `CALLCODE` and `DELEGATECALL` run the callee's code in the
    /// caller's context, where a system contract's semantics would apply to the wrong account,
    /// so they never reach an interceptor and revm builds their frame as it does for any other
    /// contract. A creation reaches no interceptor either.
    ///
    /// An answer is a [`synthetic_frame_result`](crate::synthetic_frame_result), so it settles
    /// like a frame revm ran. What each contract answers is in the `system` module.
    #[inline]
    fn intercept(&mut self, frame_init: &FrameInit) -> Option<FrameResult> {
        let FrameInput::Call(inputs) = &frame_init.frame_input else { return None };
        if !matches!(inputs.scheme, CallScheme::Call | CallScheme::StaticCall) {
            return None;
        }
        // The calling frame is on top of the stack, suspended on this call; the transaction's own
        // call has none.
        let caller_remaining = match self.inner.frame_stack.index() {
            Some(_) => self.inner.frame_stack.get().interpreter.gas.remaining(),
            None => 0,
        };
        crate::system::intercept(&mut self.inner.ctx, inputs, frame_init.depth, caller_remaining)
    }

    /// Steps 1 and 2 of [`EvmTr::frame_init`], the pre-frame check: answers a frame nothing may
    /// start ([`answer_before_building`]), with the empty lane that stands in for it. Both paths
    /// make it before the keyless rewrite, so the rewrite never sees a latched transaction; on the
    /// inspected path it comes after the inspector's `frame_start`, and the check `frame_init`
    /// makes again finds nothing to answer.
    ///
    /// Before it, the state gas the caller holds is noted: it is held outside the frame it starts.
    /// The caller is the frame on top of the stack, suspended on this frame's input; the
    /// transaction's own frame has none, and starts on what was charged before it.
    #[inline]
    fn answered_before_building(
        &mut self,
        frame_init: &FrameInit,
    ) -> Result<Option<FrameResult>, ContextDbError<MegaContext<DB, ExtEnvs>>> {
        if self.inner.frame_stack.index().is_some() {
            let held = self.inner.frame_stack.get().interpreter.gas.state_gas_spent();
            self.inner.ctx.additional_limit.note_caller_state_gas(held);
        }
        let answer = answer_before_building(&mut self.inner.ctx, frame_init)?;
        if answer.is_some() {
            self.inner.ctx.additional_limit.push_empty_frame();
        }
        Ok(answer)
    }
}

/// Holds the state gas the caller was charged upfront for the frame that is starting — the new
/// account a value `CALL` adds, a creation's account — to the state-gas limit, once revm has
/// decided the frame. `answer` is the frame's result when it was answered without running; a
/// frame revm built has none yet.
///
/// revm's `CALL`, `CREATE` and `CREATE2` make that charge before anything knows whether the frame
/// can start, and a frame that adds no account gives it back when its answer returns
/// ([`FrameResult::refundable_state_gas_charge`]): a value call its caller cannot fund, a call past
/// the call-stack limit, an answer that fails. Such a charge is never held. A charge that stands —
/// the frame is built, or answered with a success, as a value call to an account with no code is
/// — is held, and a crossing latches the transaction: a built frame returns the stop before its
/// first instruction, and an answer is rewritten to it here, so an inspector sees the answer the
/// caller gets. The writes the frame's start made go with the frames the stop reverts. A built
/// frame that later fails gives the charge back too, but by then it has started, and a crossing
/// inside a frame that later fails is a crossing.
///
/// The frame's own entry is on top by now, and holds what its caller held, the charge included;
/// the frame itself holds nothing yet. Every other charge was held where it was made, so only the
/// upfront one can cross here.
fn hold_upfront_state_gas<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    answer: Option<&mut FrameResult>,
) {
    let Some(answer) = answer else {
        ctx.additional_limit.check_state_gas(0);
        return;
    };
    if answer.refundable_state_gas_charge().is_none() &&
        ctx.additional_limit.check_state_gas(0).exceeded_limit()
    {
        ctx.additional_limit.apply_latch(answer);
    }
}

/// revm's unwinding of a runtime phase that ran out of gas before the first frame.
///
/// Called through this function rather than directly: the inspector handler's bounds keep the
/// compiler from reading the journal's database error as `DB::Error`, and here nothing does.
fn unwind_runtime_oog<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    checkpoint: JournalCheckpoint,
) -> Result<(), DB::Error> {
    runtime_oog_unwind(ctx, checkpoint)
}

/// Settles `output`, the answer the inspector gave the frame `frame_init` starts in its place.
/// The latch and the depth guard still hold: an answer cannot start a frame of a stopped
/// transaction, nor reach past the call-stack limit.
fn answered_by_inspector<DB: Database, ExtEnvs: ExternalEnvTypes, INSP>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    frame_init: &FrameInit,
    mut output: FrameResult,
) -> Result<FrameResult, ContextDbError<MegaContext<DB, ExtEnvs>>>
where
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
{
    if let Some(answer) = answer_before_building(ctx, frame_init)? {
        output = answer;
    }
    Ok(answered_without_running(ctx, inspector, frame_init, output))
}

/// Settles `output`, the answer the frame `frame_init` starts gets without running on the
/// inspected path — an inspector's, or a `keylessDeploy` call's the rewrite refused — as
/// [`EvmTr::frame_init`] settles an answer: an empty lane stands in for the frame, and the answer
/// is held to the compute limit ([`settle_answer`]) and to the state-gas limit
/// ([`hold_upfront_state_gas`]). Then the inspector is told the frame ended.
fn answered_without_running<DB: Database, ExtEnvs: ExternalEnvTypes, INSP>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    frame_init: &FrameInit,
    mut output: FrameResult,
) -> FrameResult
where
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
{
    ctx.additional_limit.push_empty_frame();
    let gas_limit = input_gas_limit(&frame_init.frame_input);
    settle_answer(ctx, frame_init.depth, gas_limit, &mut output);
    hold_upfront_state_gas(ctx, Some(&mut output));
    frame_end_checked(ctx, inspector, &frame_init.frame_input, &mut output);
    output
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

/// Asserts that revm started a frame the way the data size counted its start, once revm has built
/// the frame or answered it: that revm refused exactly the start [`caller_refuses_start`]
/// predicted, and journaled exactly the EIP-7708 transfer log counted. `counted` is whether the
/// start counts a transfer log when revm makes it
/// ([`frame_start_transfer_log`](crate::AdditionalLimit)), `refused` whether it was predicted
/// refused and so counted nothing, `logs_i` the journal's log count before revm acted, and
/// `answer` revm's answer when it did not build the frame.
///
/// The count is made before revm decides, from the frame's input and its caller's account, so this
/// is where a divergence from revm's own rules would show. revm refuses a start on its caller's
/// account with `OutOfFunds`, for a value the caller cannot fund, and with a `Return` for a
/// creation whose creator's nonce cannot be bumped — the one creation it answers with a success.
/// A built frame has run no instruction yet, so the move is all revm has done. An answered call
/// moved the value when it succeeded — a call to an account with no code, a precompile — and took
/// the move back when it failed. A creation revm answers never moved value: it refused before its
/// checkpoint, or reverted it (a creation onto an occupied address). Only transfer logs are
/// compared: a precompile may journal logs of its own.
#[cfg(debug_assertions)]
fn assert_start_as_counted<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    (counted, refused, logs_i): (bool, bool, usize),
    answer: Option<&FrameResult>,
) {
    let refused_by_revm = match answer {
        None => false,
        Some(FrameResult::Call(outcome)) => outcome.result.result == InstructionResult::OutOfFunds,
        Some(FrameResult::Create(outcome)) => matches!(
            outcome.result.result,
            InstructionResult::OutOfFunds | InstructionResult::Return
        ),
    };
    debug_assert_eq!(refused, refused_by_revm, "the start revm refused is the one predicted");
    let moved = match answer {
        None => true,
        Some(FrameResult::Call(outcome)) => outcome.result.result.is_ok(),
        Some(FrameResult::Create(_)) => false,
    };
    let journaled = ctx.journal_ref().logs()[logs_i..]
        .iter()
        .filter(|log| log.address == revm::primitives::eip7708::ETH_TRANSFER_LOG_ADDRESS)
        .count();
    debug_assert_eq!(
        journaled,
        usize::from(counted && moved),
        "the transfer log counted is the one revm journaled"
    );
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

/// The first frame of a latched transaction: the transaction's call or creation on the gas it
/// has left, for the frame the latch answers with the stop.
///
/// Nothing is loaded and nothing is charged. The frame is never built, so its code is never read.
fn unbuilt_first_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    gas: &GasTracker,
) -> FrameInit {
    let tx = ctx.tx();
    let frame_input = match tx.kind() {
        TxKind::Call(target_address) => FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(tx.input().clone()),
            return_memory_offset: 0..0,
            gas_limit: gas.remaining(),
            reservoir: gas.reservoir(),
            bytecode_address: target_address,
            known_bytecode: Default::default(),
            target_address,
            caller: tx.caller(),
            value: CallValue::Transfer(tx.value()),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        })),
        TxKind::Create => FrameInput::Create(Box::new(CreateInputs::new(
            tx.caller(),
            CreateScheme::Create,
            tx.value(),
            tx.input().clone(),
            gas.remaining(),
            gas.reservoir(),
        ))),
    };
    FrameInit { depth: 0, memory: SharedMemory::new(), frame_input }
}

/// Whether executing this transaction creates its caller's account: a deposit-like transaction
/// whose caller is empty, which the deposit path materialises by bumping its nonce or by minting
/// to it.
///
/// Read before op-revm deducts the caller, which is what materialises the account, and after
/// [`validate_env`](Handler::validate_env), which promoted a system-address transaction: a
/// deposit-like transaction is a deposit by then. Every other transaction pays a fee its caller
/// must hold, so it has an account already.
///
/// The read does not warm the account: the transaction's own touches of it pay what they would
/// have paid without the check.
fn deposit_creates_caller<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) -> Result<bool, DB::Error> {
    if ctx.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE {
        return Ok(false);
    }
    let caller = ctx.tx().caller();
    Ok(ctx.journal_mut().inspect_account(caller, false)?.info.is_empty())
}

/// Charges the state gas of the account a deposit-like transaction creates for its caller, as
/// EIP-2780 charges the account a value transfer creates for its recipient. `false` when the
/// transaction cannot pay it, which is an out-of-gas before it runs.
fn charge_created_caller<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    gas: &mut GasTracker,
) -> bool {
    let caller = ctx.tx().caller();
    let charge = StateGasCharge::one(GasId::new_account_state_gas(), StateGasSite::account(caller));
    let Some(state_gas) = ctx.state_gas_charge(charge) else { return false };
    gas.record_state_cost(state_gas)
}

/// Whether revm refuses to start the frame `input` asks for on what its caller's account holds:
/// a value the caller cannot fund, which revm answers with `OutOfFunds`, and a creation whose
/// creator's nonce cannot be bumped, which it answers with a `Return` and no address. revm checks
/// both before it moves or writes anything, so a start it refuses makes no write record and
/// journals no transfer log: it is charged nothing and counted nothing.
///
/// A creation onto an occupied address is the one refusal not predicted. revm decides it on the
/// created address's account after the start is counted, and reading that account here would load
/// it, changing what the transaction has warmed. So the creation's records and transfer log are
/// counted, and a crossing they cause stops the creation before revm could refuse it; without a
/// crossing, revm's refusal fails the creation and its lane goes with it.
pub(crate) fn caller_refuses_start<DB: revm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    input: &FrameInput,
) -> bool {
    match input {
        FrameInput::Call(inputs) => {
            inputs.transfers_value() &&
                caller_refuses(ctx, inputs.caller, inputs.call_value(), false)
        }
        FrameInput::Create(inputs) => caller_refuses(ctx, inputs.caller(), inputs.value(), true),
        FrameInput::Empty => false,
    }
}

/// Whether `caller`'s account refuses a frame start that moves `value` and, for a `creation`,
/// bumps its nonce: the balance revm's transfer checks, and the nonce revm's creation bumps.
///
/// The account is read from the journal as it stands, without loading or warming anything, and it
/// is always there. A transaction's sender is loaded by validation, which also credits a deposit's
/// mint (`validate_against_state_and_deduct_caller`, revm's and op-revm's). Any other caller is the
/// account a running frame runs as, which revm loaded to start that frame: a call's target before
/// its value moves, a creation's address when it creates the account.
fn caller_refuses<DB: revm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    caller: Address,
    value: U256,
    creation: bool,
) -> bool {
    let account = ctx.journal_ref().state.get(&caller);
    debug_assert!(account.is_some(), "a frame's caller is loaded before the frame starts");
    account.is_some_and(|account| {
        account.info.balance < value || (creation && account.info.nonce == u64::MAX)
    })
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
///
/// `state_gas` is what the transaction has been charged before its first frame, the authorities'
/// new accounts and delegations included, which the state-gas limit holds before the records are
/// counted.
fn record_applied_authorities<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    journal_i: usize,
    state_gas: i64,
) -> AppliedAuthorities {
    if ctx.tx().tx_type() != TransactionType::Eip7702 {
        return AppliedAuthorities { check: LimitCheck::WithinLimit, applied: 0 };
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
    let applied = authorities.len() as u64;
    let check = ctx.additional_limit.record_applied_authorities(
        caller,
        applied,
        target_is_authority,
        state_gas,
    );
    if check.exceeded_limit() {
        return AppliedAuthorities { check, applied: 0 };
    }
    // An applied authority that is the block beneficiary wrote the beneficiary's account.
    if authorities.binary_search(&ctx.block().beneficiary).is_ok() {
        ctx.detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
    }
    AppliedAuthorities { check, applied }
}

/// What [`record_applied_authorities`] found: the verdict of the limit check, and how many
/// authorities were applied — none, when the check stopped them.
struct AppliedAuthorities {
    check: LimitCheck,
    applied: u64,
}

/// Charges the history of the write records a transaction makes outside any frame: one per
/// applied authority, and the one its own frame makes — the recipient of its value, or the
/// account it creates.
///
/// The authorities' records are the transaction's own and outlive a first frame that fails; the
/// first frame's record does not, so what it cost is kept for the settlement to give back. A first
/// frame revm refuses to start on its sender's account makes no record, and is charged none
/// ([`caller_refuses_start`]): among the transactions that pay history, that is a creation from an
/// account whose nonce cannot be bumped, which only a caller that turned the nonce check off can
/// run, and which revm answers with a success.
/// `false` when the transaction cannot pay, which is an out-of-gas before it runs.
fn charge_records_made_outside_a_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    gas: &mut GasTracker,
    applied_authorities: u64,
) -> bool {
    if !ctx.prices_history() {
        return true;
    }
    let tx = ctx.tx();
    let writes_a_record = match tx.kind() {
        TxKind::Create => true,
        TxKind::Call(to) => {
            !tx.value().is_zero() &&
                to != tx.caller() &&
                !ctx.additional_limit.target_is_authority()
        }
    };
    let top_level_record =
        writes_a_record && !caller_refuses(ctx, tx.caller(), tx.value(), tx.kind().is_create());
    let Some(top_level) = write_record_history_gas(u64::from(top_level_record)) else {
        return false;
    };
    let Some(authorities) = write_record_history_gas(applied_authorities) else { return false };
    let Some(cost) = top_level.checked_add(authorities) else { return false };
    if !gas.record_history_cost(cost) {
        return false;
    }
    ctx.additional_limit.set_top_level_write_record_gas(top_level);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::MemoryDatabase, EvmTxRuntimeLimits, LimitKind, MegaLimitExceeded, MegaSpecId,
    };
    use alloy_primitives::{address, U256};
    use alloy_sol_types::SolError;

    const CALLER: Address = address!("00000000000000000000000000000000000e0001");
    const TARGET: Address = address!("00000000000000000000000000000000000e0002");

    /// A `CALL` from `caller` to `target` of `value` wei, which its opcode charged a new account
    /// for when `charged`.
    fn call(caller: Address, target: Address, value: u64, charged: bool) -> FrameInput {
        FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            reservoir: 0,
            bytecode_address: target,
            known_bytecode: Default::default(),
            target_address: target,
            caller,
            value: CallValue::Transfer(U256::from(value)),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: charged,
        }))
    }

    /// A `CALL` of one wei to `TARGET`, which its opcode charged a new account for.
    fn value_call() -> FrameInput {
        call(CALLER, TARGET, 1, true)
    }

    /// A `CREATE` its opcode charged the created account for.
    fn creation() -> FrameInput {
        let mut inputs =
            CreateInputs::new(CALLER, CreateScheme::Create, U256::ZERO, Bytes::new(), 100_000, 0);
        inputs.set_charged_create_state_gas(true);
        inputs.set_charged_state_gas_address(TARGET);
        FrameInput::Create(Box::new(inputs))
    }

    /// A context under a state-gas limit of 99, with the transaction's own frame on the call stack
    /// holding 100 — the upfront charge of the frame it starts — and the entry of that frame
    /// pushed.
    fn starting_a_frame_charged_100() -> MegaContext<MemoryDatabase> {
        let mut ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN)
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(99));
        let _ = ctx.additional_limit.on_frame_init(&call(CALLER, CALLER, 0, false), 0);
        ctx.additional_limit.note_caller_state_gas(100);
        ctx.additional_limit.push_empty_frame();
        ctx
    }

    /// A frame revm refuses — past the call-stack limit, which no transaction reaches under the
    /// execution cap — gives its caller's upfront charge back, so the charge is not held, for a
    /// call and a creation alike; the answer stays what it was.
    #[test]
    fn test_a_refused_frame_is_not_held_for_its_upfront_charge() {
        for input in [value_call(), creation()] {
            let mut ctx = starting_a_frame_charged_100();
            let mut refused =
                synthetic_frame_result(&input, InstructionResult::CallTooDeep, Bytes::new());
            assert!(refused.refundable_state_gas_charge().is_some(), "{input:?}");
            hold_upfront_state_gas(&mut ctx, Some(&mut refused));
            assert_eq!(ctx.additional_limit.latched(), None, "{input:?}");
            assert_eq!(refused.instruction_result(), InstructionResult::CallTooDeep);
        }
    }

    /// The guard trips on a transfer log the data size counted for a frame revm built and did not
    /// journal.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the transfer log counted is the one revm journaled")]
    fn test_a_transfer_log_counted_and_not_journaled_trips() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        assert_start_as_counted(&ctx, (true, false, 0), None);
    }

    /// The guard trips on a start revm refused on its caller's account that was not predicted,
    /// which the data size would have counted.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the start revm refused is the one predicted")]
    fn test_a_refusal_not_predicted_trips() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let unfunded =
            synthetic_frame_result(&value_call(), InstructionResult::OutOfFunds, Bytes::new());
        assert_start_as_counted(&ctx, (true, false, 0), Some(&unfunded));
    }

    /// The guard trips on a start predicted refused that revm made, which the data size did not
    /// count.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the start revm refused is the one predicted")]
    fn test_a_predicted_refusal_revm_did_not_make_trips() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        assert_start_as_counted(&ctx, (false, true, 0), None);
    }

    /// The guard expects no log where nothing moved, whatever the input would count: a start
    /// revm refuses on its caller's account, predicted — a value call it answers `OutOfFunds`, a
    /// creation whose creator's nonce cannot be bumped, answered with a `Return` — and one it
    /// fails otherwise — a creation onto an occupied address, a failing precompile; and none for
    /// a frame nothing was counted for.
    #[test]
    #[cfg(debug_assertions)]
    fn test_the_guard_expects_no_transfer_log_where_nothing_moved() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        assert_start_as_counted(&ctx, (false, false, 0), None);
        let overflow = synthetic_frame_result(&creation(), InstructionResult::Return, Bytes::new());
        assert_start_as_counted(&ctx, (true, true, 0), Some(&overflow));
        let unfunded =
            synthetic_frame_result(&value_call(), InstructionResult::OutOfFunds, Bytes::new());
        assert_start_as_counted(&ctx, (true, true, 0), Some(&unfunded));
        let collision =
            synthetic_frame_result(&creation(), InstructionResult::CreateCollision, Bytes::new());
        assert_start_as_counted(&ctx, (true, false, 0), Some(&collision));
        let failed =
            synthetic_frame_result(&value_call(), InstructionResult::PrecompileError, Bytes::new());
        assert_start_as_counted(&ctx, (true, false, 0), Some(&failed));
        let answered = synthetic_frame_result(&value_call(), InstructionResult::Stop, Bytes::new());
        assert_start_as_counted(&ctx, (false, false, 0), Some(&answered));
    }

    /// A charge that stands is held: a frame revm built latches the transaction, and a success
    /// answer carrying the charge latches it and is rewritten to the stop.
    #[test]
    fn test_a_charge_that_stands_is_held() {
        let stop = LimitCheck::ExceedsLimit {
            kind: LimitKind::StateGrowth,
            limit: 99,
            used: 100,
            frame_local: false,
        };
        let mut ctx = starting_a_frame_charged_100();
        hold_upfront_state_gas(&mut ctx, None);
        assert_eq!(ctx.additional_limit.latched(), Some(&stop), "a built frame");

        let mut ctx = starting_a_frame_charged_100();
        let mut answered =
            synthetic_frame_result(&value_call(), InstructionResult::Stop, Bytes::new());
        hold_upfront_state_gas(&mut ctx, Some(&mut answered));
        assert_eq!(ctx.additional_limit.latched(), Some(&stop), "a success answer");
        assert_eq!(answered.instruction_result(), InstructionResult::Revert);
        assert_eq!(
            answered.interpreter_result().output,
            Bytes::from(MegaLimitExceeded { kind: 3, limit: 99 }.abi_encode()),
        );
    }
}
