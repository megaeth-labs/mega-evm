//! The Satin handler and the frame lifecycle of [`MegaEvm`].
//!
//! [`MegaHandler`] runs a transaction through op-revm's [`OpHandler`] and overrides the phases
//! `MegaETH` extends. [`MegaEvm`] implements revm's [`EvmTr`] itself, so a frame's start and its
//! return to its caller (`frame_init`, `frame_return_result`) are `MegaETH`'s own; its frame run is
//! revm's, but for an action set on a frame before it runs, which it returns as revm's inspected
//! run does. Its [`InspectorEvmTr`] is revm's inspected lifecycle unchanged, which starts and
//! returns frames through the same two methods.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::num::NonZeroU64;

use op_revm::{
    handler::{IsTxError, OpHandler},
    transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
    OpHaltReason, OpTransactionError,
};
use std::{collections::BTreeMap, vec::Vec};

use alloy_evm::{precompiles::PrecompilesMap, Database};
use revm::{
    context::{
        result::{FromStringError, InvalidTransaction, ResultGas},
        transaction::TransactionType,
        ContextError, ContextTr, FrameStack, Journal, JournalTr, Transaction,
    },
    context_interface::{
        cfg::{
            gas::{GasTracker, PoolAmounts, PoolId, StartCharge},
            GasId, GasParams, StateGasCharge, StateGasSite,
        },
        journaled_state::{entry::JournalEntry, JournalCheckpoint},
        Host,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult, FrameTr},
        execution::runtime_oog_unwind,
        pre_execution::Eip7702AuthFacts,
        EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, Handler, ItemOrResult,
        PreExecutionOutput,
    },
    inspector::{InspectorEvmTr, InspectorHandler, JournalExt},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, interpreter_types::LoopControl,
        CallScheme, FrameInput, Gas, InitialAndFloorGas, InstructionResult, InterpreterAction,
        InterpreterResult,
    },
    primitives::{Address, Bytes, TxKind, CALL_STACK_LIMIT},
    Inspector,
};

