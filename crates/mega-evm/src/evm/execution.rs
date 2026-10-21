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
        ContextError, ContextTr, FrameStack, Journal, JournalTr, Transaction,
    },
    context_interface::{
        cfg::{
            gas::{GasTracker, StartCharge},
            GasId, StateGasCharge, StateGasSite,
        },
        journaled_state::{entry::JournalEntry, JournalCheckpoint},
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
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallScheme, FrameInput, Gas,
        InitialAndFloorGas, InstructionResult, InterpreterAction, InterpreterResult,
    },
    primitives::{Address, Bytes, TxKind, CALL_STACK_LIMIT},
    Inspector,
};

use crate::{
    access::ComputeStop,
    charge_write_records_history,
    evm::inspector::{frame_end_checked, ResultSource, StepGuard, UpfrontRecord},
    history_gas,
    limit::{Counted, CreatorRecord, StartRecords},
    synthetic_frame_result,
    system::keyless,
    write_record_history_gas, Detention, ExternalEnvTypes, LimitCheck, LimitKind, MegaContext,
    MegaEvm, MegaInstructions, PricedPrecompiles, VolatileDataAccess, TX_BODY_OVER_DATA_SIZE_LIMIT,
};

/// The regular gas a system call runs on, at most: EIP-8037's system-call budget. The rest of its
/// gas limit is its state-gas reservoir ([`MegaHandler`]'s `tx_gas`).
pub const SYSTEM_CALL_REGULAR_GAS_LIMIT: u64 = 30_000_000;

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
    /// The reservoir the transaction's result settled with, read by
    /// [`post_execution`](Handler::post_execution) where revm builds the result's gas, and
    /// consumed by the report once the result is built
    /// ([`execution_result`](Handler::execution_result)) or by the error path
    /// ([`catch_error`](Handler::catch_error)), so a handler reused for a system call, whose path
    /// makes no capture, reports the system call's own reservoir.
    settled_reservoir: Cell<Option<u64>>,
}

impl<EVM, ERROR, FRAME> MegaHandler<EVM, ERROR, FRAME> {
    /// Creates a handler.
    pub fn new() -> Self {
        Self {
            op: OpHandler::new(),
            deposit_creates_caller: Cell::new(false),
            settled_reservoir: Cell::new(None),
        }
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
        let applied = applied_authorities(evm.ctx_mut(), checkpoint.journal_i);
        if !charge_records_made_outside_a_frame(evm.ctx_mut(), gas, applied) {
            runtime_oog_unwind(evm.ctx(), checkpoint)?;
            return Ok(None);
        }
        evm.ctx_mut().additional_limit.count_authorities(applied);
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

    /// revm's intrinsic gas, held to a gas limit that covers the history gas of the transaction's
    /// body as well, which [`tx_gas`](Handler::tx_gas) charges.
    ///
    /// The body's bytes are fixed before the transaction runs, so their price is part of what a
    /// gas limit has to cover for the transaction to be valid at all: a limit that falls short is
    /// rejected before inclusion rather than included as an out-of-gas that burns the whole limit.
    /// revm made that check on its own figure, so the grown figure is checked again here, with the
    /// same error naming the same two numbers. The cap check revm made stands as it was: it tests
    /// the regular intrinsic gas, which the body's history does not touch.
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
        let gas = self.op.validate_initial_tx_gas(evm)?;
        if !evm.ctx_ref().prices_history() {
            return Ok(gas);
        }
        let bytes = evm.ctx_ref().additional_limit.body();
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
        evm.ctx_mut().additional_limit.set_intrinsic_history(history, bytes);
        Ok(gas)
    }

    /// revm's transaction-level gas, with the history gas of the transaction's body charged on
    /// it, and a system call's split as EIP-8037 has it: at most [`SYSTEM_CALL_REGULAR_GAS_LIMIT`]
    /// of its gas limit is regular gas, which is what `GAS` reads inside it, and the rest is its
    /// state-gas reservoir, which the state it writes draws first.
    ///
    /// The body's history is intrinsic: it comes out of the reservoir first and its excess off the
    /// regular budget, as EIP-8037's intrinsic state gas does, so a body does not take the
    /// execution cap away from computation; and, as intrinsic gas, no reservoir a frame hands back
    /// unwinds it onto the regular budget, so it is charged as no spill. It is charged here rather
    /// than before the first frame because revm's runtime out-of-gas fallback rebuilds the
    /// transaction's gas through this phase, and a transaction that runs out of gas there keeps its
    /// body charged. Validation admitted no gas limit that cannot pay for it
    /// ([`validate_initial_tx_gas`](Handler::validate_initial_tx_gas)).
    ///
    /// revm builds a system call's gas here, on no intrinsic gas, and so does its runtime
    /// out-of-gas fallback, which a system call does not reach: it carries no value and its target
    /// is no delegator. Every transaction's gas is split by the execution cap alone, the
    /// system-address transaction's included, which is a deposit and not a system call; and under
    /// the neutral configuration a system call's is not split, as revm's own runner has it.
    fn tx_gas(&self, evm: &mut Self::Evm, init_and_floor_gas: &InitialAndFloorGas) -> GasTracker {
        let mut gas = self.op.tx_gas(evm, init_and_floor_gas);
        let history = evm.ctx_ref().additional_limit.intrinsic_history_gas();
        if history != 0 {
            let spilled = gas.state_gas_spilled();
            let charged = gas.record_history_cost(history);
            debug_assert!(charged, "validation admits no gas limit that cannot pay for the body");
            gas.set_state_gas_spilled(spilled);
        }
        if evm.ctx_ref().splits_system_call_gas() {
            let margin = gas.remaining().saturating_sub(SYSTEM_CALL_REGULAR_GAS_LIMIT);
            gas.set_remaining(gas.remaining() - margin);
            gas.set_reservoir(gas.reservoir() + margin);
        }
        gas
    }

