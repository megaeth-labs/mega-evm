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
use std::vec::Vec;

use alloy_evm::{precompiles::PrecompilesMap, Database};
use revm::{
    context::{
        result::{FromStringError, InvalidTransaction, ResultGas},
        transaction::TransactionType,
        ContextError, ContextTr, FrameStack, JournalTr, Transaction,
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
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallScheme, FrameInput,
        InitialAndFloorGas, InstructionResult, InterpreterAction, InterpreterResult,
    },
    primitives::{Address, Bytes, TxKind, CALL_STACK_LIMIT, U256},
    Inspector, Journal,
};

use crate::{
    access::ComputeStop,
    evm::inspector::{
        frame_end_checked, journal_position, revert_journal_to, ResultSource, StepGuard,
        UpfrontRecord,
    },
    history_gas, synthetic_frame_result,
    system::keyless,
    write_record_history_gas, Detention, ExternalEnvTypes, LimitCheck, LimitKind, MegaContext,
    MegaEvm, MegaInstructions, PricedPrecompiles, VolatileDataAccess, TX_BODY_OVER_DATA_SIZE_LIMIT,
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

impl<DB, EVM, ERROR, FRAME, ExtEnvs> MegaHandler<EVM, ERROR, FRAME>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    EVM: EvmTr<Context = MegaContext<DB, ExtEnvs>, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError> + FromStringError + IsTxError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    /// [`validate_env`](Handler::validate_env) once the live system address is known: decides
    /// whether the transaction is a system-address transaction — of the system shape, from
    /// `system_address` — and prepares the common execution layer for it, refuses a transaction
    /// from `system_address` without the system shape, validates a system-address transaction and
    /// promotes it to a deposit, then validates the transaction as op-revm does.
    ///
    /// The layer is prepared before the refusal, so a deposit the refusal turns into a failed
    /// deposit reports its body as data size, as a deposit that fails any other check does.
    ///
    /// The handler reads the address out of the state, and checks the system address's nonce and
    /// code. What validates a transaction without state
    /// ([`validate_transaction_stateless`](crate::validate_transaction_stateless)) is given the
    /// address and leaves the account to the state, as it leaves every sender's
    /// (`check_system_account` false); every other step is this one.
    pub(crate) fn validate_env_as(
        &self,
        evm: &mut EVM,
        system_address: Option<Address>,
        check_system_account: bool,
    ) -> Result<(), ERROR> {
        let ctx = evm.ctx_mut();
        let verdict = crate::system::classify_system_transaction(ctx.tx(), system_address);
        ctx.on_new_tx(verdict == Ok(true));
        if verdict? {
            crate::system::validate_and_promote::<_, _, ERROR>(ctx, check_system_account)?;
        }
        self.op.validate_env(evm)
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
    /// The two kinds of write record made outside any frame are charged their history here, where
    /// they are made: one per applied authority, and the one the transaction's own frame makes —
    /// the recipient of its value, or the account it creates. Neither is known at validation: an
    /// authority applies or does not, and whether the recipient is already written depends on the
    /// authorities that did. A transaction that cannot pay for them runs out of gas before its
    /// first frame, the way one that cannot pay its authorizations does.
    ///
    /// Validation refused every transaction whose body, with a record for each entry of its
    /// authorization list, crosses the data-size limit, so neither the body nor the authorities'
    /// records cross it here, and no limit stops the transaction before its first frame.
    fn pre_execution(
        &self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<PreExecutionOutput>, Self::Error> {
        self.load_accounts(evm)?;
        let checkpoint = evm.ctx().journal_mut().checkpoint();
        // A bail-out of the runtime gas phase unwinds as revm's does: the checkpoint is reverted
        // and a creation's sender gets the nonce bump the frame would have made, so an included
        // out-of-gas creation cannot be replayed. revm's own pre-execution never needs the bump,
        // because only an EIP-7702 transaction, always a call, can bail out there; the two
        // charges made here can fail for a creation too.
        if self.deposit_creates_caller.get() && !charge_created_caller(evm.ctx_mut(), gas) {
            runtime_oog_unwind(evm.ctx(), checkpoint)?;
            return Ok(None);
        }
        let Some(eip7702_refund) = self.apply_eip7702_auth_list(evm, gas)? else {
            runtime_oog_unwind(evm.ctx(), checkpoint)?;
            return Ok(None);
        };
        let applied = record_applied_authorities(evm.ctx_mut(), checkpoint.journal_i);
        if !charge_records_made_outside_a_frame(evm.ctx_mut(), gas, applied) {
            runtime_oog_unwind(evm.ctx(), checkpoint)?;
            return Ok(None);
        }
        Ok(Some(PreExecutionOutput { eip7702_refund, checkpoint }))
    }

    /// Reads the live system address out of the registry, then decides whether the transaction
    /// is a system-address transaction and prepares the common execution layer for it, then
    /// validates a system-address transaction and promotes it to a deposit, then validates the
    /// transaction as op-revm does — as a deposit, when it was promoted.
    ///
    /// This is the handler's first phase, so the layer is prepared before anything else of the
    /// transaction runs, and a read the database cannot serve fails the transaction through the
    /// handler's error path, which discards what the journal loaded. Every transaction reads the
    /// live system address, whatever its shape, and the read leaves the registry's account and
    /// its slot warm for the rest of the transaction, as EIP-3651 leaves the block beneficiary.
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        let system_address = crate::system::load_system_address(evm.ctx_mut().journal_mut())?;
        self.validate_env_as(evm, system_address, true)
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
    ///
    /// Before any of it, the body and a write record for each entry of the authorization list are
    /// held to the transaction's data-size limit, on the body count the common execution layer
    /// keeps, the one the history gas prices: a transaction over it is refused
    /// ([`TX_BODY_OVER_DATA_SIZE_LIMIT`]), whatever its gas, a deposit included, which op-revm
    /// then includes as a failed deposit. Those are the only bytes a transaction counts before its
    /// first frame, so one that passes cannot cross the limit there. A system-originated
    /// transaction is exempt from the limit, and from the check.
    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        let authorizations = evm.ctx_ref().tx().authorization_list_len() as u64;
        if !evm.ctx_ref().additional_limit.admits_tx_body(authorizations) {
            return Err(InvalidTransaction::Str(TX_BODY_OVER_DATA_SIZE_LIMIT.into()).into());
        }
        let mut gas = self.op.validate_initial_tx_gas(evm)?;
        if !evm.ctx_ref().prices_history() {
            return Ok(gas);
        }
        let bytes = evm.ctx_ref().additional_limit.body_bytes();
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

    /// revm's first frame, with a read of the block beneficiary's account marked for gas detention
    /// when the transaction's recipient delegates to the beneficiary: revm resolves that delegate
    /// through the journal, where the Host does not see it.
    fn first_frame_input(
        &mut self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<FrameInit>, Self::Error> {
        let Some(frame) = self.op.first_frame_input(evm, gas)? else {
            // The runtime gas phase could not pay for the first frame's start: EIP-2780's charge
            // for the account the frame adds, a call's recipient or a creation's account, or the
            // access to the code a delegated recipient runs, warm or cold. A price of that account
            // charge that could not be read ends here too: the lookup recorded its cause, which
            // fails the transaction when it settles. In each case revm drops the phase's partial
            // charges by rebuilding the transaction's gas from its intrinsic cost, and the history
            // charged for the first frame's write record went with them: the settlement must not
            // give it back a second time, into a reservoir a transaction below the execution cap
            // never had.
            evm.ctx_mut().additional_limit.set_top_level_write_record_gas(0);
            return Ok(None);
        };
        evm.ctx_mut().mark_beneficiary_delegate();
        Ok(Some(frame))
    }

    /// Settles the outermost frame: pops its lane, then settles its gas into the transaction's as
    /// op-revm does (op-revm replaces revm's settlement, so revm's never runs here): a transaction
    /// whose own frame a limit stopped settles like an EIP-8037 revert, its unspent regular gas
    /// and reservoir back to the sender.
    /// Keeps the history gas the transaction spent: what the body was charged, and what the frames
    /// charged net of what they gave back; and the history bytes it appended, which are what those
    /// charges were made for.
    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut FrameResult,
        parent_gas: &mut GasTracker,
    ) -> Result<(), Self::Error> {
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

    /// op-revm's error handling, which answers a deposit's transaction error with a failed-deposit
    /// halt and discards everything the deposit did. The halt is what the transaction reports;
    /// what the deposit counted stands, the body and the Oracle hints it forwarded before it
    /// halted, as for any transaction that halts. Any other error refuses the transaction, and the
    /// layer's state is reset by the next one.
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
    /// 1. the depth guard: a frame past the call-stack limit is answered with `CallTooDeep`, as
    ///    revm answers it, before anything could intercept it or count its start;
    /// 2. the keyless dispatch ([`keyless::is_dispatched`]): a `keylessDeploy` call a transaction
    ///    makes is answered when it carries value, and otherwise readied for revm to build as the
    ///    frame whose actions [`keyless::run`] makes ([`keyless::ready`]);
    /// 3. system contract interception ([`MegaEvm::intercept`]), for any other frame, which answers
    ///    the frame or lets it start; the caller of a frame that starts is recorded for gas
    ///    detention while no read has set a compute limit ([`record_caller`]);
    /// 4. the frame's lane is pushed and the writes its start makes are counted; a limit they cross
    ///    answers the frame with the stop before it runs. A start revm refuses on its caller's
    ///    account ([`caller_refuses_start`]) makes no write and journals no transfer log, so it
    ///    gets an empty lane and nothing is counted;
    /// 5. revm builds the frame, or answers it. A success answer the engine then rewrites into the
    ///    stop has its journal taken back to where its start began, as revm takes back a frame that
    ///    fails ([`take_back_a_stopped_answer`]).
    ///
    /// A frame answered at step 2, 3 or 5 — a `keylessDeploy` call carrying value, an
    /// interceptor's answer, a precompile's, revm's for a call it did not start — is held to the
    /// compute limit, as a frame that ran would be ([`settle_answer`]). A precompile,
    /// which revm runs at step 5, is decided from its price before it runs when the engine can
    /// price it, and answered without running when that price crosses the compute limit; one it
    /// cannot price is run on the gas the limit leaves the frame rather than on all its caller
    /// forwarded ([`hold_precompile`]).
    ///
    /// Step 1 is the pre-frame check: it answers a frame nothing may start. No limit has a part in
    /// it: validation admits no transaction a limit would stop before its first frame, and a start
    /// is held to the limits with the writes it makes, at step 4. The frame's own writes are
    /// counted after the interceptor, because an intercepted frame's writes are the interceptor's
    /// to count. A `keylessDeploy` call's frame is built as any call's, and its creation is started
    /// by its first run, through this same frame start, as its child.
    ///
    /// A frame answered before revm builds it gets an empty lane, so the lanes stay aligned with
    /// the results [`frame_return_result`](EvmTr::frame_return_result) pops. A creation answered
    /// with a stop still bumps its creator's nonce, as one that starts and reverts does.
    #[inline]
    fn frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        if let Some(result) = self.answered_before_building(&frame_init) {
            return Ok(ItemOrResult::Result(result));
        }
        let keyless = keyless::is_dispatched(&self.inner.ctx, &frame_init);
        let answer = if keyless {
            keyless::ready(&mut self.inner.ctx, &mut frame_init)?
        } else {
            self.intercept(&frame_init)
        };
        let (depth, gas_limit) = (frame_init.depth, input_gas_limit(&frame_init.frame_input));
        if let Some(mut result) = answer {
            self.inner.ctx.additional_limit.push_empty_frame();
            settle_answer(&mut self.inner.ctx, depth, gas_limit, &mut result);
            return Ok(ItemOrResult::Result(result));
        }
        record_caller(&mut self.inner.ctx, &mut self.inner.frame_stack, depth);
        let ctx = &mut self.inner.ctx;
        let refused = start_refused(ctx, &frame_init.frame_input);
        if refused {
            ctx.additional_limit.push_empty_frame();
        } else {
            let check =
                ctx.additional_limit.on_frame_init(&frame_init.frame_input, frame_init.depth);
            if check.exceeded_limit() {
                return Ok(ItemOrResult::Result(stop_before_building(ctx, &frame_init, &check)?));
            }
        }
        #[cfg(debug_assertions)]
        let counted = (
            ctx.additional_limit.frame_start_transfer_log(&frame_init.frame_input),
            refused,
            ctx.journal_ref().logs().len(),
        );
        let hold = hold_precompile(
            ctx,
            &self.inner.precompiles,
            &self.priced_precompiles,
            &mut frame_init,
            refused,
        );
        // Where the frame's start begins, for an answer the engine then stops.
        let start = journal_position(ctx);
        let outcome = if hold == PrecompileHold::Crossing {
            Err(crossing_answer(&frame_init.frame_input))
        } else {
            match self.inner.frame_init(frame_init)? {
                ItemOrResult::Item(frame) => Ok(frame.interpreter.input.target_address),
                ItemOrResult::Result(result) => Err(result),
            }
        };
        let ctx = &mut self.inner.ctx;
        #[cfg(debug_assertions)]
        assert_start_as_counted(ctx, counted, outcome.as_ref().err());
        debug_assert!(!keyless || outcome.is_ok(), "revm builds a keylessDeploy call's frame");
        match outcome {
            Ok(address) => {
                ctx.additional_limit.set_frame_address(address);
                if keyless {
                    ctx.keyless_frame = Some(keyless::KeylessFrame::Built);
                }
                Ok(ItemOrResult::Item(self.inner.frame_stack.get()))
            }
            Err(mut result) => {
                if let PrecompileHold::Clamped(withheld) = hold {
                    Detention::restore_forward(result.interpreter_result_mut(), withheld);
                }
                let succeeded = result.instruction_result().is_ok();
                settle_answer(ctx, depth, gas_limit, &mut result);
                take_back_a_stopped_answer(ctx, start, succeeded, &result);
                Ok(ItemOrResult::Result(result))
            }
        }
    }

    /// Runs the frame on top of the stack, unless it has a stop to return: its own, when what a
    /// child left it put it over a limit. Then the frame returns the stop without running another
    /// instruction (see [`before_frame_run`]). A `keylessDeploy` call's
    /// frame runs no instruction at all: its actions are made by hand ([`keyless::run`]).
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
            None if keyless::runs(ctx, frame) => keyless::run(ctx, frame)?,
            None => frame.interpreter.run_plain(
                evm.instruction.instruction_table(),
                evm.instruction.gas_table(),
                ctx,
            ),
        };
        let mut next = frame.process_next_action(ctx, action);
        after_frame_run(ctx, frame, &mut next);
        next.inspect(|next| {
            if next.is_result() {
                frame.set_finished(true);
            }
        })
    }

    /// Pops the returning frame's lane (merged on success, discarded on failure), then returns the
    /// result to the caller as revm does.
    ///
    /// The creation a `keylessDeploy` call started returns into the call: what the call answers is
    /// read off the creation's result before revm merges it ([`keyless::returning`]), and settled
    /// into the call after ([`keyless::settle`]).
    #[inline]
    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        let refund = self.inner.ctx.additional_limit.on_frame_return(&result);
        let creation = keyless::returning(&self.inner.ctx, &mut self.inner.frame_stack, &result);
        let returned = self.inner.frame_return_result(result)?;
        // The history of the records the caller paid for and the frame did not keep, given back
        // after the merge that adopted the frame's pools. `Some` means the outermost frame
        // returned, whose caller is the transaction: it paid for no record of it.
        if returned.is_some() {
            debug_assert_eq!(refund, 0, "the outermost frame's caller is the transaction");
            return Ok(returned);
        }
        let caller = &mut self.inner.frame_stack.get().interpreter.gas;
        caller.refill_history(refund);
        if let Some(creation) = creation {
            keyless::settle(&mut self.inner.ctx, caller, creation)?;
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
    /// answer settles as a frame that never started ([`answered_without_running`]).
    ///
    /// A frame nothing may start, one past the call-stack limit, is answered by
    /// [`EvmTr::frame_init`], after the inspector's `frame_start`, as every frame revm does not
    /// build is, and the inspector is told it ended.
    ///
    /// A keyless deployment is seen as the frames it is made of: the transaction's `keylessDeploy`
    /// call, and the creation it starts, one journal depth below it, as its child. The call's
    /// frame runs no code, so the inspector is not told an interpreter was initialized for it.
    #[inline]
    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        // An inspector may have rewritten the input the opcode left pending, or rewrite it at the
        // frame's start: the caller's answer is asked again on the input it leaves.
        let _ = self.inner.ctx.additional_limit.take_start_refused();
        let (ctx, inspector) = self.ctx_inspector();
        // What the calling opcode charged upfront, before the inspector's hooks can rewrite the
        // call it charged for.
        let upfront = UpfrontRecord::of(&frame_init.frame_input);
        if let Some(output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            let output = answered_by_inspector(ctx, inspector, &frame_init, output, upfront);
            return Ok(ItemOrResult::Result(output));
        }
        let (frame_input, depth) = (frame_init.frame_input.clone(), frame_init.depth);
        // What the start journals begins here. A start answered with a success without a frame
        // is revm's — a call to a precompile or to an account with no code, which takes its
        // checkpoint before it journals anything — or an interceptor's, which journals nothing.
        let start = journal_position(ctx);
        let logs_i = start.log_i;
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
            let source = ResultSource::Engine(start);
            frame_end_checked(ctx, inspector, &frame_input, &mut output, depth, source, upfront);
            return Ok(ItemOrResult::Result(output));
        }
        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        ctx.upfront_records.resize(depth, None);
        ctx.upfront_records.push(upfront);
        if ctx.journal().logs().len() != logs_i {
            inspect_logs(ctx, inspector, logs_i);
        }
        if !keyless::runs(ctx, frame) {
            inspector.initialize_interp(&mut frame.interpreter, ctx);
        }
        Ok(ItemOrResult::Item(frame))
    }

    /// revm's inspected frame run, with the stop short-circuit of [`EvmTr::frame_run`]: a frame
    /// with a stop to return returns it without a step, and the inspector sees it end. A
    /// `keylessDeploy` call's frame makes its actions by hand, as on the plain path, with no step.
    ///
    /// The step callbacks run through [`StepGuard`], so a charge the inspector makes on the
    /// interpreter's gas and the frame cannot pay does not classify the frame's end.
    #[inline]
    fn inspect_frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector, frame, instructions) = self.ctx_inspector_frame_instructions();
        let action = match before_frame_run(ctx, frame) {
            Some(action) => action,
            None if keyless::runs(ctx, frame) => keyless::run(ctx, frame)?,
            None => inspect_instructions(
                ctx,
                &mut frame.interpreter,
                StepGuard(&mut *inspector),
                instructions.instruction_table(),
                instructions.gas_table(),
            ),
        };
        let mut next = frame.process_next_action(ctx, action);
        after_frame_run(ctx, frame, &mut next);
        if let Ok(ItemOrResult::Result(result)) = &mut next {
            let source = ResultSource::Engine(frame.checkpoint);
            let upfront = ctx.upfront_records.get(frame.depth).copied().flatten();
            frame_end_checked(ctx, inspector, &frame.input, result, frame.depth, source, upfront);
            frame.set_finished(true);
        }
        next
    }
}

