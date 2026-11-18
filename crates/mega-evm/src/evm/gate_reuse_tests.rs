//! Gates that only become observable once a context outlives a spec change.
//!
//! A block executor builds a fresh context per block, so the `MINI_REX` bookkeeping is empty
//! whenever the ladder reaches the `MiniRex1` alias window. A context can also be carried across
//! a spec change through the public API (`into_inner().ctx.with_cfg(..)`), and then the trackers,
//! the latched limit verdict and the dynamic storage-gas cache still hold the previous spec's
//! state. These tests pin what the alias window must do with that state, so a gate that is
//! flipped one rung cannot quietly act on it.

use std::{cell::Cell, rc::Rc};

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, CfgEnv, ContextTr, JournalTr, TxEnv},
    handler::EvmTr,
    interpreter::{
        CallInputs, CallOutcome, Gas, InstructionResult, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

use crate::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100001");
const INNER: Address = address!("0000000000000000000000000000000000100002");

const INITIAL_BALANCE: u64 = 1_000_000_000;

fn db_with_contract(code: alloy_primitives::Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(INITIAL_BALANCE))
        .account_code(CONTRACT, code)
}

fn tx_to_contract(gas_limit: u64) -> MegaTransaction {
    use crate::MegaTransactionNew as _;
    let mut tx = MegaTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit,
        gas_price: 0,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn zero_fees(ctx: &mut MegaContext<MemoryDatabase, crate::EmptyExternalEnv>) {
    ctx.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
}

/// A `MINI_REX` context whose transaction compute limit is 1, so accessing a block-environment
/// value latches `ExceedsLimit` on the additional-limit tracker.
fn latched_evm(
    code: alloy_primitives::Bytes,
) -> MegaEvm<MemoryDatabase, revm::inspector::NoOpInspector, crate::EmptyExternalEnv> {
    let mut ctx = MegaContext::new(db_with_contract(code), MegaSpecId::MINI_REX)
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(1),
        );
    zero_fees(&mut ctx);
    MegaEvm::new(ctx)
}

/// `TIMESTAMP POP STOP`: the first block-environment access is what the alias window must treat
/// as inert.
fn timestamp_code() -> alloy_primitives::Bytes {
    BytecodeBuilder::default().append(TIMESTAMP).append(POP).append(STOP).build()
}

/// Runs the latching transaction and returns the context that still carries its verdict.
fn latch_then_ctx(
    code: alloy_primitives::Bytes,
) -> MegaContext<MemoryDatabase, crate::EmptyExternalEnv> {
    let mut evm = latched_evm(code);
    let outcome = evm.execute_transaction(tx_to_contract(22_000)).expect("first tx must execute");
    assert!(
        outcome.result_and_state.result.is_halt(),
        "the seeding transaction must halt on its compute limit"
    );
    assert!(
        evm.ctx_ref().additional_limit.borrow().rescued_gas > 0,
        "the seeding transaction must leave rescued gas behind"
    );
    evm.into_inner().ctx
}

fn to_alias(
    ctx: MegaContext<MemoryDatabase, crate::EmptyExternalEnv>,
) -> MegaContext<MemoryDatabase, crate::EmptyExternalEnv> {
    ctx.with_cfg(CfgEnv::new_with_spec(MegaSpecId::MINI_REX_1))
}

/// The `MINI_REX` gate on frame entry must not act on a verdict inherited from the previous spec:
/// with the gate off, the alias window runs the transaction instead of rejecting it up front.
#[test]
fn test_mini_rex_1_ignores_the_latched_limit_on_frame_entry() {
    let ctx = to_alias(latch_then_ctx(timestamp_code()));
    let mut evm = MegaEvm::new(ctx);
    let outcome = evm
        .execute_transaction(tx_to_contract(30_000))
        .expect("the alias window must execute the transaction");
    assert!(
        outcome.result_and_state.result.is_success(),
        "the alias window must not reject a transaction on a stale limit verdict: {:?}",
        outcome.result_and_state.result
    );
}

/// `try_rescue_gas` is called unconditionally on the frame-return path, so the alias window must
/// not run that path: the rescued-gas total inherited from `MINI_REX` stays untouched.
#[test]
fn test_mini_rex_1_does_not_rescue_gas_for_inherited_state() {
    let ctx = to_alias(latch_then_ctx(timestamp_code()));
    let before = ctx.additional_limit.borrow().rescued_gas;
    let mut evm = MegaEvm::new(ctx);
    evm.execute_transaction(tx_to_contract(30_000)).expect("tx must execute");
    let after = evm.ctx_ref().additional_limit.borrow().rescued_gas;
    assert_eq!(after, before, "the alias window must not add rescued gas");
}