    /// revm's first frame, on the transaction's pool, carrying as its start charge the history
    /// charged before execution for the write record it makes; with a read of the block
    /// beneficiary's account marked for gas detention when the transaction's recipient delegates
    /// to the beneficiary: revm resolves that delegate through the journal, where the Host does
    /// not see it.
    ///
    /// The pool is what the transaction has left of its data-size limit once its body and the
    /// records of the authorities it applied are counted, under the frame cap
    /// ([`AdditionalLimit::first_frame_pool`](crate::AdditionalLimit)); revm hands the frame the
    /// whole of it.
    fn first_frame_input(
        &mut self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<FrameInit>, Self::Error> {
        gas.set_bytes_remaining(evm.ctx_ref().additional_limit.first_frame_pool());
        let Some(mut frame) = self.op.first_frame_input(evm, gas)? else {
            // The runtime gas phase could not pay for the first frame's start: EIP-2780's charge
            // for the account the frame adds, a call's recipient or a creation's account, or the
            // access to the code a delegated recipient runs, warm or cold. A price of that account
            // charge that could not be read ends here too: the lookup recorded its cause, which
            // fails the transaction when it settles. In each case revm takes the phase's state
            // back, the authorizations with their records, and drops its partial charges by
            // rebuilding the transaction's gas from its intrinsic cost, the history charged for
            // the first frame's write record included, which no frame carries back.
            evm.ctx_mut().additional_limit.take_back_authorities();
            return Ok(None);
        };
        let charge = StartCharge::new(evm.ctx_ref().additional_limit.first_frame_charge(), 0);
        match &mut frame.frame_input {
            FrameInput::Call(inputs) => inputs.charged_start = charge,
            FrameInput::Create(inputs) => inputs.set_charged_start(charge),
            FrameInput::Empty => {}
        }
        evm.ctx_mut().mark_beneficiary_delegate();
        Ok(Some(frame))
    }

    /// Settles the outermost frame into the transaction's gas as op-revm does (op-revm replaces
    /// revm's settlement, so revm's never runs here): a transaction whose own frame a limit stopped
    /// settles like an EIP-8037 revert, its unspent regular gas and reservoir back to the sender.
    ///
    /// The write record the transaction's own frame makes was charged before execution and rides
    /// on the frame's input as its start charge, which the settlement gives back when the frame
    /// did not succeed, the way EIP-8037 gives back the state gas of the account that frame would
    /// have created. A frame answered without returning through
    /// [`frame_return_result`](EvmTr::frame_return_result) — revm's answer to the first frame, an
    /// interceptor's, an inspector's — closes its start and gives up its stop here.
    ///
    /// Keeps the history gas the transaction spent, which its gas holds once the frame settled into
    /// it: what the body was charged, and what the frames charged net of what they gave back; and
    /// the history bytes it appended, which are what those charges were made for.
    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut FrameResult,
        parent_gas: &mut GasTracker,
    ) -> Result<(), Self::Error> {
        let layer = &mut evm.ctx_mut().additional_limit;
        layer.close_start(0, frame_result.instruction_result().is_ok());
        layer.take_frame_stop(frame_result, true);
        self.op.last_frame_result(evm, frame_result, parent_gas)?;
        let priced = evm.ctx_ref().prices_history();
        let history = 0_u64.saturating_add_signed(frame_result.gas().history_gas_spent());
        evm.ctx_mut().additional_limit.settle_history(priced, history);
        Ok(())
    }