use crate::{
    access::ComputeStop,
    charge_write_records_history,
    limit::{CreatorBefore, StartBefore},
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
/// It keeps nothing of the transaction it runs: what a phase learns is kept on the context.
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
    /// records cross it here. The KV-update and state-growth limits can be crossed here, by the
    /// authorization list: a list that would cross either is not applied, and the transaction's
    /// own frame is stopped before it runs.
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
        if deposit_created_caller(evm.ctx()) {
            evm.ctx_mut().additional_limit.count_created_caller();
            if !charge_created_caller(evm.ctx_mut(), gas) {
                runtime_oog_unwind(evm.ctx(), checkpoint)?;
                return Ok(None);
            }
        }
        // The authorization list is decided before it is applied: a list that would keep more
        // records or create more accounts than the transaction's limits allow is skipped whole,
        // charged nothing, and the transaction's own frame returns the stop before it runs.
        let stop = hold_authorization_list(evm.ctx_mut())?;
        let eip7702_refund = if let Some(stop) = stop {
            evm.ctx_mut().additional_limit.skip_authorization_list(stop);
            0
        } else {
            let Some(eip7702_refund) = self.apply_eip7702_auth_list(evm, gas)? else {
                runtime_oog_unwind(evm.ctx(), checkpoint)?;
                return Ok(None);
            };
            eip7702_refund
        };
        let (applied, created) = applied_authorities(evm.ctx_mut(), checkpoint.journal_i);
        if !charge_records_made_outside_a_frame(evm.ctx_mut(), gas, applied) {
            runtime_oog_unwind(evm.ctx(), checkpoint)?;
            return Ok(None);
        }
        evm.ctx_mut().additional_limit.count_authorities(applied, created);
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

    /// Deducts the caller as op-revm does.
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
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
        // A byte count with no price saturates, which no gas limit covers: the transaction is
        // rejected for not covering its own intrinsic gas.
        let history = evm.ctx_ref().intrinsic_history_gas();
        let gas_limit = evm.ctx_ref().tx().gas_limit();
        let initial_gas = gas
            .initial_regular_gas()
            .saturating_add(gas.initial_state_gas_final().saturating_add(history));
        if initial_gas > gas_limit {
            return Err(
                InvalidTransaction::CallGasCostMoreThanGasLimit { gas_limit, initial_gas }.into()
            );
        }
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
        let history = evm.ctx_ref().intrinsic_history_gas();
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
    /// charged before execution for the write record it makes, priced again here on the journal
    /// as pre-execution left it ([`first_frame_record_charge`]); with a read of the block
    /// beneficiary's account marked for gas detention when the transaction's recipient delegates
    /// to the beneficiary: revm resolves that delegate through the journal, where the Host does
    /// not see it.
    ///
    /// The pools are what the transaction has left of its data-size, KV-update and state-growth
    /// limits once its body, the records of the authorities it applied, the accounts they created
    /// and a deposit's created caller are counted, the data size under the frame cap
    /// ([`AdditionalLimit::first_frame_pools`](crate::AdditionalLimit)); revm hands the frame the
    /// whole of each.
    fn first_frame_input(
        &mut self,
        evm: &mut Self::Evm,
        gas: &mut GasTracker,
    ) -> Result<Option<FrameInit>, Self::Error> {
        // Priced before revm's runtime gas phase loads the recipient: on the journal the charge
        // was made on.
        let charge = StartCharge::new(first_frame_record_charge(evm.ctx_ref()), PoolAmounts::ZERO);
        gas.set_pools_remaining(evm.ctx_ref().additional_limit.first_frame_pools());
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

    /// revm's post-execution, with the reservoir the transaction leaves unspent reported first
    /// ([`AdditionalLimit::reservoir_remaining`](crate::AdditionalLimit::reservoir_remaining)),
    /// where revm builds the result's gas: before the EIP-7623 floor check, which absorbs the
    /// reservoir into the floor's cost when the floor binds. The refund before it does not move
    /// the reservoir.
    fn post_execution(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<ResultGas, Self::Error> {
        let reservoir = exec_result.gas().reservoir();
        evm.ctx_mut().additional_limit.set_reservoir_remaining(reservoir);
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

    /// op-revm's result; for a system call, with the reservoir it settled with reported
    /// ([`AdditionalLimit::reservoir_remaining`](crate::AdditionalLimit::reservoir_remaining)).
    ///
    /// revm's system-call path settles without [`post_execution`](Handler::post_execution), which
    /// reports a transaction's: the result it hands over here is the settled one. A deposit that
    /// halts is not a result: op-revm turns it into a failed deposit, which leaves no reservoir
    /// ([`catch_error`](Handler::catch_error)).
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: FrameResult,
        result_gas: revm::context::result::ResultGas,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error> {
        let system_call = evm.ctx_ref().is_system_call().then(|| result.gas().reservoir());
        let result = self.op.execution_result(evm, result, result_gas)?;
        if let Some(reservoir) = system_call {
            evm.ctx_mut().additional_limit.set_reservoir_remaining(reservoir);
        }
        Ok(result)
    }

    /// op-revm's error handling, which answers a deposit's transaction error with a failed-deposit
    /// halt and discards everything the deposit did. The halt is what the transaction reports;
    /// what the deposit counted stands, the body and the Oracle hints it forwarded before it
    /// halted, as for any transaction that halts. Any other error refuses the transaction, and the
    /// layer's state is reset by the next one. A reservoir reported for a result that op-revm then
    /// refused is taken back here: a failed deposit leaves none, nor does an error.
    ///
    /// A failed deposit still bumps its caller's nonce and mints, so one whose caller had no
    /// account creates it: one account of state growth, which the transaction reports and its
    /// block counts, though it pays no state gas for it and no limit stops a deposit that already
    /// failed.
    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<revm::context::result::ExecutionResult<Self::HaltReason>, Self::Error> {
        evm.ctx_mut().additional_limit.set_reservoir_remaining(0);
        let result = self.op.catch_error(evm, error);
        let failed_deposit = matches!(
            result,
            Ok(revm::context::result::ExecutionResult::Halt {
                reason: OpHaltReason::FailedDeposit,
                ..
            })
        );
        if failed_deposit && deposit_created_caller(evm.ctx()) {
            evm.ctx_mut().additional_limit.count_created_caller();
        }
        result
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
    /// 1. the caller the start suspends, the frame on top of the stack, is settled for it
    ///    ([`suspend`]): gas detention keeps its compute for the frame to add to the transaction's,
    ///    and the share of its pool the frame starts with is held to the frame cap;
    /// 2. the depth guard: a frame past the call-stack limit is answered with `CallTooDeep`, as
    ///    revm answers it, before anything could intercept it or count its start;
    /// 3. the keyless dispatch ([`keyless::is_dispatched`]): a `keylessDeploy` call a transaction
    ///    makes is answered when it carries value, and otherwise readied for revm to build as the
    ///    frame whose actions are made by hand ([`keyless::ready`]);
    /// 4. system contract interception ([`MegaEvm::intercept`]), for any other frame, which answers
    ///    the frame or lets it start;
    /// 5. where the journal stands is marked, and the accounts the start writes on its frame and a
    ///    creation's creator are read as they stand ([`FrameStart::of`]);
    /// 6. revm builds the frame, or answers it, and the start is held to the limits on the pool its
    ///    frame starts with, where its records and transfer log are drawn
    ///    ([`AdditionalLimit::count_start`](crate::AdditionalLimit)): a frame revm built whose
    ///    start crossed a limit ends with the stop before it runs an instruction ([`stop_action`],
    ///    [`end_before_it_runs`]); a success revm answered gives way to the stop
    ///    ([`stop_the_answer`]), what its start moved taken back to the mark, as revm takes back a
    ///    frame that fails ([`take_back_a_stopped_answer`]). An answer that failed — a start revm
    ///    refuses on its caller's account, a precompile that fails, a creation revm answers — kept
    ///    nothing, revm having taken back what it moved, and is held to nothing: its result is
    ///    revm's. A creation's creator is handed to its return, which counts the record of the
    ///    nonce revm bumped ([`frame_return_result`](EvmTr::frame_return_result)).
    ///
    /// A frame revm built is held to the compute limit for its first run (gas detention's
    /// [`on_frame_run`](Detention::on_frame_run)). A `keylessDeploy` call's frame runs no
    /// instruction: its first action is made here ([`keyless::start`]). A refusal ends the frame
    /// before it runs, as the stop does; the creation is set on its interpreter, which returns it
    /// when the frame runs, and starts it through this same frame start.
    ///
    /// A frame answered at step 3, 4 or 6 — a `keylessDeploy` call carrying value, an
    /// interceptor's answer, a precompile's, revm's for a call it did not start — is held to the
    /// compute limit, as a frame that ran would be ([`settle_answer`]). A precompile, which revm
    /// runs at step 6, is decided from its price before it runs when the engine can price it, and
    /// answered without running when that price crosses the compute limit, before revm moves
    /// anything for it, so it is held to the price alone; one it cannot price is run on the gas
    /// the limit leaves the frame rather than on all its caller forwarded ([`hold_precompile`]).
    ///
    /// Step 2 is the pre-frame check: it answers a frame nothing may start. No limit has a part in
    /// it: validation admits no transaction a limit would stop before its first frame, and a start
    /// is held to the limits with the writes it makes, at step 6. The frame's own writes are
    /// counted after the interceptor, because an intercepted frame's writes are the interceptor's
    /// to count. A `keylessDeploy` call's frame is built as any call's, and its creation is started
    /// by its first action, through this same frame start, as its child.
    ///
    /// revm's inspected frame start calls this one once the inspector's `frame_start` let the
    /// frame start; a frame the inspector answers itself never reaches it, and its answer is taken
    /// as given.
    #[inline]
    fn frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        // The frame on top of the stack, if any, is the caller, which suspends on this start.
        if self.inner.frame_stack.index().is_some() {
            suspend(&mut self.inner.ctx, self.inner.frame_stack.get(), &mut frame_init);
        }
        if let Some(result) = call_too_deep(&frame_init) {
            return Ok(ItemOrResult::Result(result));
        }
        if frame_init.depth == 0 {
            if let Some(mut result) = stop_before_the_first_frame(&mut self.inner.ctx, &frame_init)
            {
                let gas_limit = input_gas_limit(&frame_init.frame_input);
                settle_answer(&mut self.inner.ctx, 0, gas_limit, &mut result);
                return Ok(ItemOrResult::Result(result));
            }
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
        let start = FrameStart::of(ctx, &frame_init)?;
        let hold = hold_precompile(
            ctx,
            &self.inner.precompiles,
            &self.priced_precompiles,
            &mut frame_init,
        );
        if hold == PrecompileHold::Crossing {
            // revm never decides the start, so nothing moved and nothing is counted: the answer is
            // held to the price alone, which its settlement turns into the stop. A creation is
            // never a precompile's.
            let mut result = crossing_answer(&frame_init.frame_input);
            settle_answer(ctx, depth, gas_limit, &mut result);
            return Ok(ItemOrResult::Result(result));
        }
        let decided = match self.inner.frame_init(frame_init)? {
            ItemOrResult::Item(_) => None,
            ItemOrResult::Result(result) => Some(result),
        };
        let ctx = &mut self.inner.ctx;
        let Some(mut result) = decided else {
            ctx.additional_limit.open_start(depth, start.creator);
            let frame = self.inner.frame_stack.get();
            let verdict = start.count(ctx, frame.interpreter.gas.tracker_mut());
            ctx.detention.on_frame_run(&mut frame.interpreter.gas, depth);
            let first = if verdict.exceeded_limit() {
                Some(stop_action(ctx, frame, &verdict))
            } else if keyless {
                Some(keyless::start(ctx, frame)?)
            } else {
                None
            };
            match first {
                Some(action @ InterpreterAction::Return(_)) => {
                    let result = end_before_it_runs(ctx, frame, action)?;
                    self.inner.frame_stack.pop();
                    return Ok(ItemOrResult::Result(result));
                }
                Some(action) => frame.interpreter.bytecode.set_action(action),
                None => {}
            }
            return Ok(ItemOrResult::Item(self.inner.frame_stack.get()));
        };
        debug_assert!(!keyless, "revm builds a keylessDeploy call's frame");
        if let PrecompileHold::Clamped(withheld) = hold {
            Detention::restore_forward(result.interpreter_result_mut(), withheld);
        }
        let succeeded = result.instruction_result().is_ok();
        // An answer that failed kept nothing: revm took back what its start moved — a value its
        // caller could not fund, a precompile's failure — or, for a creation, what it made at an
        // occupied address. It is held to nothing, and its result is revm's. A creation revm
        // answers creates nothing; its creator's record, if revm bumped the nonce, is counted on
        // the creator as the answer returns, as a built creation's is. A call revm answered with a
        // success is held to the records its start made, which revm committed.
        ctx.additional_limit.open_start(depth, start.creator);
        if succeeded && matches!(result, FrameResult::Call(_)) {
            let verdict = start.count(ctx, result.gas_mut().tracker_mut());
            stop_the_answer_at(ctx, &start, &mut result, verdict);
        }
        settle_answer(ctx, depth, gas_limit, &mut result);
        take_back_a_stopped_answer(ctx, start.mark, succeeded, &result);
        Ok(ItemOrResult::Result(result))
    }

    /// revm's frame run. The actions the engine makes for a frame outside an instruction — the
    /// creation a `keylessDeploy` call starts and the call's answer ([`EvmTr::frame_init`],
    /// [`EvmTr::frame_return_result`]), and the stop of a frame a child returned into over a limit
    /// ([`EvmTr::frame_return_result`]) — are set on the frame's interpreter, and revm's run, plain
    /// or inspected, returns an action set before it runs without running an instruction or calling
    /// the inspector's `step`.
    #[inline]
    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    /// Classifies the result of the frame on top of the stack, which ran and returns, for gas
    /// detention ([`end_frame`]). revm's loops, plain and inspected, hand it here once the frame's
    /// last action was processed — `return_create` included — and before anything else sees it:
    /// the inspector's `call_end`, `create_end` and `frame_end`, then
    /// [`frame_return_result`](EvmTr::frame_return_result) and the caller or, for the transaction's
    /// own frame, [`last_frame_result`](Handler::last_frame_result). So a frame that crossed the
    /// compute limit ends with the stop for all of them, an inspector included.
    ///
    /// A result [`frame_init`](EvmTr::frame_init) returns does not come here: a frame that ends
    /// before it runs is classified where it ends ([`end_before_it_runs`]), and an answer is held
    /// to the limit by [`settle_answer`].
    #[inline]
    fn frame_run_result(
        &mut self,
        result: &mut FrameResult,
    ) -> Result<(), ContextDbError<Self::Context>> {
        let depth = self.inner.frame_stack.get().depth;
        end_frame(&mut self.inner.ctx, result, depth);
        Ok(())
    }

    /// Returns the result to the caller as revm does, in this order:
    ///
    /// 1. its start is closed: a frame that kept nothing has what the transaction keeps restored to
    ///    what it kept when its start began, and one that kept its start gives its caller back what
    ///    the start's credit is ([`close_start`](crate::AdditionalLimit)), refilled on the result
    ///    before revm merges it, which lands in the caller's pools as a refill after the merge
    ///    would;
    /// 2. revm merges the result into the caller; a creation's creator is then counted the record
    ///    of the nonce revm bumped for it, held to its budget and then to the transaction's limit
    ///    ([`hold_creator`]), the caller is held to the transaction's limits with what the frames
    ///    keep after the return — a failure restores what they kept at its start, which a hint sent
    ///    meanwhile may leave above a limit
    ///    ([`AdditionalLimit::hold_caller`](crate::AdditionalLimit)) — and, by gas detention, to
    ///    the compute limit for its resume ([`on_frame_run`](Detention::on_frame_run)): a caller
    ///    over a limit returns the stop without running another instruction, set on its interpreter
    ///    for its run to return.
    ///
    /// The creation a `keylessDeploy` call started returns into the call: what the call answers is
    /// read off the creation's result before revm merges it ([`keyless::returning`]), settled into
    /// the call after, the signer's nonce record counted there ([`keyless::settle`]), and set on
    /// the call's interpreter, unless it has a stop to return ([`keyless::answer`]).
    ///
    /// A frame that ran was classified when its run returned
    /// ([`frame_run_result`](EvmTr::frame_run_result)); what arrives here is the result the caller
    /// settles, an inspector's rewrite included, which is taken as given.
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
        let (credit, creator) = layer.close_start(depth, result.instruction_result().is_ok());
        if credit != 0 {
            result.gas_mut().refill_history(credit);
        }
        layer.take_frame_stop(&result, outermost);
        let creation = keyless::returning(&self.inner.ctx, &mut self.inner.frame_stack, &result);
        let returned = self.inner.frame_return_result(result)?;
        if returned.is_some() {
            return Ok(returned);
        }
        let ctx = &mut self.inner.ctx;
        let caller = self.inner.frame_stack.get();
        let (stop, deployment) = match creation {
            Some(creation) => {
                let gas = &mut caller.interpreter.gas;
                let (deployment, stop) = keyless::settle(ctx, gas, creation, creator)?;
                (stop, Some(deployment))
            }
            None => {
                let bumped = creator.filter(|creator| ctx.accounts().bumped(creator));
                let gas = caller.interpreter.gas.tracker_mut();
                (bumped.and_then(|creator| hold_creator(ctx, gas, &creator)), None)
            }
        };
        let stop = stop.or_else(|| ctx.additional_limit.hold_caller());
        ctx.detention.on_frame_run(&mut caller.interpreter.gas, caller.depth);
        let action = match (stop, deployment) {
            (Some(stop), _) => stop_action(ctx, caller, &stop),
            (None, Some(deployment)) => keyless::answer(ctx, &deployment, caller.interpreter.gas),
            (None, None) => return Ok(None),
        };
        caller.interpreter.bytecode.set_action(action);
        Ok(None)
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
}

/// Settles `caller`, the frame on top of the stack, which suspends to start `child`: gas detention
/// keeps its compute for the child's start to add to the transaction's, with the stipend revm
/// gives a value call's child, which the caller did not forward
/// ([`on_frame_suspend`](Detention::on_frame_suspend)), and the share of its pool the child starts
/// with is held to the frame cap ([`cap_the_shares`]).
///
/// It runs where the start reaches [`EvmTr::frame_init`]: under an inspector, after the
/// inspector's `frame_start` saw the start. A start the inspector answers itself settles nothing:
/// the caller resumes on the answer as its opcode left it.
#[inline]
fn suspend<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    caller: &mut EthFrame<EthInterpreter>,
    child: &mut FrameInit,
) {
    let stipend = value_call_stipend(&child.frame_input, ctx.gas_params().call_stipend());
    ctx.detention.on_frame_suspend(&caller.interpreter.gas, caller.depth, stipend);
    let caps = ctx.additional_limit.frame_caps();
    cap_the_shares(ctx.gas_params(), caps, &mut caller.interpreter.gas, child);
}

/// The action of `frame` that stops it at the limit `verdict` names before it runs another
/// instruction: a revert carrying `MegaLimitExceeded` on the gas the frame holds. A frame revm just
/// built whose start crossed the limit returns it before its first instruction: its journal
/// checkpoint takes back what the start moved and its transfer log, and a creation keeps its
/// creator's nonce bump, which revm makes before the checkpoint, its record counted on the creator
/// once the creation returned. A creator that record put over a limit returns it on its resume.
#[cold]
#[inline(never)]
fn stop_action<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &EthFrame<EthInterpreter>,
    verdict: &LimitCheck,
) -> InterpreterAction {
    let output = ctx.additional_limit.note_frame_stop(verdict);
    InterpreterAction::new_return(InstructionResult::Revert, output, frame.interpreter.gas)
}

/// Ends `frame`, which revm just built, with `action`, the return made for it before it runs an
/// instruction — the stop its start crossed, or a `keylessDeploy` call's refusal — as revm's
/// frame run ends a frame that returns: revm settles the frame's journal checkpoint and builds its
/// result, which gas detention then classifies ([`end_frame`]).
///
/// The frame never ran an instruction, so it is returned as an answer is, off the stack: its
/// caller resumes on it, and an inspector sees it start and end, and neither an interpreter nor
/// what its start journaled when the return reverts it.
#[cold]
#[inline(never)]
fn end_before_it_runs<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &mut EthFrame<EthInterpreter>,
    action: InterpreterAction,
) -> Result<FrameResult, ContextDbError<MegaContext<DB, ExtEnvs>>> {
    let next =
        frame.process_next_action::<_, ContextDbError<MegaContext<DB, ExtEnvs>>>(ctx, action);
    let ItemOrResult::Result(mut result) = next? else {
        unreachable!("a frame that returns has a result")
    };
    end_frame(ctx, &mut result, frame.depth);
    Ok(result)
}

/// Classifies `result`, the result of a frame at `depth` that ran, for gas detention, once revm
/// processed the frame's last action — `return_create` included, whose deposit and hash charges
/// are the creating frame's own compute.
///
/// A frame that ran out of gas on a charge the withheld part of its gas would have paid crossed
/// the compute limit rather than its own gas: its halt becomes the compute stop, a revert of that
/// frame alone ([`stop_at_the_compute_limit`]). Every other out-of-gas halts, as it would without
/// the read.
#[inline]
fn end_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    result: &mut FrameResult,
    depth: usize,
) {
    let instruction_result = result.instruction_result();
    if let Some(stop) = ctx.detention.on_frame_end(instruction_result, result.gas_mut(), depth) {
        stop_at_the_compute_limit(ctx, result.interpreter_result_mut(), stop);
    }
}