/// The action of a frame about to run: the stop it returns without running an instruction, when
/// it has one ([`stop_before_run`](crate::AdditionalLimit::stop_before_run)); `None` otherwise,
/// and the frame runs.
///
/// A frame runs here for the first time or after a child returned into it. A stop is always the
/// latter: the child was a failed creation whose nonce record, which lands on its creator, put the
/// creator over a limit, its budget or the transaction's, and the creator reverts alone, as for a
/// write of its own. A child that crossed a limit itself reverted alone, and its caller resumes.
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
        ctx.additional_limit.note_frame_stop(&stop),
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
/// the compute limit rather than its own gas: its halt becomes the compute stop, a revert of that
/// frame alone ([`stop_at_the_compute_limit`]). Every other out-of-gas halts, as it would without
/// the read.
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
            if let Some(stop) =
                ctx.detention.on_frame_end(instruction_result, result.gas_mut(), frame.depth)
            {
                stop_at_the_compute_limit(ctx, result.interpreter_result_mut(), stop);
            }
        }
        Err(_) => {}
    }
}

/// Records, for gas detention, the frame revm is about to build a child of at `depth`, while no
/// read has set a limit ([`Detention::on_child_build`]): the caller is still on top of the stack.
#[inline]
fn record_caller<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_stack: &mut FrameStack<EthFrame<EthInterpreter>>,
    depth: usize,
) {
    if depth > 0 && ctx.detention.records_callers() {
        debug_assert_eq!(frame_stack.index(), Some(depth - 1), "the caller is on top");
        ctx.detention.on_child_build(depth, &frame_stack.get().interpreter.gas);
    }
}