    /// revm's post-execution, with the reservoir the transaction leaves unspent read first, where
    /// revm builds the result's gas: before the EIP-7623 floor check, which absorbs the reservoir
    /// into the floor's cost when the floor binds. The refund before it does not move the
    /// reservoir.
    fn post_execution(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<ResultGas, Self::Error> {
        self.settled_reservoir.set(Some(exec_result.gas().reservoir()));
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

    /// op-revm's result, and the reservoir it settled with reported
    /// ([`AdditionalLimit::reservoir_remaining`](crate::AdditionalLimit::reservoir_remaining)).
    ///
    /// revm's system-call path settles without [`post_execution`](Handler::post_execution): the
    /// result it hands over here is the settled one, and the capture it finds is none, since the
    /// report of the execution before consumed it. A deposit that halts is not a result: op-revm
    /// turns it into a failed deposit, which leaves no reservoir.
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: FrameResult,
        result_gas: revm::context::result::ResultGas,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error> {
        let reservoir = self.settled_reservoir.take().unwrap_or_else(|| result.gas().reservoir());
        let result = self.op.execution_result(evm, result, result_gas)?;
        evm.ctx_mut().additional_limit.set_reservoir_remaining(reservoir);
        Ok(result)
    }

    /// op-revm's error handling, which answers a deposit's transaction error with a failed-deposit
    /// halt and discards everything the deposit did. The halt is what the transaction reports;
    /// what the deposit counted stands, the body and the Oracle hints it forwarded before it
    /// halted, as for any transaction that halts. Any other error refuses the transaction, and the
    /// layer's state is reset by the next one. A reservoir captured for a result that is not
    /// reported is dropped here, so the next execution on this handler does not report it.
    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error> {
        self.settled_reservoir.take();
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
    // Implied by `EvmTrError<EVM>`, but the `ContextTr<Journal = Journal<DB>>` bound above
    // stops the compiler from normalizing the context's database type down to `DB`.
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
    ///    the frame or lets it start;
    /// 4. where the journal stands is marked, and the records the start makes are read off it
    ///    ([`FrameStart::of`]);
    /// 5. revm builds the frame, or answers it, and the start is held to the limits on the pool its
    ///    frame starts with, where its records and transfer log are drawn
    ///    ([`AdditionalLimit::count_start`](crate::AdditionalLimit)): a frame revm built returns
    ///    the stop on its first run when its start crossed a limit
    ///    ([`AdditionalLimit::hold_start`](crate::AdditionalLimit)), and an answer gives way to the
    ///    stop ([`stop_the_answer`]), what its start moved taken back to the mark, as revm takes
    ///    back a frame that fails ([`take_back_a_stopped_answer`]). A start revm refuses on its
    ///    caller's account writes nothing and counts nothing, and a creation revm answers kept
    ///    nothing.
    ///
    /// A frame answered at step 2, 3 or 5 — a `keylessDeploy` call carrying value, an
    /// interceptor's answer, a precompile's, revm's for a call it did not start — is held to the
    /// compute limit, as a frame that ran would be ([`settle_answer`]). A precompile, which revm
    /// runs at step 5, is decided from its price before it runs when the engine can price it, and
    /// answered without running when that price crosses the compute limit; one it cannot price is
    /// run on the gas the limit leaves the frame rather than on all its caller forwarded
    /// ([`hold_precompile`]).
    ///
    /// Step 1 is the pre-frame check: it answers a frame nothing may start. No limit has a part in
    /// it: validation admits no transaction a limit would stop before its first frame, and a start
    /// is held to the limits with the writes it makes, at step 5. The frame's own writes are
    /// counted after the interceptor, because an intercepted frame's writes are the interceptor's
    /// to count. A `keylessDeploy` call's frame is built as any call's, and its creation is started
    /// by its first run, through this same frame start, as its child.
    #[inline]
    fn frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        // What the start's hook left for revm's decision is this start's, whoever decides it.
        let creator = self.inner.ctx.additional_limit.take_creator_record();
        #[cfg(debug_assertions)]
        let charged = self.inner.ctx.charged_records.take();
        if let Some(result) = call_too_deep(&frame_init) {
            debug_assert!(
                creator.is_none(),
                "a creation past the depth limit takes no address, and its hook counts nothing"
            );
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
            settle_answer(&mut self.inner.ctx, depth, gas_limit, &mut result);
            return Ok(ItemOrResult::Result(result));
        }
        let ctx = &mut self.inner.ctx;
        #[cfg(debug_assertions)]
        let start = FrameStart::of(ctx, &frame_init, creator.is_some(), charged);
        #[cfg(not(debug_assertions))]
        let start = FrameStart::of(ctx, &frame_init, creator.is_some());
        let hold = hold_precompile(
            ctx,
            &self.inner.precompiles,
            &self.priced_precompiles,
            &mut frame_init,
        );
        if hold == PrecompileHold::Crossing {
            // revm never decides the start: what it would have counted is held to the limits as
            // the hypothetical the answer gives way to. A creation is never a precompile's.
            let mut result = crossing_answer(&frame_init.frame_input);
            let verdict = ctx.additional_limit.hypothetical_start(&start.counted, start.share);
            stop_the_answer_at(ctx, &start, &mut result, verdict);
            settle_answer(ctx, depth, gas_limit, &mut result);
            return Ok(ItemOrResult::Result(result));
        }
        let decided = match self.inner.frame_init(frame_init)? {
            ItemOrResult::Item(_) => None,
            ItemOrResult::Result(result) => Some(result),
        };
        let ctx = &mut self.inner.ctx;
        match decided {
            None => {
                if keyless {
                    ctx.keyless_frame = Some(keyless::KeylessFrame::Built);
                }
                ctx.additional_limit.open_start(depth);
                let frame = self.inner.frame_stack.get();
                let gas = frame.interpreter.gas.tracker_mut();
                let verdict = ctx.additional_limit.count_start(gas, &start.counted, start.credit);
                ctx.additional_limit.hold_start(verdict);
                #[cfg(debug_assertions)]
                start.assert_made(ctx);
                Ok(ItemOrResult::Item(frame))
            }
            Some(mut result) => {
                debug_assert!(!keyless, "revm builds a keylessDeploy call's frame");
                if let PrecompileHold::Clamped(withheld) = hold {
                    Detention::restore_forward(result.interpreter_result_mut(), withheld);
                }
                let succeeded = result.instruction_result().is_ok();
                // A creation revm answers creates nothing: refused on its creator's account, it
                // bumped nothing, and the record its hook counted on the creator comes back with
                // its answer; failed onto an occupied address, it is held to nothing at its start,
                // and its failure takes its records and transfer log back.
                if let Some(record) = creator.filter(|_| !start.bumped(ctx)) {
                    give_back_creator_record(ctx, &mut result, record);
                }
                ctx.additional_limit.open_start(depth);
                if let FrameResult::Call(_) = result {
                    let verdict = if succeeded {
                        let gas = result.gas_mut().tracker_mut();
                        ctx.additional_limit.count_start(gas, &start.counted, start.credit)
                    } else {
                        ctx.additional_limit.hypothetical_start(&start.counted, start.share)
                    };
                    stop_the_answer_at(ctx, &start, &mut result, verdict);
                }
                settle_answer(ctx, depth, gas_limit, &mut result);
                take_back_a_stopped_answer(ctx, start.mark, succeeded, &result);
                Ok(ItemOrResult::Result(result))
            }
        }
    }

    /// Runs the frame on top of the stack, unless it has a stop to return: its own start's, when
    /// it crossed a limit once revm built the frame, or the one a child left it, when what the
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

    /// Returns the result to the caller as revm does, once its start is closed: a frame that kept
    /// nothing has what the transaction keeps restored to what it kept when its start began, and
    /// one that kept its start gives its caller back what the start's credit is
    /// ([`close_start`](crate::AdditionalLimit)), refilled on the result before revm merges it,
    /// which lands in the caller's pools as a refill after the merge would. The caller is then held
    /// to its limits with what the frame left it ([`AdditionalLimit::hold_caller`](
    /// crate::AdditionalLimit)).
    ///
    /// The creation a `keylessDeploy` call started returns into the call: what the call answers is
    /// read off the creation's result before revm merges it ([`keyless::returning`]), and settled
    /// into the call after ([`keyless::settle`]).
    #[inline]
    fn frame_return_result(
        &mut self,
        mut result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        let stack = &mut self.inner.frame_stack;
        let (depth, outermost) = match stack.index() {
            Some(index) if stack.get().is_finished() => (stack.get().depth, index == 0),
            Some(_) => (stack.get().depth + 1, false),
            None => (0, true),
        };
        let layer = &mut self.inner.ctx.additional_limit;
        let credit = layer.close_start(depth, result.instruction_result().is_ok());
        if credit != 0 {
            result.gas_mut().refill_history(credit);
        }
        layer.take_frame_stop(&result, outermost);
        let creation = keyless::returning(&self.inner.ctx, &mut self.inner.frame_stack, &result);
        let returned = self.inner.frame_return_result(result)?;
        if returned.is_some() {
            return Ok(returned);
        }
        let caller = &mut self.inner.frame_stack.get().interpreter.gas;
        if let Some(creation) = creation {
            keyless::settle(&mut self.inner.ctx, caller, creation)?;
        }
        self.inner.ctx.additional_limit.hold_caller(caller.tracker_mut());
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

    /// revm's inspected frame start: a frame the inspector answers itself never reaches
    /// [`EvmTr::frame_init`], and its answer settles as a frame that never started
    /// ([`answered_by_inspector`]).
    ///
    /// A frame nothing may start, one past the call-stack limit, is answered by
    /// [`EvmTr::frame_init`], after the inspector's `frame_start`, as every frame revm does not
    /// build is, and the inspector is told it ended.
    ///
    /// A keyless deployment is seen as the frames it is made of: the transaction's `keylessDeploy`
    /// call, and the creation it starts, one journal depth below it, as its child. The call's
    /// frame runs no code, so the inspector is not told an interpreter was initialized for it.
    ///
    /// A frame whose start crossed a limit returns the stop before it runs: the inspector sees it
    /// start and end with the stop, and is told neither of an interpreter nor of the logs its
    /// start journaled, which the stop takes back — what it sees of a call answered with the stop.
    #[inline]
    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector) = self.ctx_inspector();
        // What the calling opcode charged upfront, before the inspector's hooks can rewrite the
        // call it charged for.
        let upfront = UpfrontRecord::of(&frame_init.frame_input);
        if let Some(output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            let output = answered_by_inspector(ctx, inspector, &frame_init, output, upfront);
            return Ok(ItemOrResult::Result(output));
        }
        // The start its charge paid for — the calling opcode's, or the transaction's for its own
        // frame — may not be the one the inspector lets start.
        #[cfg(debug_assertions)]
        {
            ctx.charged_records = None;
        }
        let (frame_input, depth) = (frame_init.frame_input.clone(), frame_init.depth);
        // What the start journals begins here. A start answered with a success without a frame
        // is revm's — a call to a precompile or to an account with no code, which takes its
        // checkpoint before it journals anything — or an interceptor's, which journals nothing.
        let start = mark(ctx.journal_mut());
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
            frame_end_checked(ctx, inspector, &frame_input, &mut output, source, upfront);
            return Ok(ItemOrResult::Result(output));
        }
        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        ctx.upfront_records.resize(depth, None);
        ctx.upfront_records.push(upfront);
        // A frame whose start crossed a limit returns the stop before it runs, which takes back
        // what its start journaled, as for a call answered with the stop.
        let stopped = ctx.additional_limit.has_stop_before_run();
        if !stopped && ctx.journal().logs().len() != logs_i {
            inspect_logs(ctx, inspector, logs_i);
        }
        if !keyless::runs(ctx, frame) && !stopped {
            StepGuard(&mut *inspector).initialize_interp(&mut frame.interpreter, ctx);
        }
        Ok(ItemOrResult::Item(frame))
    }