/// Counts on a creator whose pools `gas` holds, once a creation it started returned into it, what
/// the nonce bump revm made for the creation made of it ([`Accounts::creator_counted`]) — `before`
/// is the creator as it stood before revm decided the creation — and answers the stop the creator
/// returns before it runs on, if the count crossed a limit: the record is held to the creator's
/// budget, then to the transaction's limit, as any count the creator makes
/// ([`draw`](crate::AdditionalLimit)). The creation's budget was taken before the record, at its
/// opcode, so the record is the creator's alone, and it is counted whether the creation succeeded
/// or not, since the bump outlives it.
///
/// [`Accounts::creator_counted`]: crate::limit::Accounts
pub(crate) fn hold_creator<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    gas: &mut GasTracker,
    before: &CreatorBefore,
) -> Option<LimitCheck> {
    let counted = ctx.accounts().creator_counted(before);
    let check = ctx.additional_limit.draw(gas, &counted);
    check.exceeded_limit().then_some(check)
}

/// Holds the share of each of its caller's pools the frame `child` starts with to the share of what
/// the caller has left, its remaining clamped to its budget, and to that pool's frame cap in
/// `caps`, and gives what it forwarded above that back to the caller, whose pools `caller` holds.
///
/// A frame's remaining is `min(budget, max(0, budget − net))`: a frame whose net is negative — it
/// took back a record or removed state a frame above it made — holds more in its pool than the
/// budget it was given, and the share it forwards is of that budget, not of the pool, so a negative
/// net never enlarges a child's budget. The fork forwarded the schedule's share of the pool the
/// opcode found ([`GasParams::pool_forward_share`]); the caller had that pool and the share
/// together, and its budget is that less its net draw.
#[inline]
fn cap_the_shares(params: &GasParams, caps: PoolAmounts, caller: &mut Gas, child: &mut FrameInit) {
    let mut shares = match &child.frame_input {
        FrameInput::Call(inputs) => inputs.pool_shares,
        FrameInput::Create(inputs) => inputs.pool_shares(),
        FrameInput::Empty => return,
    };
    let mut capped = false;
    for pool in PoolId::ALL {
        let share = shares.get(pool);
        let mut allowed = caps.get(pool);
        let spent = caller.pool_spent(pool);
        if spent < 0 {
            let had = caller.pool_remaining(pool).saturating_add(share);
            let budget = had.saturating_add_signed(spent);
            allowed = allowed.min(params.pool_forward_share(pool, budget));
        }
        let Some(excess) = share.checked_sub(allowed).filter(|excess| *excess != 0) else {
            continue;
        };
        caller.set_pool_remaining(pool, caller.pool_remaining(pool).saturating_add(excess));
        shares.set(pool, allowed);
        capped = true;
    }
    if !capped {
        return;
    }
    match &mut child.frame_input {
        FrameInput::Call(inputs) => inputs.pool_shares = shares,
        FrameInput::Create(inputs) => inputs.set_pool_shares(shares),
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

/// What holding a frame's start to the limits needs of it once revm consumed its input: where the
/// start began, the accounts it writes as they stood before it, what its caller paid for it and
/// what a stop answering it carries.
struct FrameStart {
    /// Where the journal stood before revm decided the start: what revm journals for it — a
    /// creator's nonce bump, the value the start moves and its transfer log, a precompile's
    /// writes — comes after.
    mark: JournalCheckpoint,
    /// The accounts the start writes on its frame as the transaction held them before revm decided
    /// it, and the logs the journal held: what the start made is read off the journal against them
    /// once revm decided it ([`count`](Self::count)).
    before: StartBefore,
    /// The history gas the start's caller paid at its opcode for the records on the frame.
    charged: u64,
    /// The gas limit the frame's caller forwarded, before gas detention held a precompile to less.
    gas_limit: u64,
    /// The reservoir the frame inherits.
    reservoir: u64,
    /// The shares of its caller's pools the frame starts with: its budgets.
    shares: PoolAmounts,
    /// For a creation a frame makes, its creator as it stands before revm decides the start, which
    /// tells once the creation returned what revm's nonce bump made of it. The transaction's own
    /// creation has none: its sender's account is the transaction body's.
    creator: Option<CreatorBefore>,
}

impl FrameStart {
    /// The start of the frame `frame_init` asks for, about to be decided by revm: where the
    /// journal stands, the accounts the start writes on its frame as the transaction holds them
    /// ([`Accounts::start_before`]) — a call's caller and target, a creation's created account —
    /// and a creation's creator. A creation's creator is not among the accounts: its nonce record
    /// is the creator's, counted from the bump once the creation returned.
    ///
    /// [`Accounts::start_before`]: crate::limit::Accounts
    fn of<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        frame_init: &FrameInit,
    ) -> Result<Self, ContextError<DB::Error>> {
        let mut creator = None;
        let (account, other, charged, reservoir, shares) = match &frame_init.frame_input {
            FrameInput::Call(inputs) => (
                inputs.caller,
                Some(inputs.target_address),
                inputs.charged_start.history,
                inputs.reservoir,
                inputs.pool_shares,
            ),
            FrameInput::Create(inputs) => {
                // A creation's creator and address are loaded before revm decides the start, as
                // revm loads them itself, outside the creation's checkpoint, once it decides it:
                // the creator first thing, the address once the creator funds the endowment and
                // its nonce bumps. The `CREATE` opcode loaded the address already unless EIP-8037
                // is off, and the creator is the frame that runs it, so only an inspector's
                // rewrite, or EIP-8037 off, makes a load here that revm would not have made yet;
                // neither changes what revm decides. An address an earlier transaction on the same
                // journal left there is not loaded until it is again.
                let info = &ctx.journal_mut().load_account(inputs.caller())?.data.info;
                let bumps = info.balance >= inputs.value() && info.nonce != u64::MAX;
                let created = inputs.created_address(info.nonce);
                if bumps && !ctx.accounts().is_loaded(created) {
                    ctx.journal_mut().load_account(created)?;
                }
                if frame_init.depth > 0 {
                    creator = Some(ctx.accounts().creator_before(inputs.caller()));
                }
                (
                    created,
                    None,
                    inputs.charged_start().history,
                    inputs.reservoir(),
                    inputs.pool_shares(),
                )
            }
            FrameInput::Empty => unreachable!(
                "a frame input always names a call or a creation, as revm's frame init asserts"
            ),
        };
        let mark = mark(ctx.journal_mut());
        Ok(Self {
            before: ctx.accounts().start_before(account, other, mark.log_i),
            mark,
            charged,
            gas_limit: input_gas_limit(&frame_init.frame_input),
            reservoir,
            shares,
            creator,
        })
    }

    /// Holds the start, which revm just decided, to the limits on the pools of its frame, which
    /// `gas` holds, and counts there what it made, read off the journal — its records and their
    /// bytes, its state growth and its transfer log ([`Accounts::start_made`]) — with what its
    /// caller gets back once the frame returned with its start kept: what it paid for the records
    /// on the frame, less the price of those the start made, and the price of those it took back
    /// ([`AdditionalLimit::count_start`](crate::AdditionalLimit)).
    ///
    /// [`Accounts::start_made`]: crate::limit::Accounts
    fn count<DB: Database, ExtEnvs: ExternalEnvTypes>(
        &self,
        ctx: &mut MegaContext<DB, ExtEnvs>,
        gas: &mut GasTracker,
    ) -> LimitCheck {
        let (records, counted) = ctx.accounts().start_made(&self.before);
        let credit = if ctx.prices_history() {
            let price = |records| write_record_history_gas(records).unwrap_or(0);
            self.charged.saturating_sub(price(records.added)).saturating_add(price(records.taken))
        } else {
            0
        };
        ctx.additional_limit.count_start(gas, &counted, credit)
    }
}

/// Holds `answer`, the success revm answered the call `start` began with, to `verdict`: a crossing
/// is the stop the answer gives way to, on the gas its caller forwarded, untouched, as a frame
/// stopped before it ran ([`stop_the_answer`]), and what the answer's start moved is taken back
/// with it ([`take_back_a_stopped_answer`]). A failed answer kept nothing and is held to nothing.
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
/// call's caller forwarded, untouched, with the reservoir and the pools the call inherited, and no
/// precompile output, as [`synthetic_frame_result`] answers a frame that never started.
#[cold]
#[inline(never)]
fn stop_the_answer(answer: &mut FrameResult, start: &FrameStart, output: Bytes) {
    let FrameResult::Call(outcome) = answer else {
        unreachable!("only a call is held to the limits once revm answered it")
    };
    let mut gas = Gas::new_with_regular_gas_and_reservoir(start.gas_limit, start.reservoir);
    gas.tracker_mut().set_pools_remaining(start.shares);
    let allowance = outcome.result.gas.history_allowance();
    gas.set_history_allowance(allowance);
    outcome.result = InterpreterResult::new(InstructionResult::Revert, output, gas);
    outcome.was_precompile_called = false;
    outcome.precompile_call_logs.clear();
}

/// The answer to the transaction's own frame `frame_init` when its authorization list was skipped
/// for a limit ([`AdditionalLimit::skip_authorization_list`](crate::AdditionalLimit)): the stop,
/// before the frame runs and before revm moves anything for it, on the gas its input carries, with
/// the reservoir and the start charge, which the first frame's settlement gives back.
fn stop_before_the_first_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &FrameInit,
) -> Option<FrameResult> {
    let stop = ctx.additional_limit.take_pre_frame_stop()?;
    let output = ctx.additional_limit.note_frame_stop(&stop);
    Some(synthetic_frame_result(&frame_init.frame_input, InstructionResult::Revert, output))
}