/// Holds a frame answered without running to the compute limit: the answer to a frame of
/// `gas_limit` at `depth`, which its caller forwarded.
///
/// An interceptor builds its answer on the whole gas the caller forwarded, the part gas detention
/// withholds from the caller's regular charges included. An answer that spent more than the frame
/// could have run on is answered out of gas and marked as a crossing, and becomes the stop as a
/// frame that ran would ([`Detention::on_answer`]); so does a precompile whose price crosses the
/// limit, and one the engine cannot price that ran out of the allowance it was run on
/// ([`hold_precompile`]). An answer that halts otherwise burns what it was given.
fn settle_answer<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    depth: usize,
    gas_limit: u64,
    answer: &mut FrameResult,
) {
    let answer = answer.interpreter_result_mut();
    if let Some(stop) = ctx.detention.on_answer(answer, depth, gas_limit) {
        stop_at_the_compute_limit(ctx, answer, stop);
    }
}

/// How gas detention holds the precompile call a frame start makes ([`hold_precompile`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrecompileHold {
    /// The call runs as it would without the limit.
    Unheld,
    /// The call runs on the allowance, and its answer gets back what was taken off the forward.
    Clamped(NonZeroU64),
    /// The call's price crosses the limit: it is answered without running ([`crossing_answer`]).
    Crossing,
}