    /// revm's inspected frame run, with the stop short-circuit of [`EvmTr::frame_run`]: a frame
    /// with a stop to return returns it without a step, and the inspector sees it end. A
    /// `keylessDeploy` call's frame makes its actions by hand, as on the plain path, with no step.
    ///
    /// The step callbacks run through [`StepGuard`], as `initialize_interp` does at the frame's
    /// start, so a charge the inspector makes on the interpreter's gas and the frame cannot pay
    /// does not classify the frame's end.
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
            frame_end_checked(ctx, inspector, &frame.input, result, source, upfront);
            frame.set_finished(true);
        }
        next
    }
}

/// The action of a frame about to run: the stop it returns without running an instruction, when
/// it has one ([`stop_before_run`](crate::AdditionalLimit::stop_before_run)); `None` otherwise,
/// and the frame runs.
///
/// A frame runs here for the first time or after a child returned into it. A stop on a first run
/// is the frame's own start, which crossed a limit once revm built the frame: its revert takes
/// back what the start moved. A stop on a resume is the child's doing: the child was a failed
/// creation whose nonce record, which lands on its creator, put the creator over a limit, its
/// budget or the transaction's, and the creator reverts alone, as for a write of its own. A child
/// that crossed a limit itself reverted alone, and its caller resumes.
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
/// for the child's start to add to the transaction's, with the stipend revm gives a value call's
/// child, which the frame did not forward; a frame that returns is classified.
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
    frame: &mut EthFrame<EthInterpreter>,
    next: &mut Result<FrameInitOrResult<EthFrame<EthInterpreter>>, E>,
) {
    match next {
        Ok(ItemOrResult::Item(child)) => {
            let stipend = value_call_stipend(&child.frame_input, ctx.gas_params().call_stipend());
            ctx.detention.on_frame_suspend(&frame.interpreter.gas, frame.depth, stipend);
            cap_the_share(ctx.additional_limit.frame_cap(), &mut frame.interpreter.gas, child);
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

/// Holds the share of its caller's pool the frame `child` starts with to the frame cap, `cap`,
/// and gives what it forwarded above it back to the caller, whose pool `caller` holds: a frame's
/// budget is the schedule's share of what its caller has left, under the cap.
#[inline]
fn cap_the_share(cap: u64, caller: &mut Gas, child: &mut FrameInit) {
    let share = match &child.frame_input {
        FrameInput::Call(inputs) => inputs.bytes_share,
        FrameInput::Create(inputs) => inputs.bytes_share(),
        FrameInput::Empty => return,
    };
    let Some(excess) = share.checked_sub(cap).filter(|excess| *excess != 0) else { return };
    caller.set_bytes_remaining(caller.bytes_remaining().saturating_add(excess));
    match &mut child.frame_input {
        FrameInput::Call(inputs) => inputs.bytes_share = cap,
        FrameInput::Create(inputs) => inputs.set_bytes_share(cap),
        FrameInput::Empty => {}
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
/// cannot fund — runs no precompile, and revm answers it as without the limit: the answer given
/// without running is not given in its place, which a comparison of the caller's balance with the
/// value decides, as revm's transfer does.
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
) -> PrecompileHold {
    if ctx.detention.compute_limit().is_none() {
        return PrecompileHold::Unheld;
    }
    hold_detained_precompile(ctx, precompiles, priced, frame_init)
}

/// [`hold_precompile`] once a read set a compute limit.
#[inline(never)]
fn hold_detained_precompile<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    precompiles: &PrecompilesMap,
    priced: &PricedPrecompiles,
    frame_init: &mut FrameInit,
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
    if price <= allowance || price > inputs.gas_limit {
        return PrecompileHold::Unheld;
    }
    let caller = ctx.journal_ref().state.get(&inputs.caller);
    let unfunded = inputs
        .value
        .transfer()
        .zip(caller)
        .is_some_and(|(value, caller)| caller.info.balance < value);
    if unfunded {
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
pub(crate) const fn input_gas_limit(input: &FrameInput) -> u64 {
    match input {
        FrameInput::Call(inputs) => inputs.gas_limit,
        FrameInput::Create(inputs) => inputs.gas_limit(),
        FrameInput::Empty => 0,
    }
}

/// The stipend revm adds to the gas limit of the frame `input` starts, `call_stipend` for a call
/// that transfers value and nothing for any other: gas the caller never paid.
pub(crate) fn value_call_stipend(input: &FrameInput, call_stipend: u64) -> u64 {
    match input {
        FrameInput::Call(inputs) if inputs.transfers_value() => call_stipend,
        _ => 0,
    }
}

/// Turns a frame that crossed gas detention's compute limit into the compute stop: a revert of
/// that frame alone carrying `MegaLimitExceeded` (kind: compute). The frame's gas is what it had
/// before the charge that crossed, which goes back with the revert to its caller. The caller
/// resumes held to what the limit leaves the transaction, which is less than the charge that
/// crossed, so a caller that carries on is stopped at its next charge past it.
///
/// The crossing charge's size is not kept, so the stop reports the transaction's compute at the
/// crossing as what was used ([`LimitCheck::ExceedsLimit`]): what its regular ledger bills, and
/// the stipends of its value calls on top.
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
}

/// Marks where `journal` stands, without changing its depth: a checkpoint opened and committed at
/// once, which [`revert_to`] takes the journal back to.
pub(crate) fn mark<DB: revm::Database>(journal: &mut Journal<DB>) -> JournalCheckpoint {
    let position = journal.checkpoint();
    journal.checkpoint_commit();
    position
}

/// Takes back every journal entry, log and self-destruct made since `mark`, as revm takes back a
/// frame that fails; the journal's depth is left as it is.
pub(crate) fn revert_to<DB: revm::Database>(journal: &mut Journal<DB>, mark: JournalCheckpoint) {
    let _ = journal.checkpoint();
    journal.checkpoint_revert(mark);
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
        revert_to(ctx.journal_mut(), start);
    }
}

/// Settles `output`, the answer an inspector gave the frame `frame_init` starts in its place, as
/// [`EvmTr::frame_init`] settles an answer: the depth guard still holds, so an answer cannot reach
/// past the call-stack limit, and the answer is held to the compute limit ([`settle_answer`]). Then
/// the inspector is told the frame ended, and the answer settles as a frame that kept nothing: the
/// frame never started, so no value moved and no account was added, and the upfront state gas its
/// caller's opcode was charged comes back whatever the answer ([`frame_end_checked`]).
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
    let mut output = call_too_deep(frame_init).unwrap_or(output);
    let gas_limit = input_gas_limit(&frame_init.frame_input);
    settle_answer(ctx, frame_init.depth, gas_limit, &mut output);
    let input = &frame_init.frame_input;
    frame_end_checked(ctx, inspector, input, &mut output, ResultSource::Inspector, upfront);
    output
}

/// What holding a frame's start to the limits needs of it once revm consumed its input: where the
/// start began, what it counts and what a stop answering it carries.
struct FrameStart {
    /// Where the journal stood before revm decided the start: what revm journals for it — a
    /// creator's nonce bump, the value the start moves and its transfer log, a precompile's
    /// writes — comes after.
    mark: JournalCheckpoint,
    /// The records and transfer log the start makes on its frame, read off the journal before
    /// revm decides it.
    counted: Counted,
    /// What the start's caller gets back once the frame returned with its start kept: what it
    /// paid for the records on the frame, less the price of those the start makes, and the price
    /// of those it takes back.
    credit: u64,
    /// The gas limit the frame's caller forwarded, before gas detention held a precompile to less.
    gas_limit: u64,
    /// The reservoir the frame inherits.
    reservoir: u64,
    /// The share of its caller's pool the frame starts with: its budget.
    share: u64,
    /// For a creation, its creator and the creator's nonce before revm decides the start, which
    /// says whether revm bumped it.
    creator_nonce: Option<(Address, u64)>,
    /// The records the start makes, for a debug build to hold what revm then made to.
    #[cfg(debug_assertions)]
    made: MadeRecords,
}

impl FrameStart {
    /// The start of the frame `frame_init` asks for, about to be decided by revm: where the
    /// journal stands, and the records it makes as the journal's state stands
    /// ([`Accounts::start_records`](crate::limit::Accounts)). A creation's creator record is not
    /// among them: its start's hook counted it on the creator.
    fn of<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        frame_init: &FrameInit,
        creator_counted: bool,
        #[cfg(debug_assertions)] hooked: Option<StartRecords>,
    ) -> Self {
        let accounts = ctx.accounts();
        let (records, charged, reservoir, share, creator_nonce) = match &frame_init.frame_input {
            FrameInput::Call(inputs) => (
                accounts.transfer_records(
                    inputs.caller,
                    inputs.target_address,
                    inputs.value.transfer().unwrap_or_default(),
                ),
                inputs.charged_start.history,
                inputs.reservoir,
                inputs.bytes_share,
                None,
            ),
            FrameInput::Create(inputs) => {
                let nonce = account_nonce(ctx, inputs.caller());
                let created = inputs.created_address(nonce);
                (
                    accounts.creation_records(inputs.caller(), Some(created), inputs.value()),
                    inputs.charged_start().history,
                    inputs.reservoir(),
                    inputs.bytes_share(),
                    Some((inputs.caller(), nonce)),
                )
            }
            FrameInput::Empty => unreachable!(
                "a frame input always names a call or a creation, as revm's frame init asserts"
            ),
        };
        let on_frame = records.on_frame();
        // The hook, or a keyless deployment's start, charged the price of the records it predicted,
        // which a debug build holds to those predicted here unless an inspector may have rewritten
        // the start since.
        #[cfg(debug_assertions)]
        if hooked.is_some() && ctx.prices_history() {
            debug_assert_eq!(
                Some(charged),
                write_record_history_gas(on_frame.added),
                "a start's charge is the price of the records it makes on its frame"
            );
        }
        let credit = if ctx.prices_history() {
            let price = |records| write_record_history_gas(records).unwrap_or(0);
            charged.saturating_sub(price(on_frame.added)).saturating_add(price(on_frame.taken))
        } else {
            0
        };
        #[cfg(debug_assertions)]
        let made = MadeRecords::before(ctx, frame_init, records, creator_counted, hooked);
        #[cfg(not(debug_assertions))]
        let _ = creator_counted;
        Self {
            mark: mark(ctx.journal_mut()),
            counted: records.counted(),
            credit,
            gas_limit: input_gas_limit(&frame_init.frame_input),
            reservoir,
            share,
            creator_nonce,
            #[cfg(debug_assertions)]
            made,
        }
    }

    /// Whether revm bumped the creator's nonce for the creation this start is, which it does
    /// unless it refuses the creation on its creator's account.
    fn bumped<DB: Database, ExtEnvs: ExternalEnvTypes>(
        &self,
        ctx: &MegaContext<DB, ExtEnvs>,
    ) -> bool {
        self.creator_nonce.is_some_and(|(creator, nonce)| account_nonce(ctx, creator) != nonce)
    }

    /// Asserts revm's start made the records the start was charged for and counted, read off
    /// the journal once revm built the frame.
    #[cfg(debug_assertions)]
    fn assert_made<DB: Database, ExtEnvs: ExternalEnvTypes>(&self, ctx: &MegaContext<DB, ExtEnvs>) {
        self.made.assert_after(ctx);
    }
}

/// The accounts a start writes and whether the transaction had changed each before it, for a debug
/// build to hold the records the start was charged for and counted to what revm's start made of
/// them.
#[cfg(debug_assertions)]
#[derive(Debug)]
struct MadeRecords {
    /// The records predicted.
    records: StartRecords,
    /// The accounts the start writes, each with whether it was changed before the start: a call's
    /// sender and recipient, or a creation's created account and creator.
    accounts: [(Address, bool); 2],
    /// Whether the start is a creation.
    creation: bool,
    /// Whether the creator's record was counted on it at the start's hook.
    creator_counted: bool,
}

#[cfg(debug_assertions)]
impl MadeRecords {
    /// The accounts the start `frame_init` asks for writes, read before revm decides it; and,
    /// unless an inspector may rewrite the start between the two, the records the start's hook
    /// charged for, which must be those predicted here.
    fn before<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &MegaContext<DB, ExtEnvs>,
        frame_init: &FrameInit,
        records: StartRecords,
        creator_counted: bool,
        charged: Option<StartRecords>,
    ) -> Self {
        if let Some(charged) = charged {
            debug_assert_eq!(
                charged, records,
                "the records a frame's start makes changed after its caller was charged for them"
            );
        }
        let (addresses, creation) = match &frame_init.frame_input {
            FrameInput::Call(inputs) => ([inputs.caller, inputs.target_address], false),
            FrameInput::Create(inputs) => {
                let nonce = ctx
                    .journal_ref()
                    .state
                    .get(&inputs.caller())
                    .map_or(0, |account| account.info.nonce);
                ([inputs.created_address(nonce), inputs.caller()], true)
            }
            FrameInput::Empty => ([Address::ZERO; 2], false),
        };
        let accounts = ctx.accounts();
        let accounts = addresses.map(|address| (address, accounts.is_changed(address)));
        Self { records, accounts, creation, creator_counted }
    }

    /// Asserts the frame revm built made the records predicted: a call's two sides changed and
    /// returned as its transfer's records say, a creation's created account as its record says,
    /// and its creator, when the hook counted the creator's record, changed by the bump.
    fn assert_after<DB: Database, ExtEnvs: ExternalEnvTypes>(
        &self,
        ctx: &MegaContext<DB, ExtEnvs>,
    ) {
        use crate::limit::RecordDelta;
        let accounts = ctx.accounts();
        let made = |(address, before): (Address, bool)| {
            RecordDelta::of_account(before, accounts.is_changed(address))
        };
        let [first, second] = self.accounts;
        if !self.creation {
            let made = if first.0 == second.0 {
                RecordDelta::NONE
            } else {
                made(first).with(made(second))
            };
            debug_assert_eq!(made, self.records.transfer, "a call's start made its records");
            return;
        }
        let created = RecordDelta::of_account(false, self.records.created);
        debug_assert_eq!(made(first), created, "a creation made its created account's record");
        if self.creator_counted {
            debug_assert_eq!(made(second), RecordDelta::of_account(false, true), "the creator's");
        }
    }
}

