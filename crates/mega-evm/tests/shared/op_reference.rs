//! op-revm as the reference the comparisons hold Satin to.
//!
//! A target includes this file once, from its `main.rs`:
//!
//! ```ignore
//! #[path = "../shared/op_reference.rs"]
//! mod op_reference;
//! ```
//!
//! op-revm's handler leaves two things to its consumer that Satin's handler does itself: EIP-8037's
//! split of a system call's gas — at most [`SYSTEM_CALL_REGULAR_GAS`] of it regular gas, the rest
//! its state-gas reservoir — and the reservoir a result settled with, which revm's result does not
//! carry. [`OpReference`] is op-revm's handler with both, so a comparison runs op-revm on the gas
//! Satin runs on, and reads op-revm's reservoir beside its result.
//!
//! The reference overrides every phase op-revm's handler overrides, each forwarded to it, and
//! nothing else: a phase op-revm comes to override must be forwarded here too.

use std::{cell::Cell, convert::Infallible, ops::Deref};

use alloy_primitives::{Address, Bytes};
use mega_evm::{test_utils::MemoryDatabase, MegaHaltReason};
use op_revm::{
    handler::OpHandler, L1BlockInfo, OpEvm, OpSpecId, OpTransaction, OpTransactionError,
};
use revm::{
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult, ResultGas},
        BlockEnv, CfgEnv, Context, ContextSetters, TxEnv,
    },
    context_interface::cfg::gas::GasTracker,
    handler::{EthFrame, ExecuteEvm, FrameResult, Handler, SystemCallTx},
    inspector::NoOpInspector,
    interpreter::{interpreter::EthInterpreter, InitialAndFloorGas},
    state::EvmState,
    Journal,
};

/// op-revm's context over a [`MemoryDatabase`].
pub(crate) type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;

/// op-revm's EVM over [`OpContext`].
pub(crate) type Op = OpEvm<OpContext, NoOpInspector>;

type OpError = EVMError<Infallible, OpTransactionError>;

/// The regular gas EIP-8037 runs a system call on, at most: the system-call gas limit before
/// EIP-8037. Written out rather than taken from the engine, so the comparison holds the engine's
/// own number to it.
const SYSTEM_CALL_REGULAR_GAS: u64 = 30_000_000;

/// A result and its state, and the reservoir the result settled with: what a transaction or a
/// system call left of its reservoir, which its sender gets back.
///
/// Derefs to the result and state, so `outcome.result` and `outcome.state` read through.
#[derive(Debug)]
pub(crate) struct Settled {
    pub(crate) result_and_state: ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>,
    pub(crate) reservoir_remaining: u64,
}

impl Deref for Settled {
    type Target = ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>;

    fn deref(&self) -> &Self::Target {
        &self.result_and_state
    }
}

/// Runs `tx` on `op` through [`OpReference`], as op-revm's own `transact` runs it.
pub(crate) fn transact(op: &mut Op, tx: OpTransaction<TxEnv>) -> Result<Settled, OpError> {
    op.0.ctx.set_tx(tx);
    let mut handler = OpReference::new(false);
    let result = handler.run(op)?;
    Ok(Settled {
        result_and_state: ExecResultAndState::new(result, op.finalize()),
        reservoir_remaining: handler.reservoir.get(),
    })
}

/// Runs op-revm's system call from `caller` to `contract` with `data` on `gas_limit` through
/// [`OpReference`]: its system-call transaction with the gas limit replaced, run through its
/// handler's system-call path.
pub(crate) fn system_call(
    op: &mut Op,
    caller: Address,
    contract: Address,
    data: Bytes,
    gas_limit: u64,
) -> Result<Settled, OpError> {
    let mut tx = OpTransaction::<TxEnv>::new_system_tx_with_caller(caller, contract, data);
    tx.base.gas_limit = gas_limit;
    op.0.ctx.set_tx(tx);
    let mut handler = OpReference::new(true);
    let result = handler.run_system_call(op)?;
    Ok(Settled {
        result_and_state: ExecResultAndState::new(result, op.finalize()),
        reservoir_remaining: handler.reservoir.get(),
    })
}

/// op-revm's handler, splitting a system call's gas as EIP-8037 has it and keeping the reservoir
/// its result settled with. Every phase op-revm's handler overrides is its own.
struct OpReference {
    op: OpHandler<Op, OpError, EthFrame<EthInterpreter>>,
    /// Whether the handler runs a system call, whose gas it splits.
    system_call: bool,
    /// The reservoir read where revm builds the result's gas of a transaction.
    settled: Cell<Option<u64>>,
    /// The reservoir the result settled with; none for a failed deposit.
    reservoir: Cell<u64>,
}

impl OpReference {
    fn new(system_call: bool) -> Self {
        Self {
            op: OpHandler::new(),
            system_call,
            settled: Cell::new(None),
            reservoir: Cell::new(0),
        }
    }
}

impl Handler for OpReference {
    type Evm = Op;
    type Error = OpError;
    type HaltReason = MegaHaltReason;

    fn validate_env(&self, evm: &mut Op) -> Result<(), OpError> {
        self.op.validate_env(evm)
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Op,
        init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), OpError> {
        self.op.validate_against_state_and_deduct_caller(evm, init_and_floor_gas)
    }

    fn tx_gas(&self, evm: &mut Op, init_and_floor_gas: &InitialAndFloorGas) -> GasTracker {
        let mut gas = self.op.tx_gas(evm, init_and_floor_gas);
        if self.system_call {
            let margin = gas.remaining().saturating_sub(SYSTEM_CALL_REGULAR_GAS);
            gas.set_remaining(gas.remaining() - margin);
            gas.set_reservoir(gas.reservoir() + margin);
        }
        gas
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Op,
        frame_result: &mut FrameResult,
        parent_gas: &mut GasTracker,
    ) -> Result<(), OpError> {
        self.op.last_frame_result(evm, frame_result, parent_gas)
    }

    fn post_execution(
        &self,
        evm: &mut Op,
        exec_result: &mut FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<ResultGas, OpError> {
        self.settled.set(Some(exec_result.gas().reservoir()));
        self.op.post_execution(evm, exec_result, init_and_floor_gas, eip7702_gas_refund)
    }

    fn reimburse_caller(&self, evm: &mut Op, exec_result: &mut FrameResult) -> Result<(), OpError> {
        self.op.reimburse_caller(evm, exec_result)
    }

    fn refund(
        &self,
        evm: &mut Op,
        exec_result: &mut FrameResult,
        eip7702_refund: i64,
    ) -> Result<(), OpError> {
        self.op.refund(evm, exec_result, eip7702_refund)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Op,
        exec_result: &mut FrameResult,
    ) -> Result<(), OpError> {
        self.op.reward_beneficiary(evm, exec_result)
    }

    /// revm's system-call path settles without `post_execution`: the result it hands over here is
    /// the settled one.
    fn execution_result(
        &mut self,
        evm: &mut Op,
        result: FrameResult,
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<MegaHaltReason>, OpError> {
        let reservoir = self.settled.get().unwrap_or_else(|| result.gas().reservoir());
        let result = self.op.execution_result(evm, result, result_gas)?;
        self.reservoir.set(reservoir);
        Ok(result)
    }

    fn catch_error(
        &self,
        evm: &mut Op,
        error: OpError,
    ) -> Result<ExecutionResult<MegaHaltReason>, OpError> {
        self.op.catch_error(evm, error)
    }
}