/// Holds a precompile the frame `frame_init` is about to call to what gas detention's limit leaves
/// it: revm runs the precompile inside the frame's start, against its gas limit, so a precompile
/// forwarded more than the allowance would otherwise compute past the limit before its answer
/// could be classified.
///
/// A call to anything else, and a forward within the allowance, run as they are
/// ([`Unheld`](PrecompileHold::Unheld)). A precompile the engine can price
/// ([`PricedPrecompiles::price`]) is decided from its price before it runs:
///
/// - priced within the allowance, it runs on its whole forward, as without the limit: it charges
///   its price either way;
/// - priced past the allowance and within the forward, it needs gas the limit withholds: it is
///   answered without running ([`Crossing`](PrecompileHold::Crossing)), out of gas and marked as a
///   crossing, which the answer's settlement turns into the stop;
/// - priced past its whole forward, it runs on the forward and runs out of gas, as without the
///   limit: a failed call its caller survives.
///
/// The price does not tell whether the input passes the checks a precompile makes after its gas
/// check, so an input priced past the allowance and within the forward is the stop even when
/// those checks would fail the call. A call revm refuses on its caller's account — a value it
/// cannot fund — runs no precompile, and revm answers it as without the limit.
///
/// A precompile the engine cannot price is run on the allowance and sees it as its gas limit
/// ([`Clamped`](PrecompileHold::Clamped), with what was taken off the forward); its answer gets the
/// rest back ([`Detention::restore_forward`]) before it is settled. One priced past the allowance
/// runs out of gas on it and is the stop, whether its price is within the forward or not.
#[inline]
fn hold_precompile<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    precompiles: &PrecompilesMap,
    priced: &PricedPrecompiles,
    frame_init: &mut FrameInit,
    refused: bool,
) -> PrecompileHold {
    if ctx.detention.compute_limit().is_none() {
        return PrecompileHold::Unheld;
    }
    hold_detained_precompile(ctx, precompiles, priced, frame_init, refused)
}