/// The nonce of an account the journal holds; zero for one it does not.
fn account_nonce<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    address: Address,
) -> u64 {
    ctx.journal_ref().state.get(&address).map_or(0, |account| account.info.nonce)
}

/// Gives back the record of its own nonce a creation's start hook counted on its creator, for a
/// creation that never bumped the nonce — revm refused it on its creator's account, after an
/// inspector rewrote it — through `answer`, the creation's failure: what the creator paid for the
/// record, in history and in bytes, joins the start charge its failure gives back.
pub(crate) fn give_back_creator_record<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    answer: &mut FrameResult,
    record: CreatorRecord,
) {
    ctx.additional_limit.give_back_creator_record(record);
    if let FrameResult::Create(outcome) = answer {
        let charge = outcome.charged_start;
        outcome.charged_start =
            StartCharge::new(charge.history + record.history, charge.bytes + record.drawn);
    }
}

/// Holds `answer`, revm's or gas detention's answer to the call `start` began, to `verdict`: a
/// crossing is the stop the answer gives way to, on the gas its caller forwarded, untouched, as a
/// frame stopped before it ran ([`stop_the_answer`]). What a successful answer's start moved is
/// taken back with it ([`take_back_a_stopped_answer`]); a failed one's revm took back itself.
fn stop_the_answer_at<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    start: &FrameStart,
    answer: &mut FrameResult,
    verdict: LimitCheck,
) {
    if verdict.exceeded_limit() {
        let output = ctx.additional_limit.note_frame_stop(&verdict);
        stop_the_answer(answer, start, output);
    }
}

