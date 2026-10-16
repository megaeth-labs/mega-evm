//! Every stop class through the harness: a transaction stopped by the data-size, KV, state-gas
//! and compute limits, a crossing at a precompile's price, a crossing in an answer, and a block
//! budget refusing a later transaction. A stop is what the block records, so a validator must
//! produce the same stop from the same witness.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::keyless::{IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS},
    test_utils::BytecodeBuilder,
    LimitCheck, LimitKind, ProtocolLimits, TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{
        ADD, CALL, DUP1, GAS, JUMP, JUMPDEST, LOG0, MCOPY, POP, PUSH0, SLOAD, SSTORE, SWAP1,
        TIMESTAMP,
    },
    context::result::ExecutionResult,
};

use super::{
    basics::{slot, slot_writer},
    harness::{call, call_with_value, Case, Run},
};
use crate::common::{self, loosest_tx, CALLER, CONTRACT};

/// Code that emits one log of `data_size` zero bytes.
fn logger(data_size: usize) -> Bytes {
    BytecodeBuilder::default()
        .push_number(data_size as u32)
        .push_number(0_u8)
        .append(LOG0)
        .stop()
        .build()
}

/// A gas limit for a call to the logger of `data_size` bytes.
fn log_gas(data_size: usize) -> u64 {
    1_000_000 +
        common::body_history(0) +
        mega_evm::history_gas(mega_evm::log_history_bytes(0, data_size as u64))
            .expect("the log has a price")
}

/// Code that adds one to each of the slots `1..=writes`.
fn incrementer(writes: u64) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 1..=writes {
        code = code
            .push_number(slot)
            .append(DUP1)
            .append(SLOAD)
            .push_number(1_u8)
            .append(ADD)
            .append(SWAP1)
            .append(SSTORE);
    }
    code.stop().build()
}

/// A gas limit for a call to the incrementer of `writes` slots.
fn increment_gas(writes: u64) -> u64 {
    10_000_000 +
        writes * common::slot_state_gas() +
        common::body_history(0) +
        mega_evm::write_record_history_gas(writes).expect("the records have a price")
}

/// Code that reads the block's timestamp, then copies memory forever.
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

/// Code that reads the block's timestamp, then calls `ecrecover` on 128 bytes of memory with
/// all its gas: a precompile priced at 3,000, more than a small cap leaves.
fn read_then_ecrecover() -> Bytes {
    BytecodeBuilder::default()
        .append_many([TIMESTAMP, POP])
        .append_many([PUSH0, PUSH0])
        .push_number(128_u8)
        .append_many([PUSH0, PUSH0])
        .push_number(1_u8)
        .append_many([GAS, CALL, POP])
        .stop()
        .build()
}

/// The calldata of a `keylessDeploy` call the dispatch takes: its selector and bytes the value
/// rule refuses before they are decoded.
fn keyless_call() -> Bytes {
    IKeylessDeploy::keylessDeployCall::SELECTOR.iter().copied().chain([0_u8; 8]).collect()
}

/// The stop `kind` reports.
fn stop_kind(run: &Run, index: usize) -> LimitKind {
    match run.tx(index).limit_exceeded {
        Some(LimitCheck::ExceedsLimit { kind, .. }) => kind,
        other => panic!("transaction {index} was not stopped: {other:?}"),
    }
}

/// The transaction at `index` is a stop of `kind`: a revert carrying the stop.
fn assert_stopped(run: &Run, index: usize, kind: LimitKind) {
    assert!(
        matches!(run.tx(index).result, ExecutionResult::Revert { .. }),
        "{:?}",
        run.tx(index).result
    );
    assert_eq!(stop_kind(run, index), kind);
}

/// A transaction stopped by the data-size limit.
#[test]
fn test_a_data_size_stop_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, logger(2_000));
    let limits = ProtocolLimits::loosest()
        .with_tx_runtime_limits(loosest_tx().with_tx_data_size_limit(TX_BODY_SIZE + 1_000));
    let replay = Case::new("data-size stop", db)
        .limits(limits)
        .tx(call(0, CONTRACT, Bytes::new(), log_gas(2_000)))
        .run();
    assert_stopped(&replay.recorded, 0, LimitKind::DataSize);
}

/// A transaction stopped by the KV limit.
#[test]
fn test_a_kv_stop_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, incrementer(3));
    let limits =
        ProtocolLimits::loosest().with_tx_runtime_limits(loosest_tx().with_tx_kv_update_limit(1));
    let replay = Case::new("kv stop", db)
        .limits(limits)
        .tx(call(0, CONTRACT, Bytes::new(), increment_gas(3)))
        .run();
    assert_stopped(&replay.recorded, 0, LimitKind::KVUpdate);
    assert!(replay.recorded.record.storage.len() >= 2, "the slots read before the stop");
}