/// [`hold_precompile`] once a read set a compute limit.
#[inline(never)]
fn hold_detained_precompile<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    precompiles: &PrecompilesMap,
    priced: &PricedPrecompiles,
    frame_init: &mut FrameInit,
    refused: bool,
) -> PrecompileHold {
    let FrameInput::Call(inputs) = &mut frame_init.frame_input else {
        return PrecompileHold::Unheld;
    };
    let Some(allowance) = ctx.detention.allowance(frame_init.depth, inputs.gas_limit) else {
        return PrecompileHold::Unheld;
    };
    let Some(withheld) = NonZeroU64::new(inputs.gas_limit.saturating_sub(allowance)) else {
        return PrecompileHold::Unheld;
    };
    if precompiles.get(&inputs.bytecode_address).is_none() {
        return PrecompileHold::Unheld;
    }
    let price = priced.price(precompiles, &inputs.bytecode_address, &inputs.input.as_bytes(ctx));
    let Some(price) = price else {
        inputs.gas_limit = allowance;
        return PrecompileHold::Clamped(withheld);
    };
    if price <= allowance || price > inputs.gas_limit || refused {
        return PrecompileHold::Unheld;
    }
    PrecompileHold::Crossing
}

/// The answer to the precompile call `input` starts when its price crosses gas detention's limit
/// ([`PrecompileHold::Crossing`]): out of gas without running, the forward untouched, and marked
/// as the crossing ([`Detention::cross_at_price`]).
#[cold]
#[inline(never)]
fn crossing_answer(input: &FrameInput) -> FrameResult {
    let mut answer = synthetic_frame_result(input, InstructionResult::PrecompileOOG, Bytes::new());
    Detention::cross_at_price(answer.interpreter_result_mut());
    answer
}

/// The gas limit of the frame `input` starts.
const fn input_gas_limit(input: &FrameInput) -> u64 {
    match input {
        FrameInput::Call(inputs) => inputs.gas_limit,
        FrameInput::Create(inputs) => inputs.gas_limit(),
        FrameInput::Empty => 0,
    }
}

/// Turns a frame that crossed gas detention's compute limit into the compute stop: a revert of
/// that frame alone carrying `MegaLimitExceeded` (kind: compute). The frame's gas is what it had
/// before the charge that crossed, which goes back with the revert to its caller. The caller
/// resumes held to what the limit leaves the transaction, which is less than the charge that
/// crossed, so a caller that carries on is stopped at its next charge past it.
///
/// The crossing charge's size is not kept, so the stop reports the transaction's compute at the
/// crossing as what was used ([`LimitCheck::ExceedsLimit`]): what its regular ledger bills.
fn stop_at_the_compute_limit<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    result: &mut InterpreterResult,
    stop: ComputeStop,
) {
    let stop = ctx.additional_limit.crossed(LimitKind::ComputeGas, stop.limit, stop.used, false);
    result.result = InstructionResult::Revert;
    result.output = ctx.additional_limit.note_frame_stop(&stop);
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

    /// Step 1 of [`EvmTr::frame_init`], the pre-frame check: answers a frame nothing may start, one
    /// past the call-stack limit ([`call_too_deep`]), with the empty lane that stands in for it. On
    /// the inspected path it comes after the inspector's `frame_start`.
    #[inline]
    fn answered_before_building(&mut self, frame_init: &FrameInit) -> Option<FrameResult> {
        let answer = call_too_deep(frame_init);
        if answer.is_some() {
            self.inner.ctx.additional_limit.push_empty_frame();
        }
        answer
    }
}

/// Takes the journal back to `start`, where a frame's start began, when revm answered the frame
/// with a success (`succeeded`) that the engine then rewrote into `result`, the stop.
///
/// revm commits an answered frame's journal checkpoint before its answer returns: a value transfer
/// to an account with no code, or to a precompile that answers, has moved the value, created the
/// recipient and journaled the transfer log by then. The stop is a revert of the frame alone, and
/// the caller that resumes keeps its own writes, so the answer's writes are taken back here, as
/// revm takes back a frame that fails; for the transaction's own frame, which has no caller, this
/// is all that takes them back. The journal's depth is left as it is.
fn take_back_a_stopped_answer<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    start: JournalCheckpoint,
    succeeded: bool,
    result: &FrameResult,
) {
    if succeeded && !result.instruction_result().is_ok() {
        revert_journal_to(ctx, start);
    }
}

/// Settles `output`, the answer the inspector gave the frame `frame_init` starts in its place.
/// The depth guard still holds: an answer cannot reach past the call-stack limit.
fn answered_by_inspector<DB: Database, ExtEnvs: ExternalEnvTypes, INSP>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    frame_init: &FrameInit,
    output: FrameResult,
    upfront: Option<UpfrontRecord>,
) -> FrameResult
where
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
{
    let output = call_too_deep(frame_init).unwrap_or(output);
    answered_without_running(ctx, inspector, frame_init, output, upfront)
}

/// Settles `output`, the answer an inspector gave the frame `frame_init` starts in its place, as
/// [`EvmTr::frame_init`] settles an answer: an empty lane stands in for the frame, and the answer
/// is held to the compute limit ([`settle_answer`]). Then the inspector is told the frame ended,
/// and the answer settles as a frame that kept nothing: the frame never started, so no value moved
/// and no account was added, and the upfront state gas its caller's opcode was charged comes back
/// whatever the answer ([`frame_end_checked`]).
fn answered_without_running<DB: Database, ExtEnvs: ExternalEnvTypes, INSP>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inspector: &mut INSP,
    frame_init: &FrameInit,
    mut output: FrameResult,
    upfront: Option<UpfrontRecord>,
) -> FrameResult
where
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
{
    ctx.additional_limit.push_empty_frame();
    let gas_limit = input_gas_limit(&frame_init.frame_input);
    settle_answer(ctx, frame_init.depth, gas_limit, &mut output);
    let (input, depth) = (&frame_init.frame_input, frame_init.depth);
    frame_end_checked(ctx, inspector, input, &mut output, depth, ResultSource::Inspector, upfront);
    output
}