/// Rewrites `answer`, to the call `start` began, into the stop `output`: a revert on the gas the
/// call's caller forwarded, untouched, with the reservoir and the pool the call inherited, and no
/// precompile output, as [`synthetic_frame_result`] answers a frame that never started.
#[cold]
#[inline(never)]
fn stop_the_answer(answer: &mut FrameResult, start: &FrameStart, output: Bytes) {
    let FrameResult::Call(outcome) = answer else {
        unreachable!("only a call is held to the limits once revm answered it")
    };
    let mut gas = Gas::new_with_regular_gas_and_reservoir(start.gas_limit, start.reservoir);
    gas.set_bytes_remaining(start.share);
    let allowance = outcome.result.gas.history_allowance();
    gas.set_history_allowance(allowance);
    outcome.result = InterpreterResult::new(InstructionResult::Revert, output, gas);
    outcome.was_precompile_called = false;
    outcome.precompile_call_logs.clear();
}

/// The depth guard: a frame past the call-stack limit, of any call scheme or a creation, answered
/// with `CallTooDeep`, its gas untouched and its reservoir carried — the answer revm gives it.
///
/// revm checks the depth first when it builds a frame, before it moves value or bumps a creator's
/// nonce. The guard gives the same answer before anything else sees the frame: an interceptor or an
/// inspector answers before revm builds anything, so without the guard a system contract could be
/// reached at any depth; and a call's start is held to the limits whatever revm answers but a
/// value its caller cannot fund, so without the guard a call past the limit would be held to them
/// as a start revm made.
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