/// A transaction stopped by the state-gas limit, whose crossing write was priced through SALT:
/// the bucket of the slot that crossed is in the export, as a validator asks about it too.
#[test]
fn test_a_state_gas_stop_replays() {
    if common::state_is_free() {
        return;
    }
    let mut db = common::database();
    db.set_account_code(CONTRACT, incrementer(2));
    let limits = ProtocolLimits::loosest()
        .with_tx_runtime_limits(loosest_tx().with_tx_state_gas_limit(common::slot_state_gas()));
    let replay = Case::new("state-gas stop", db)
        .limits(limits)
        .tx(call(0, CONTRACT, Bytes::new(), increment_gas(2)))
        .run();
    assert_stopped(&replay.recorded, 0, LimitKind::StateGrowth);
    assert_eq!(replay.recorded.bucket_ids.len(), 2, "both slots were priced before the stop");
}

/// A transaction stopped by gas detention at the compute cap: a regular charge crossed the
/// withheld part.
#[test]
fn test_a_compute_stop_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, read_then_spin());
    let replay = Case::new("compute stop", db)
        .limits(ProtocolLimits::DEFAULT)
        .tx(call(0, CONTRACT, Bytes::new(), 29_000_000))
        .run();
    assert_stopped(&replay.recorded, 0, LimitKind::ComputeGas);
}

/// A crossing decided at a precompile's price: after a read of the block's timestamp under a cap
/// of 1,000, `ecrecover`'s 3,000 is past the allowance and within the forward, so the call is
/// answered without running and the transaction stopped.
#[test]
fn test_a_precompile_price_crossing_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, read_then_ecrecover());
    let limits = ProtocolLimits::loosest()
        .with_tx_runtime_limits(loosest_tx().with_block_env_access_compute_gas_limit(1_000));
    let replay = Case::new("precompile crossing", db)
        .limits(limits)
        .tx(call(0, CONTRACT, Bytes::new(), 1_000_000))
        .run();
    assert_stopped(&replay.recorded, 0, LimitKind::ComputeGas);
}

/// A crossing in an answer: a `keylessDeploy` call carrying value from the block beneficiary,
/// detained from its start under a cap of 1,000, spends the call's overhead in its refusal and
/// crosses there.
#[test]
fn test_an_answer_crossing_replays() {
    let mut env = common::evm_env();
    env.block_env.beneficiary = CALLER;
    let limits = ProtocolLimits::loosest()
        .with_tx_runtime_limits(loosest_tx().with_block_env_access_compute_gas_limit(1_000));
    let replay = Case::new("answer crossing", common::database())
        .env(env)
        .limits(limits)
        .tx(call_with_value(0, KEYLESS_DEPLOY_ADDRESS, U256::from(1), keyless_call(), 1_000_000))
        .run();
    assert_stopped(&replay.recorded, 0, LimitKind::ComputeGas);
}

/// A block budget: the transaction that crosses the block's data-size limit is packed, and the
/// one after it is refused before it runs, on both runs alike.
#[test]
fn test_a_block_budget_refusal_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, logger(2_000));
    let limits = ProtocolLimits::loosest().with_block_txs_data_limit(2_500);
    let replay = Case::new("block budget", db)
        .limits(limits)
        .tx(call(0, CONTRACT, Bytes::new(), log_gas(2_000)))
        .tx(call(1, CONTRACT, Bytes::new(), log_gas(2_000)))
        .tx(call(2, CONTRACT, Bytes::new(), log_gas(2_000)))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert!(run.refusal(2).contains("Block transactions data limit reached"), "{}", run.refusal(2));
    assert_eq!(run.receipts.len(), 2);
}

/// A stop above the execution cap, where the reservoir paid the body: the same stop, the same
/// ledgers on replay.
#[test]
fn test_a_stop_above_the_execution_cap_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    // Room for the calldata but not for the slot's write record.
    let limits = ProtocolLimits::loosest()
        .with_tx_runtime_limits(loosest_tx().with_tx_data_size_limit(TX_BODY_SIZE + 32 + 10));
    let mut env = common::evm_env();
    env.block_env.gas_limit = TX_GAS_LIMIT_CAP + 100_000_000;
    let replay = Case::new("stop above the cap", db)
        .env(env)
        .limits(limits)
        .tx(call(0, CONTRACT, slot(1), TX_GAS_LIMIT_CAP + 50_000_000))
        .run();
    let run = &replay.recorded;
    assert_stopped(run, 0, LimitKind::DataSize);
    assert!(run.tx(0).gas.reservoir_remaining > 0, "the reservoir came back");
}