/// Decides the transaction's EIP-7702 authorization list before it is applied, as the spec has
/// it: the whole list is simulated in order — each entry with a chain id that is neither zero nor
/// the chain's, a nonce of `2^64 − 1` or a signature that recovers no authority skipped unread;
/// every other authority loaded once, with its code, and skipped when that code is not a
/// delegation; a nonce compared with the authority's effective one, which the simulation bumps
/// for each entry it accepts — and the totals it would keep are held to the transaction's KV and
/// state-growth limits. The stop when they cross one, KV updates first; `None` when they fit, or
/// when the list is too short to cross either.
///
/// The simulation writes nothing and warms nothing: the loads are taken back to a journal mark,
/// which leaves the accounts in the transaction's state, cold, so a stateless witness holds every
/// authority the simulation read. A database error reading one is the transaction's error.
fn hold_authorization_list<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) -> Result<Option<LimitCheck>, DB::Error> {
    if ctx.tx().tx_type() != TransactionType::Eip7702 {
        return Ok(None);
    }
    let entries = ctx.tx().authorization_list_len() as u64;
    if !ctx.additional_limit.authorization_list_may_cross(entries) {
        return Ok(None);
    }
    let (records, created) = simulate_authorization_list(ctx)?;
    let check = ctx.additional_limit.hold_authorization_list(records, created);
    Ok(check.exceeded_limit().then_some(check))
}

