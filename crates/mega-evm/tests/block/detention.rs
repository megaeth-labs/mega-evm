//! Gas detention in a block: a stopped transaction is included with a failed receipt that bills
//! what ran, and every transaction starts detention afresh.

use alloy_evm::block::BlockExecutor;
use alloy_primitives::Bytes;
use mega_evm::{
    constants::BLOCK_ENV_ACCESS_COMPUTE_GAS, test_utils::BytecodeBuilder, LimitCheck, LimitKind,
};
use revm::{
    bytecode::opcode::{JUMP, JUMPDEST, MCOPY, POP, PUSH0, TIMESTAMP},
    context::result::ExecutionResult,
    database::State,
};

use crate::common::{self, executor_with_env, unlimited_ctx, user_tx};

/// Reads the block's timestamp, then copies memory forever.
fn read_then_spin() -> Bytes {
    let code = BytecodeBuilder::default().append_many([TIMESTAMP, POP]);
    let dest = code.len() as u32;
    code.append(JUMPDEST)
        .push_number(0x8000_u16)
        .append_many([PUSH0, PUSH0, MCOPY])
        .push_number(dest)
        .append(JUMP)
        .build()
}

/// The stop is a failed receipt with no log, billed for the transaction's intrinsic gas, the
/// compute it spent up to the cap and its body's history, not for the gas it was given; the
/// block counts the same. The next transaction, which reads again, is held to a cap of its own
/// and stops the same way.
#[test]
fn test_a_stopped_transaction_is_included_with_a_failed_receipt_that_bills_what_ran() {
    let mut db = common::database();
    db.set_account_code(common::CONTRACT, read_then_spin());
    let mut state = State::builder().with_database(db).build();
    let mut env = common::evm_env();
    env.block_env.gas_limit = 100_000_000;
    let mut executor = executor_with_env(&mut state, unlimited_ctx(), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    let gas_limit = 29_000_000;
    let mut used = Vec::new();
    for nonce in 0..2 {
        let outcome = executor.run_transaction(&user_tx(nonce, gas_limit)).expect("it executes");
        assert!(
            matches!(outcome.inner.result, ExecutionResult::Revert { .. }),
            "{:?}",
            outcome.inner.result
        );
        assert!(
            matches!(
                outcome.inner.limit_exceeded,
                Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, .. })
            ),
            "{:?}",
            outcome.inner.limit_exceeded
        );
        let gas = outcome.inner.gas;
        assert!(gas.regular > BLOCK_ENV_ACCESS_COMPUTE_GAS);
        assert!(gas.regular < BLOCK_ENV_ACCESS_COMPUTE_GAS + 100_000, "{}", gas.regular);
        assert!(gas.gas_used < gas_limit, "the stop does not burn the gas");
        used.push(gas);
        executor.commit_transaction_outcome(outcome).expect("the stop is included");
    }
    assert_eq!(used[0], used[1], "each transaction is held to its own cap");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    let receipts = result.receipts();
    assert_eq!(receipts.len(), 2);
    let mut cumulative = 0;
    for (receipt, gas) in receipts.iter().zip(&used) {
        cumulative += gas.gas_used;
        assert!(!receipt.status(), "a stop is a failed receipt");
        assert!(receipt.logs().is_empty());
        assert_eq!(receipt.cumulative_gas_used(), cumulative);
    }
    assert_eq!(result.gas.execution, used[0].regular + used[1].regular);
    assert_eq!(result.gas.history, used[0].history + used[1].history);
}