/// The account writes of the EIP-7702 authorities applied since journal entry `journal_i`: each
/// applied authorization bumps its authority's nonce once, so the distinct authorities other than
/// the sender are the accounts written. The sender's write is part of the transaction body.
fn applied_authorities<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    journal_i: usize,
) -> u64 {
    if ctx.tx().tx_type() != TransactionType::Eip7702 {
        return 0;
    }
    let caller = ctx.tx().caller();
    let mut authorities: Vec<Address> = ctx.journal_ref().journal()[journal_i..]
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::NonceBump { address } if *address != caller => Some(*address),
            _ => None,
        })
        .collect();
    authorities.sort_unstable();
    authorities.dedup();
    // An applied authority that is the block beneficiary wrote the beneficiary's account.
    if authorities.binary_search(&ctx.block().beneficiary).is_ok() {
        ctx.detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
    }
    authorities.len() as u64
}

/// Charges the history of the write records a transaction makes outside any frame: one per
/// applied authority, and the one its own frame makes — the recipient of its value, when the
/// value changes an account the transaction has not changed, or the account it creates.
///
/// The recipient's record is predicted as the journal's state stands before the first frame
/// ([`Accounts::transfer_records`](crate::limit::Accounts)): a recipient that is the sender, or an
/// authority the transaction applied, is changed already, and gets none; the sender's side never
/// makes one, validation having changed its account. A creation's account is charged as a record,
/// as a `CREATE` opcode charges it; the transaction has changed no account but its sender before
/// its first frame, so the account it creates is one it had not changed.
///
/// The authorities' records are the transaction's own and outlive a first frame that fails; the
/// first frame's record does not, so what it cost is kept for that frame to carry as its start
/// charge, which its failure gives back. A first frame revm refuses to start on its sender's
/// account makes no record, and gets the charge back as any frame that kept nothing does: among
/// the transactions that pay history, that is a creation from an account whose nonce cannot be
/// bumped, which only a caller that turned the nonce check off can run, and which revm answers
/// with a success and no address.
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
    let first_frame = match tx.kind() {
        TxKind::Create => 1,
        TxKind::Call(to) => {
            let predicted = ctx.accounts().transfer_records(tx.caller(), to, tx.value()).transfer;
            debug_assert_eq!(predicted.taken, 0, "the sender's value returns no account");
            predicted.added
        }
    };
    let Some((first_frame, _)) =
        charge_write_records_history(gas, first_frame, applied_authorities)
    else {
        return false;
    };
    ctx.additional_limit.set_first_frame_charge(first_frame);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, MegaSpecId};
    use alloy_primitives::{address, U256};
    use revm::{
        context_interface::journaled_state::account::JournaledAccountTr,
        interpreter::{gas::WithheldCrossing, CallInput, CallInputs, CallValue, SharedMemory},
    };
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
            charged_start: Default::default(),
            bytes_share: u64::MAX,
            history_allowance: 0,
        }))
    }

    /// A debug build holds the records a start's hook charged its caller for to those predicted
    /// once revm is about to decide the start: a start whose records changed in between trips,
    /// since its charge and the refund its failure gets back would both be wrong. `CALLER` holds
    /// nothing, so the value call makes no record; its hook was charged for a transfer log.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "changed after its caller was charged for them")]
    fn test_a_start_whose_records_changed_after_its_charge_trips() {
        let mut ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let init = FrameInit { depth: 1, memory: SharedMemory::new(), frame_input: value_call() };
        let charged = StartRecords { transfer_log: true, ..StartRecords::NONE };
        let _ = FrameStart::of(&mut ctx, &init, false, Some(charged));
    }

    /// A debug build holds the history a start declares to the price of the records predicted for
    /// it: a start whose hook charged another price trips.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a start's charge is the price of the records it makes on its frame")]
    fn test_a_start_charged_another_price_trips() {
        let mut ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN);
        let mut frame_input = value_call();
        if let FrameInput::Call(inputs) = &mut frame_input {
            inputs.charged_start = StartCharge::new(1, 0);
        }
        let init = FrameInit { depth: 1, memory: SharedMemory::new(), frame_input };
        let _ = FrameStart::of(&mut ctx, &init, false, Some(StartRecords::NONE));
    }

    /// A context whose transaction read the block environment before its first frame under a cap
    /// of `cap`, so a frame at depth 0 starts with an allowance of `cap`.
    fn detained(cap: u64) -> MegaContext<MemoryDatabase> {
        detained_on(MemoryDatabase::default(), cap)
    }

    /// [`detained`] over `db`.
    fn detained_on(db: MemoryDatabase, cap: u64) -> MegaContext<MemoryDatabase> {
        let mut ctx = MegaContext::new(db, MegaSpecId::SATIN);
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
                charged_start: Default::default(),
                bytes_share: u64::MAX,
                history_allowance: 0,
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
    ) -> (PrecompileHold, u64) {
        hold_input(cap, priced, dispatched, to, &[0; 32], forward)
    }

    /// [`hold`] for a call of `input`.
    fn hold_input(
        cap: u64,
        priced: &PricedPrecompiles,
        dispatched: &PrecompilesMap,
        to: Address,
        input: &[u8],
        forward: u64,
    ) -> (PrecompileHold, u64) {
        let ctx = detained(cap);
        let mut frame_init = static_call(to, input, forward);
        let held = hold_precompile(&ctx, dispatched, priced, &mut frame_init);
        (held, input_gas_limit(&frame_init.frame_input))
    }

    /// A precompile the engine prices is decided from its price before it runs: within the
    /// allowance, and past its whole forward, it runs on the forward as without the limit; past
    /// the allowance and within the forward, it is answered without running, out of gas and
    /// marked as a crossing whose record is the forward. A value call its caller cannot fund,
    /// which revm refuses before it runs the precompile, runs as without the limit.
    #[test]
    fn test_a_priced_precompile_is_decided_from_its_price() {
        const KZG: Address = crate::kzg_point_evaluation::ADDRESS;
        const PRICE: u64 = crate::kzg_point_evaluation::GAS_COST;
        let (priced, map) = (PricedPrecompiles::default(), crate::satin_precompiles_map());

        // Within the allowance: runs on the forward.
        let (held, ran_on) = hold(PRICE, &priced, &map, KZG, 3 * PRICE);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, 3 * PRICE));

        // Past the allowance, within the forward: the crossing, without running.
        for forward in [PRICE, 3 * PRICE] {
            let (held, ran_on) = hold(PRICE - 1, &priced, &map, KZG, forward);
            assert_eq!((held, ran_on), (PrecompileHold::Crossing, forward), "the input as it was");
        }

        // Past the whole forward: a plain out-of-gas, run on the forward.
        let (held, ran_on) = hold(PRICE / 2, &priced, &map, KZG, PRICE - 1);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, PRICE - 1));

        // A value call past the allowance, within the forward: the crossing when its caller can
        // fund the value; revm's refusal, without running the precompile, when it cannot. A
        // `DELEGATECALL` moves nothing, whatever value it carries for its callee to read and
        // whatever its caller holds: the crossing.
        for (scheme, balance, held) in [
            (CallScheme::Call, 1_u64, PrecompileHold::Crossing),
            (CallScheme::Call, 0, PrecompileHold::Unheld),
            (CallScheme::DelegateCall, 0, PrecompileHold::Crossing),
        ] {
            let db = MemoryDatabase::default().account_balance(CALLER, U256::from(balance));
            let mut ctx = detained_on(db, PRICE - 1);
            ctx.journal_mut().load_account(CALLER).unwrap();
            let mut frame_init = static_call(KZG, &[0; 32], 3 * PRICE);
            let FrameInput::Call(inputs) = &mut frame_init.frame_input else { unreachable!() };
            (inputs.scheme, inputs.is_static) = (scheme, false);
            inputs.value = match scheme {
                CallScheme::DelegateCall => CallValue::Apparent(U256::from(1)),
                _ => CallValue::Transfer(U256::from(1)),
            };
            let case = format!("{scheme:?} holding {balance}");
            assert_eq!(hold_precompile(&ctx, &map, &priced, &mut frame_init), held, "{case}");
        }

        // A forward within the allowance, and a call to anything but a precompile, run as they are.
        let (held, ran_on) = hold(3 * PRICE, &priced, &map, KZG, 3 * PRICE);
        assert_eq!((held, ran_on), (PrecompileHold::Unheld, 3 * PRICE));
        let (held, ran_on) = hold(PRICE - 1, &priced, &map, TARGET, 3 * PRICE);
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
            let hold =
                |cap, input: &[u8], forward| hold_input(cap, &priced, &map, to, input, forward);

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
            let (held, ran_on) = hold(1_000, priced, &map, to, 300_000);
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
            let start = mark(ctx.journal_mut());
            // What the answer wrote: a bump of the recipient's nonce.
            let _ = ctx.journal_mut().load_account_mut(TARGET).unwrap().data.bump_nonce();
            let answer = synthetic_frame_result(&value_call(), result, Bytes::new());
            take_back_a_stopped_answer(&mut ctx, start, succeeded, &answer);
            let nonce = ctx.journal_ref().state.get(&TARGET).map_or(0, |a| a.info.nonce);
            assert_eq!(nonce, u64::from(!taken_back), "{case}");
        }
    }

    /// One handler runs one transaction, and what a phase learns about it is kept on the handler:
    /// the reservoir the transaction settled with is read where revm builds its result's gas and
    /// reported once the result is built. A handler reused for a system call, whose path builds
    /// no result gas and skips that read, reports the system call's own reservoir: the capture is
    /// consumed by the report, and by the error path, which gives no report. A transaction below
    /// the execution cap leaves a capture of nothing, one above it a capture of its own margin, and
    /// a deposit that halts a capture its failed-deposit result never reports; after each, a
    /// system call on revm's default gas limit reports the margin above the system-call budget.
    #[test]
    fn test_a_reused_handler_reports_each_executions_own_reservoir() {
        use crate::{
            constants::TX_GAS_LIMIT_CAP,
            test_utils::{op_transaction, zero_fee_l1_block_info},
            MegaTransaction, SYSTEM_CALL_REGULAR_GAS_LIMIT,
        };
        use alloy_op_evm::OpTx;
        use revm::{
            context::{ContextSetters, TxEnv},
            handler::{system_call::SYSTEM_CALL_GAS_LIMIT, SystemCallTx},
            inspector::NoOpInspector,
        };
        const HALTS: Address = address!("00000000000000000000000000000000000e0003");
        const DEFAULT_RESERVOIR: u64 = SYSTEM_CALL_GAS_LIMIT - SYSTEM_CALL_REGULAR_GAS_LIMIT;

        let db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_code(TARGET, Bytes::new())
            .account_code(HALTS, Bytes::from_static(&[revm::bytecode::opcode::INVALID]));
        let mut evm = MegaEvm::new(
            MegaContext::new(db, MegaSpecId::SATIN).with_chain(zero_fee_l1_block_info()),
        );
        let mut handler = MegaHandler::<
            MegaEvm<MemoryDatabase, NoOpInspector>,
            crate::MegaEvmError<core::convert::Infallible>,
            EthFrame<EthInterpreter>,
        >::new();
        let room = 100_000 + history_gas(crate::TX_BODY_SIZE).unwrap();
        let tx = |to: Address, gas_limit: u64, nonce: u64| {
            OpTx(op_transaction(TxEnv {
                caller: CALLER,
                nonce,
                gas_limit,
                kind: TxKind::Call(to),
                ..Default::default()
            }))
        };
        let mut deposit = tx(HALTS, room, 0);
        deposit.0.deposit.source_hash = alloy_primitives::B256::repeat_byte(0x11);
        deposit.0.base.gas_price = 0;

        for (case, tx, succeeds) in [
            ("below the cap", tx(TARGET, room, 0), true),
            ("above the cap", tx(TARGET, TX_GAS_LIMIT_CAP + 10_000_000, 1), true),
            ("a halting deposit", deposit, false),
        ] {
            evm.inner.ctx.set_tx(tx);
            let result = handler.run(&mut evm).expect(case);
            assert_eq!(result.is_success(), succeeds, "{case}: {result:?}");
            let settled = evm.ctx().additional_limit().reservoir_remaining();
            assert_ne!(settled, DEFAULT_RESERVOIR, "{case}: the capture would show");

            let call = MegaTransaction::new_system_tx_with_caller(CALLER, TARGET, Bytes::new());
            evm.inner.ctx.set_tx(call);
            evm.inner.ctx.on_new_system_call();
            let result = handler.run_system_call(&mut evm).expect("the system call runs");
            assert!(result.is_success(), "{case}: {result:?}");
            assert_eq!(
                evm.ctx().additional_limit().reservoir_remaining(),
                DEFAULT_RESERVOIR,
                "{case}: the system call's own reservoir, not the capture"
            );
        }
    }
}