/// What one authority of a simulated authorization list stands at.
struct SimulatedAuthority {
    address: Address,
    /// Its nonce, bumped by every entry the simulation accepted.
    nonce: u64,
    /// Whether its code is empty or a delegation, which an entry needs to apply.
    applies: bool,
    /// Whether the transaction's state did not hold it, so applying an entry creates it.
    absent: bool,
    /// Whether an entry for it was accepted.
    accepted: bool,
}

/// The write records and the created accounts the transaction's authorization list would keep
/// were it applied ([`hold_authorization_list`]): one record for each distinct authority an
/// accepted entry changes, the sender's account being the body's, and one created account for each
/// distinct authority it would create.
fn simulate_authorization_list<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
) -> Result<(u64, u64), DB::Error> {
    let chain_id = ctx.cfg().chain_id;
    let caller = ctx.tx().caller();
    let entries = Eip7702AuthFacts::collect(ctx.tx(), chain_id);
    let start = mark(ctx.journal_mut());
    let mut authorities: Vec<SimulatedAuthority> = Vec::new();
    // Each authority's place in `authorities`, so a list of any length is simulated without a
    // scan of what it has seen.
    let mut seen: BTreeMap<Address, usize> = BTreeMap::new();
    for entry in &entries {
        let Some(authority) = entry.authority else { continue };
        let index = match seen.get(&authority) {
            Some(index) => *index,
            None => {
                let account = ctx.journal_mut().load_account_with_code(authority)?.data;
                let applies = account
                    .info
                    .code
                    .as_ref()
                    .is_none_or(|code| code.is_empty() || code.is_eip7702());
                let absent =
                    account.info.is_empty() && account.is_loaded_as_not_existing_not_touched();
                authorities.push(SimulatedAuthority {
                    address: authority,
                    nonce: account.info.nonce,
                    applies,
                    absent,
                    accepted: false,
                });
                seen.insert(authority, authorities.len() - 1);
                authorities.len() - 1
            }
        };
        let simulated = &mut authorities[index];
        if !simulated.applies || entry.nonce != simulated.nonce {
            continue;
        }
        simulated.nonce += 1;
        simulated.accepted = true;
    }
    revert_to(ctx.journal_mut(), start);
    let accepted = authorities.iter().filter(|authority| authority.accepted);
    let records = accepted.clone().filter(|authority| authority.address != caller).count();
    let created = accepted.filter(|authority| authority.absent).count();
    Ok((records as u64, created as u64))
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
/// forwards at most 98/100 of what its caller has left, so a frame at depth 1,024 has less than a
/// gas.
fn call_too_deep(frame_init: &FrameInit) -> Option<FrameResult> {
    (frame_init.depth > CALL_STACK_LIMIT as usize).then(|| {
        synthetic_frame_result(
            &frame_init.frame_input,
            InstructionResult::CallTooDeep,
            Bytes::new(),
        )
    })
}

/// Whether executing this transaction created its caller's account: a deposit-like transaction
/// whose caller was empty when the transaction loaded it, which the deposit path materialised by
/// bumping its nonce or by minting to it.
///
/// Read once op-revm deducted the caller, which loaded it and is what materialised it, off the
/// info revm keeps of the account as the transaction first loaded it (`Account::original_info`),
/// and after [`validate_env`](Handler::validate_env), which promoted a system-address transaction:
/// a deposit-like transaction is a deposit by then. Every other transaction pays a fee its caller
/// must hold, so it has an account already.
fn deposit_created_caller<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
) -> bool {
    if ctx.tx().tx_type() != DEPOSIT_TRANSACTION_TYPE {
        return false;
    }
    let caller = ctx.tx().caller();
    ctx.journal_ref().state.get(&caller).is_some_and(|account| account.original_info().is_empty())
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

/// The account writes of the EIP-7702 authorities applied since journal entry `journal_i`, and the
/// accounts among them the application created: each applied authorization bumps its authority's
/// nonce once, so the distinct authorities other than the sender are the accounts written. The
/// sender's write is part of the transaction body.
///
/// An authority the application created is one revm charged the account's state gas for: one
/// that was empty, loaded as not existing and untouched when the application reached it. Read
/// after the application, that is an authority the database answered no account for, empty at
/// the transaction's start, and touched first by the application — revm's flag of a touched
/// account outlives the transaction that set it on a journal that runs several, so an authority an
/// earlier transaction touched was touched before. Validation's bump of the sender comes first,
/// and never makes the sender a new account.
fn applied_authorities<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    journal_i: usize,
) -> (u64, u64) {
    if ctx.tx().tx_type() != TransactionType::Eip7702 {
        return (0, 0);
    }
    let caller = ctx.tx().caller();
    let mut authorities: Vec<Address> = Vec::new();
    let mut touched: Vec<Address> = Vec::new();
    for entry in &ctx.journal_ref().journal()[journal_i..] {
        match entry {
            JournalEntry::NonceBump { address } if *address != caller => authorities.push(*address),
            JournalEntry::AccountTouched { address } => touched.push(*address),
            _ => {}
        }
    }
    authorities.sort_unstable();
    authorities.dedup();
    touched.sort_unstable();
    // An applied authority that is the block beneficiary wrote the beneficiary's account.
    if authorities.binary_search(&ctx.block().beneficiary).is_ok() {
        ctx.detention.mark_before_execution(VolatileDataAccess::BENEFICIARY_BALANCE);
    }
    let state = &ctx.journal_ref().state;
    let created = authorities
        .iter()
        .filter(|authority| {
            touched.binary_search(authority).is_ok() &&
                state.get(*authority).is_some_and(|account| {
                    account.is_loaded_as_not_existing() && account.original_info().is_empty()
                })
        })
        .count();
    (authorities.len() as u64, created as u64)
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
/// first frame's record does not, so that frame carries what it cost as its start charge, which
/// its failure gives back ([`first_frame_record_charge`]). A first frame revm refuses to start on
/// its sender's account makes no record, and gets the charge back as any frame that kept nothing
/// does: among the transactions that pay history, that is a creation from an account whose nonce
/// cannot be bumped, which only a caller that turned the nonce check off can run, and which revm
/// answers with a success and no address.
/// `false` when the transaction cannot pay, which is an out-of-gas before it runs.
fn charge_records_made_outside_a_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    gas: &mut GasTracker,
    applied_authorities: u64,
) -> bool {
    if !ctx.prices_history() {
        return true;
    }
    charge_write_records_history(gas, first_frame_records(ctx), applied_authorities).is_some()
}