/// Answers a frame a limit stops before it is built with the stop: a revert with the stop's
/// [`MegaLimitExceeded`](crate::MegaLimitExceeded) output, as a [`synthetic_frame_result`] that
/// settles like a frame revm ran.
///
/// A creation still bumps its creator's nonce, as one that starts and reverts does: a nested
/// creation's caller sees an ordinary failed creation, and a creation transaction whose own frame
/// is stopped at its start cannot be replayed.
fn stop_before_building<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &FrameInit,
    check: &LimitCheck,
) -> Result<FrameResult, ContextDbError<MegaContext<DB, ExtEnvs>>> {
    if let FrameInput::Create(inputs) = &frame_init.frame_input {
        let _ = ctx.journal_mut().load_account_mut(inputs.caller())?.data.bump_nonce();
    }
    let output = ctx.additional_limit.note_frame_stop(check);
    Ok(synthetic_frame_result(&frame_init.frame_input, InstructionResult::Revert, output))
}

/// The depth guard: a frame past the call-stack limit, of any call scheme or a creation, answered
/// with `CallTooDeep`, its gas untouched and its reservoir carried — the answer revm gives it.
///
/// revm checks the depth first when it builds a frame, before it moves value or bumps a creator's
/// nonce. The guard gives the same answer before anything else sees the frame: an interceptor or an
/// inspector answers before revm builds anything, so without the guard a system contract could be
/// reached at any depth; and a start's writes are counted before revm decides it, so without the
/// guard a creation past the limit would count its creator's nonce record — which outlives a failed
/// creation — for a nonce revm never bumps.
///
/// No transaction reaches the limit: regular gas is capped by the execution cap and every call
/// forwards at most sixty-three sixty-fourths of what its caller has left, so a frame at depth
/// 1,024 has a few gas.
fn call_too_deep(frame_init: &FrameInit) -> Option<FrameResult> {
    (frame_init.depth > CALL_STACK_LIMIT as usize).then(|| {
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
/// It refuses a start past the call-stack limit too, but the depth guard answers that start
/// before revm sees it ([`call_too_deep`]), so revm never answers one. A built frame has run no
/// instruction yet, so the move is all revm has done. An answered call
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
    debug_assert!(
        answer.is_none_or(|answer| answer.instruction_result() != InstructionResult::CallTooDeep),
        "the depth guard answers every start past the call-stack limit"
    );
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

/// Whether executing this transaction creates its caller's account: a deposit-like transaction
/// whose caller is empty, which the deposit path materialises by bumping its nonce or by minting
/// to it.
///
/// Read before op-revm deducts the caller, which is what materialises the account, and after
/// [`validate_env`](Handler::validate_env), which promoted a system-address transaction: a
/// deposit-like transaction is a deposit by then. Every other transaction pays a fee its caller
/// must hold, so it has an account already.
///
/// The read loads the caller as op-revm's deduction then does: the transaction's caller starts
/// warm, so the read changes nothing the transaction pays.
fn deposit_creates_caller<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) -> Result<bool, DB::Error> {
    if ctx.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE {
        return Ok(false);
    }
    let caller = ctx.tx().caller();
    Ok(ctx.journal_mut().load_account(caller)?.data.info.is_empty())
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

/// [`caller_refuses_start`] for the frame `input` asks for, as the opcode starting it — or, for the
/// transaction's own frame, the charge of its record before execution — already found it: nothing
/// between the two changes the caller's account, so a second lookup of it would give the same
/// answer. A debug build makes the lookup and asserts that it does.
///
/// A start that moves no value and creates nothing is never refused there, and takes nothing.
#[inline]
fn start_refused<DB: revm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    input: &FrameInput,
) -> bool {
    let refusable = match input {
        FrameInput::Call(inputs) => inputs.transfers_value(),
        FrameInput::Create(_) => true,
        FrameInput::Empty => false,
    };
    if !refusable {
        return false;
    }
    let Some(refused) = ctx.additional_limit.take_start_refused() else {
        return caller_refuses_start(ctx, input);
    };
    debug_assert_eq!(
        refused,
        caller_refuses_start(ctx, input),
        "the answer staged for a frame's start is the one its caller's account gives"
    );
    refused
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

/// Records the account writes of the EIP-7702 authorities applied since journal entry
/// `journal_i`, and answers how many there are: each applied authorization bumps its authority's
/// nonce once, so the distinct authorities other than the sender are the accounts written. The
/// sender's write is part of the transaction body.
fn record_applied_authorities<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    journal_i: usize,
) -> u64 {
    if ctx.tx().tx_type() != TransactionType::Eip7702 {
        return 0;
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
    ctx.additional_limit.record_applied_authorities(caller, applied, target_is_authority);
    // An applied authority that is the block beneficiary wrote the beneficiary's account.
    if authorities.binary_search(&ctx.block().beneficiary).is_ok() {
        ctx.detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
    }
    applied
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
/// run, and which revm answers with a success. The answer is left for the first frame's init,
/// which asks the same of the same account.
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
    let (caller, value, creation) = (tx.caller(), tx.value(), tx.kind().is_create());
    let top_level_record = writes_a_record && {
        let refused = caller_refuses(ctx, caller, value, creation);
        ctx.additional_limit.stage_start_refused(refused);
        !refused
    };
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
    use crate::{test_utils::MemoryDatabase, MegaSpecId};
    use alloy_primitives::{address, U256};
    use revm::interpreter::{
        gas::WithheldCrossing, CallInput, CallInputs, CallValue, SharedMemory,
    };
    #[cfg(debug_assertions)]
    use revm::interpreter::{CreateInputs, CreateScheme};
    use std::boxed::Box;

    const CALLER: Address = address!("00000000000000000000000000000000000e0001");
    const TARGET: Address = address!("00000000000000000000000000000000000e0002");

    /// A `CALL` of one wei to `TARGET`, which its opcode charged a new account for.
    fn value_call() -> FrameInput {
        FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            reservoir: 0,
            bytecode_address: TARGET,
            known_bytecode: Default::default(),
            target_address: TARGET,
            caller: CALLER,
            value: CallValue::Transfer(U256::from(1)),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: true,
        }))
    }

    /// A `CREATE` its opcode charged the created account for.
    #[cfg(debug_assertions)]
    fn creation() -> FrameInput {
        let mut inputs =
            CreateInputs::new(CALLER, CreateScheme::Create, U256::ZERO, Bytes::new(), 100_000, 0);
        inputs.set_charged_create_state_gas(true);
        inputs.set_charged_state_gas_address(TARGET);
        FrameInput::Create(Box::new(inputs))
    }

    /// A context whose transaction read the block environment before its first frame under a cap
    /// of `cap`, so a frame at depth 0 starts with an allowance of `cap`.
    fn detained(cap: u64) -> MegaContext<MemoryDatabase> {
        let mut ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        ctx.detention.reset(true, cap, cap);
        ctx.detention.mark_before_execution(VolatileDataAccess::TIMESTAMP);
        ctx
    }

    /// A `STATICCALL` at depth 0 to `to` with `input`, forwarded `forward`.
    fn static_call(to: Address, input: &[u8], forward: u64) -> FrameInit {
        FrameInit {
            depth: 0,
            memory: SharedMemory::new(),
            frame_input: FrameInput::Call(Box::new(CallInputs {
                input: CallInput::Bytes(Bytes::copy_from_slice(input)),
                return_memory_offset: 0..0,
                gas_limit: forward,
                reservoir: 0,
                bytecode_address: to,
                known_bytecode: Default::default(),
                target_address: to,
                caller: CALLER,
                value: CallValue::Apparent(U256::ZERO),
                scheme: CallScheme::StaticCall,
                is_static: true,
                charged_new_account_state_gas: false,
            })),
        }
    }

    /// What `hold_precompile` decides for a call to `to`, forwarded `forward`, under an allowance
    /// of `cap`, with `priced`; and the gas limit the call is left to run on.
    fn hold(
        cap: u64,
        priced: &PricedPrecompiles,
        dispatched: &PrecompilesMap,
        to: Address,
        forward: u64,
        refused: bool,
    ) -> (PrecompileHold, u64) {
        hold_input(cap, priced, dispatched, to, &[0; 32], forward, refused)
    }

    /// [`hold`] for a call of `input`.
    fn hold_input(
        cap: u64,
        priced: &PricedPrecompiles,
        dispatched: &PrecompilesMap,
        to: Address,
        input: &[u8],
        forward: u64,
        refused: bool,
    ) -> (PrecompileHold, u64) {
        let ctx = detained(cap);
        let mut frame_init = static_call(to, input, forward);
        let held = hold_precompile(&ctx, dispatched, priced, &mut frame_init, refused);
        (held, input_gas_limit(&frame_init.frame_input))
    }

    /// A precompile the engine prices is decided from its price before it runs: within the
    /// allowance, and past its whole forward, it runs on the forward as without the limit; past
    /// the allowance and within the forward, it is answered without running, out of gas and
    /// marked as a crossing whose record is the forward. A call revm refuses on its caller's
    /// account runs as without the limit.
    #[test]
    fn test_a_priced_precompile_is_decided_from_its_price() {
        const KZG: Address = crate::kzg_point_evaluation::ADDRESS;
        const PRICE: u64 = crate::kzg_point_evaluation::GAS_COST;
        let (priced, map) = (PricedPrecompiles::default(), crate::satin_precompiles_map());

        // Within the allowance: runs on the forward.
        let (held, ran_on) = hold(PRICE, &priced, &map, KZG, 3 * PRICE, false);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, 3 * PRICE));

        // Past the allowance, within the forward: the crossing, without running.
        for forward in [PRICE, 3 * PRICE] {
            let (held, ran_on) = hold(PRICE - 1, &priced, &map, KZG, forward, false);
            assert_eq!((held, ran_on), (PrecompileHold::Crossing, forward), "the input as it was");
        }

        // Past the whole forward: a plain out-of-gas, run on the forward.
        let (held, ran_on) = hold(PRICE / 2, &priced, &map, KZG, PRICE - 1, false);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, PRICE - 1));

        // Refused on the caller's account: revm answers it without running the precompile.
        let (held, ran_on) = hold(PRICE - 1, &priced, &map, KZG, 3 * PRICE, true);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, 3 * PRICE));

        // A forward within the allowance, and a call to anything but a precompile, run as they are.
        let (held, ran_on) = hold(3 * PRICE, &priced, &map, KZG, 3 * PRICE, false);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, 3 * PRICE));
        let (held, ran_on) = hold(PRICE - 1, &priced, &map, TARGET, 3 * PRICE, false);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, 3 * PRICE));
    }

    /// op-revm's size-limited precompiles in the Satin set — the BN254 pairing and the BLS12-381
    /// G1 MSM, G2 MSM and pairing — are decided from their prices as the KZG entry is. Called
    /// with two pairs of zeros, which their EIPs price, the call runs on its forward within the
    /// allowance and past its forward, and is the crossing between the two. An input past the size
    /// limit is refused before any gas check, so its price is nothing, and it runs on its forward
    /// under any allowance, to be refused as without the limit.
    #[test]
    fn test_a_size_limited_precompile_is_decided_from_its_price() {
        use op_revm::precompiles::{bls12_381, bn254_pair};
        let (priced, map) = (PricedPrecompiles::default(), crate::satin_precompiles_map());
        // (the precompile, the length of a pair, the size limit, the price of two pairs)
        let wrappers = [
            // EIP-1108: 45,000, and 34,000 a pair.
            (bn254_pair::KARST, 192, bn254_pair::KARST_MAX_INPUT_SIZE, 45_000 + 2 * 34_000),
            // EIP-2537: 12,000 a pair, two pairs discounted to 949 per mille.
            (
                bls12_381::JOVIAN_G1_MSM,
                160,
                bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE,
                2 * 12_000 * 949 / 1_000,
            ),
            // EIP-2537: 22,500 a pair, two pairs undiscounted.
            (bls12_381::JOVIAN_G2_MSM, 288, bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE, 2 * 22_500),
            // EIP-2537: 37,700, and 32,600 a pair.
            (
                bls12_381::JOVIAN_PAIRING,
                384,
                bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE,
                37_700 + 2 * 32_600,
            ),
        ];
        for (precompile, pair, limit, price) in wrappers {
            let to = *precompile.address();
            let two_pairs = std::vec![0; 2 * pair];
            let hold = |cap, input: &[u8], forward| {
                hold_input(cap, &priced, &map, to, input, forward, false)
            };

            // Within the allowance: runs on the forward.
            assert_eq!(hold(price, &two_pairs, 3 * price), (PrecompileHold::Unheld, 3 * price));

            // Past the allowance, within the forward: the crossing, without running.
            for forward in [price, 3 * price] {
                let held = hold(price - 1, &two_pairs, forward);
                assert_eq!(held, (PrecompileHold::Crossing, forward), "{to}: {forward}");
            }

            // Past the whole forward: a plain out-of-gas, run on the forward.
            let held = hold(price / 2, &two_pairs, price - 1);
            assert_eq!(held, (PrecompileHold::Unheld, price - 1), "{to}");

            // Past the size limit: priced at nothing, run on the forward.
            let over = std::vec![0; limit + pair];
            assert_eq!(hold(1, &over, 3 * price), (PrecompileHold::Unheld, 3 * price), "{to}");
        }
    }

    /// The answer to a call whose price crosses the limit is out of gas without running, with its
    /// forward untouched and the crossing recorded with the forward.
    #[test]
    fn test_a_crossing_answer_is_marked_with_its_forward() {
        for forward in [1, 100_000, 3_000_000] {
            let input = static_call(crate::kzg_point_evaluation::ADDRESS, &[0; 32], forward);
            let FrameResult::Call(answer) = crossing_answer(&input.frame_input) else {
                panic!("a call's answer")
            };
            assert!(!answer.was_precompile_called, "answered without running");
            assert_eq!(answer.result.result, InstructionResult::PrecompileOOG);
            assert_eq!(answer.result.gas.limit(), forward);
            assert_eq!(answer.result.gas.remaining(), forward, "nothing spent");
            let record = NonZeroU64::new(forward).map(WithheldCrossing::with_remaining);
            assert_eq!(answer.result.gas.withheld_crossing(), record);
        }
    }

    /// A precompile the engine cannot price keeps the clamp: it runs on the allowance, and its
    /// answer is owed the rest of the forward. That is a node's own precompile, a Satin address a
    /// node replaced, and every precompile of a set that is not the Satin one, op-revm's wrapper of
    /// the BN254 pairing included.
    #[test]
    fn test_an_unpriced_precompile_runs_on_the_allowance() {
        const KZG: Address = crate::kzg_point_evaluation::ADDRESS;
        let pairing = *op_revm::precompiles::bn254_pair::KARST.address();
        let own = address!("00000000000000000000000000000000000e0003");
        let mut map = crate::satin_precompiles_map();
        map.apply_precompile(&own, |_| {
            Some(alloy_evm::precompiles::DynPrecompile::new(
                revm::precompile::PrecompileId::Custom("own".into()),
                |input| {
                    Ok(revm::precompile::PrecompileOutput::new(1, Bytes::new(), input.reservoir))
                },
            ))
        });
        let mut replaced = PricedPrecompiles::default();
        replaced.record_replaced(KZG);
        let mut foreign = PricedPrecompiles::default();
        foreign.record_foreign();
        for (priced, to) in [
            (&PricedPrecompiles::default(), own),
            (&replaced, KZG),
            (&foreign, KZG),
            (&foreign, pairing),
        ] {
            let (held, ran_on) = hold(1_000, priced, &map, to, 300_000, false);
            let clamped = PrecompileHold::Clamped(NonZeroU64::new(299_000).unwrap());
            assert_eq!(held, clamped, "{to}");
            assert_eq!(ran_on, 1_000, "{to}: run on the allowance");
        }
    }

    /// A frame answered with a success that the engine then rewrote into the stop, at any depth,
    /// has what its start wrote taken back to where its start began. An answer revm made a failure
    /// took its writes back itself, and one the engine did not rewrite keeps them.
    #[test]
    fn test_a_success_answer_rewritten_into_the_stop_is_taken_back() {
        for (succeeded, result, taken_back) in [
            (true, InstructionResult::Revert, true),
            (true, InstructionResult::Stop, false),
            (false, InstructionResult::Revert, false),
        ] {
            let case = format!("succeeded {succeeded}, now {result:?}");
            let mut ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
            let start = journal_position(&mut ctx);
            // What the answer wrote: a bump of the recipient's nonce.
            let _ = ctx.journal_mut().load_account_mut(TARGET).unwrap().data.bump_nonce();
            let answer = synthetic_frame_result(&value_call(), result, Bytes::new());
            take_back_a_stopped_answer(&mut ctx, start, succeeded, &answer);
            let nonce = ctx.journal_ref().state.get(&TARGET).map_or(0, |a| a.info.nonce);
            assert_eq!(nonce, u64::from(!taken_back), "{case}");
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

    /// The guard trips on a start revm answered past the call-stack limit, which the depth guard
    /// answers before revm sees it, for a creation and a call alike.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the depth guard answers every start past the call-stack limit")]
    fn test_a_start_revm_answered_past_the_depth_limit_trips() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        for input in [creation(), value_call()] {
            let too_deep =
                synthetic_frame_result(&input, InstructionResult::CallTooDeep, Bytes::new());
            assert_start_as_counted(&ctx, (false, false, 0), Some(&too_deep));
        }
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
}