/// The final-frame result path reads `rescued_gas` when it erases the refund. At the alias window
/// it must stay closed, so a transaction consumes exactly what the same transaction consumes on a
/// fresh alias context.
#[test]
fn test_mini_rex_1_charges_the_same_gas_as_a_fresh_context() {
    let mut fresh = MegaEvm::new({
        let mut ctx = MegaContext::new(db_with_contract(timestamp_code()), MegaSpecId::MINI_REX_1);
        zero_fees(&mut ctx);
        ctx
    });
    let fresh_gas = fresh
        .execute_transaction(tx_to_contract(30_000))
        .expect("fresh tx must execute")
        .result_and_state
        .result
        .tx_gas_used();

    let mut reused = MegaEvm::new(to_alias(latch_then_ctx(timestamp_code())));
    let reused_gas = reused
        .execute_transaction(tx_to_contract(30_000))
        .expect("reused tx must execute")
        .result_and_state
        .result
        .tx_gas_used();

    assert_eq!(
        reused_gas, fresh_gas,
        "the alias window must charge the same gas whether or not the context carries MINI_REX state"
    );
}

/// A halt that happens after a block-environment access must keep the generic limit verdict in
/// the alias window instead of being reclassified as a volatile-data halt.
#[test]
fn test_mini_rex_1_does_not_reclassify_a_halt_as_volatile_data() {
    let ctx = to_alias(latch_then_ctx(timestamp_code()));
    let mut evm = MegaEvm::new(ctx);
    let outcome = evm
        .execute_transaction(tx_to_contract(21_002))
        .expect("the transaction must produce an execution result");

    match &outcome.result_and_state.result {
        ExecutionResult::Halt { reason, .. } => {
            assert!(
                matches!(reason, MegaHaltReason::ComputeGasLimitExceeded { .. }),
                "the alias window must keep the generic limit verdict, got {reason:?}"
            );
        }
        other => panic!("expected a halt, got {other:?}"),
    }
}

/// `on_new_block` refreshes the dynamic storage-gas cache. The alias window has no reader for the
/// refreshed cache, and clearing it would lose the buckets the previous spec priced.
#[test]
fn test_mini_rex_1_keeps_the_dynamic_storage_gas_buckets_across_a_block() {
    let mut ctx = MegaContext::new(
        db_with_contract(
            BytecodeBuilder::default().sstore(U256::from(1), U256::from(2)).stop().build(),
        ),
        MegaSpecId::MINI_REX,
    );
    zero_fees(&mut ctx);
    let mut evm = MegaEvm::new(ctx);
    evm.execute_transaction(tx_to_contract(1_000_000)).expect("sstore tx must execute");
    let ctx = evm.into_inner().ctx;
    let before = ctx.dynamic_storage_gas_cost.borrow().get_bucket_ids();
    assert!(!before.is_empty(), "the seeding transaction must price at least one bucket");

    let ctx = to_alias(ctx).with_block(Default::default());
    let after = ctx.dynamic_storage_gas_cost.borrow().get_bucket_ids();
    assert_eq!(after, before, "the alias window must not clear the inherited bucket set");
}

/// An inspector that answers a nested call must not leave a limit frame behind in the alias
/// window: `push_empty_frame` is only paired with a pop when the `MINI_REX` accounting runs.
struct SkipNestedCall {
    intercepted: Rc<Cell<u32>>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipNestedCall {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (context.journal().depth() > 0).then(|| {
            self.intercepted.set(self.intercepted.get() + 1);
            CallOutcome::new(
                InterpreterResult {
                    result: InstructionResult::Stop,
                    output: Bytes::new(),
                    gas: Gas::new(inputs.gas_limit),
                },
                0..0,
            )
        })
    }
}

#[test]
fn test_mini_rex_1_inspector_intercept_leaves_no_limit_frame() {
    let callee = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(INNER)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let inner = BytecodeBuilder::default().stop().build();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(INITIAL_BALANCE))
        .account_code(CONTRACT, callee)
        .account_code(INNER, inner);
    let mut ctx = MegaContext::new(db, MegaSpecId::MINI_REX_1);
    zero_fees(&mut ctx);
    let intercepted = Rc::new(Cell::new(0u32));
    let inspector = SkipNestedCall { intercepted: Rc::clone(&intercepted) };
    let mut evm = MegaEvm::new(ctx).with_inspector(inspector);

    evm.execute_transaction(tx_to_contract(1_000_000)).expect("intercepted tx must execute");
    assert_eq!(intercepted.get(), 1, "the nested call must be intercepted");
    assert!(
        !evm.ctx_ref().additional_limit.borrow().data_size.has_active_frame(),
        "an intercepted frame must not leave a limit frame behind in the alias window"
    );
}