/// The write records the transaction's own frame makes, as the journal's state stands before the
/// first frame: the recipient's, when the value changes an account the transaction has not
/// changed, or the account a creation creates ([`charge_records_made_outside_a_frame`]).
fn first_frame_records<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
) -> u64 {
    let tx = ctx.tx();
    match tx.kind() {
        TxKind::Create => 1,
        TxKind::Call(to) => {
            let predicted = ctx.accounts().transfer_records(tx.caller(), to, tx.value()).transfer;
            debug_assert_eq!(predicted.taken, 0, "the sender's value returns no account");
            predicted.added
        }
    }
}

/// The history gas charged before execution for the write record the transaction's own frame
/// makes, which the frame carries as its start charge: nothing for a transaction that pays no
/// history. Priced on the journal as pre-execution left it, which is the state the charge was
/// made on: nothing between the charge and the first frame's input changes it.
fn first_frame_record_charge<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
) -> u64 {
    if !ctx.prices_history() {
        return 0;
    }
    // Pre-execution charged this price, so it has one.
    write_record_history_gas(first_frame_records(ctx)).unwrap_or_default()
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
            pool_shares: PoolAmounts::UNLIMITED,
            history_allowance: 0,
        }))
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
                pool_shares: PoolAmounts::UNLIMITED,
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

    /// The reservoir a transaction settled with is reported where revm builds its result's gas, and
    /// a system call's, whose path builds no result gas there, once its result is built: a handler
    /// reused for a system call reports the system call's own reservoir, not the transaction's
    /// before it. A transaction below the execution cap reports nothing, one above it its own
    /// margin, and a deposit that halts nothing, its failed-deposit result leaving no reservoir;
    /// after each, a system call on revm's default gas limit reports the margin above the
    /// system-call budget.
    #[test]
    fn test_a_reused_handler_reports_each_executions_own_reservoir() {
        use crate::{
            constants::TX_GAS_LIMIT_CAP,
            history_gas,
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
            assert_ne!(settled, DEFAULT_RESERVOIR, "{case}: the transaction's own would show");

            let call = MegaTransaction::new_system_tx_with_caller(CALLER, TARGET, Bytes::new());
            evm.inner.ctx.set_tx(call);
            evm.inner.ctx.on_new_system_call();
            let result = handler.run_system_call(&mut evm).expect("the system call runs");
            assert!(result.is_success(), "{case}: {result:?}");
            assert_eq!(
                evm.ctx().additional_limit().reservoir_remaining(),
                DEFAULT_RESERVOIR,
                "{case}: the system call's own reservoir, not the transaction's"
            );
        }
    }
}
