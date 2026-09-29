//! A deposit is never refused by a block's packing budgets.
//!
//! The execution-gas, state-gas, data-size and KV limits of a block budget the transactions its
//! builder chooses. A deposit is not chosen: the block derived from L1 must include it. So none
//! of the four refuses a deposit — not before it executes, not after, and not when an outcome
//! executed earlier is committed through either commit path — and every deposit still counts
//! towards all four, so the ordinary transactions after the deposits find the room they used.

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::BytecodeBuilder, BlockGasCounters, BlockLimits, LimitUsage, MegaTxEnvelope,
};
use revm::{
    bytecode::opcode::{CALLDATALOAD, PUSH0, SSTORE},
    database::State,
};

use crate::common::{self, executor, CHAIN_ID};

/// Sets the slot the first calldata word names to one.
const WRITER: Address = address!("0x1000000000000000000000000000000000000d01");

/// The sender of the ordinary transaction, so its nonce does not depend on the deposits'.
const SENDER: Address = address!("0x2000000000000000000000000000000000000d02");

/// Calldata past the slot word, so each deposit carries data size well beyond its body.
const PADDING: usize = 1_000;

/// 1,000,000 of regular gas on top of what the slot, its write record and the body of an
/// ordinary transaction cost at the byte prices in effect.
fn gas_limit() -> u64 {
    1_000_000 +
        common::slot_state_gas() +
        common::body_history(32 + PADDING as u64) +
        mega_evm::write_record_history_gas(1).expect("a record has a price")
}

fn state() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    let writer = BytecodeBuilder::default()
        .push_number(1_u8)
        .append(PUSH0)
        .append(CALLDATALOAD)
        .append(SSTORE)
        .stop()
        .build();
    db.set_account_code(WRITER, writer);
    db.set_account_balance(SENDER, U256::from(1_000_000_000_000_000_u64));
    State::builder().with_database(db).build()
}

/// Calldata that names `slot`, padded with [`PADDING`] bytes.
fn input(slot: u64) -> Bytes {
    let mut input = U256::from(slot).to_be_bytes::<32>().to_vec();
    input.resize(32 + PADDING, 0xab);
    Bytes::from(input)
}

/// A deposit that fills the fresh `slot`: execution gas, state gas, data size and a write record,
/// all four.
fn deposit(slot: u64) -> Recovered<MegaTxEnvelope> {
    common::deposit_tx_to(WRITER, input(slot), gas_limit())
}

/// An ordinary transaction that fills the fresh `slot`, so each of the four limits has something
/// to refuse it on.
fn ordinary(slot: u64) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce: 0,
        gas_price: 1_000_000,
        gas_limit: gas_limit(),
        to: TxKind::Call(WRITER),
        value: U256::ZERO,
        input: input(slot),
    };
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::repeat_byte(0x0d),
        )),
        SENDER,
    )
}

/// What one deposit adds to a block, read off a probe block with no limits.
fn one_deposit() -> (BlockGasCounters, LimitUsage) {
    let mut state = state();
    let mut probe = executor(&mut state, common::unlimited_ctx());
    probe.apply_pre_execution_changes().expect("the block starts");
    probe.execute_transaction(&deposit(1)).expect("the probe executes");
    let (gas, usage) = (*probe.gas(), probe.limiter().usage);
    assert!(gas.execution > 0 && usage.data_size > 0, "{gas:?} {usage:?}");
    assert_eq!(gas.state, common::slot_state_gas(), "the slot's state gas");
    assert_eq!(usage.write_records, 1, "the slot");
    (gas, usage)
}

/// The block limits the test holds a block to.
#[derive(Clone, Copy, Debug)]
enum Cap {
    ExecutionGas,
    StateGas,
    DataSize,
    KvUpdates,
    All,
}

impl Cap {
    /// Limits with room for a deposit and a half on this cap, so the second deposit crosses it. A
    /// deposit keeps one record, so on the write records that is room for one.
    fn limits(self, gas: &BlockGasCounters, usage: &LimitUsage) -> BlockLimits {
        let half_again = |one: u64| one + one / 2;
        let limits = BlockLimits::no_limits();
        let execution = limits.with_block_execution_gas_limit(half_again(gas.execution));
        let state = limits.with_block_state_gas_limit(half_again(gas.state));
        let data = limits.with_block_txs_data_limit(half_again(usage.data_size));
        let kv = limits.with_block_kv_update_limit(half_again(usage.write_records));
        match self {
            Self::ExecutionGas => execution,
            Self::StateGas => state,
            Self::DataSize => data,
            Self::KvUpdates => kv,
            Self::All => execution
                .with_block_state_gas_limit(half_again(gas.state))
                .with_block_txs_data_limit(half_again(usage.data_size))
                .with_block_kv_update_limit(half_again(usage.write_records)),
        }
    }

    /// What the refusal of the ordinary transaction names. With all four limits reached, the
    /// execution-gas limit is the first the block checks.
    const fn refusal(self) -> &'static str {
        match self {
            Self::ExecutionGas | Self::All => "Block execution gas limit reached",
            Self::StateGas => "Block state gas limit reached",
            Self::DataSize => "Block transactions data limit reached",
            Self::KvUpdates => "Block KV update limit reached",
        }
    }
}

/// How the deposits reach the block.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// Executed and committed one at a time, so the third executes in a block past its limits.
    OneByOne,
    /// Executed as candidates before any commits, then committed through the checked commit,
    /// which checks the block's counters again: the third commits past the limits.
    CheckedCommit,
    /// The same candidates committed through alloy-evm's commit, which a debug build holds to the
    /// same check.
    TraitCommit,
}

/// Deposits cross each block limit and are included all the same, through every route; each one
/// counts towards all four dimensions; and an ordinary transaction after them is refused — before
/// it executes, and, as a candidate executed before the deposits committed, at its commit.
#[test]
fn test_no_block_limit_refuses_a_deposit_and_every_deposit_counts() {
    let (gas, usage) = one_deposit();
    for cap in [Cap::ExecutionGas, Cap::StateGas, Cap::DataSize, Cap::KvUpdates, Cap::All] {
        // A deposit that adds no state gas cannot cross a state-gas cap, and the ordinary
        // transaction after it adds none to be refused for.
        if matches!(cap, Cap::StateGas) && common::state_is_free() {
            continue;
        }
        for route in [Route::OneByOne, Route::CheckedCommit, Route::TraitCommit] {
            let name = format!("{cap:?} by {route:?}");
            let limits = cap.limits(&gas, &usage);
            let mut state = state();
            let mut executor = executor(&mut state, common::block_ctx(limits));
            executor.apply_pre_execution_changes().expect("the block starts");

            let mut late_candidate = None;
            match route {
                Route::OneByOne => {
                    for slot in 1..=3 {
                        executor
                            .execute_transaction(&deposit(slot))
                            .unwrap_or_else(|err| panic!("{name}: deposit {slot}: {err}"));
                    }
                }
                Route::CheckedCommit | Route::TraitCommit => {
                    let deposits: Vec<_> = (1..=3)
                        .map(|slot| {
                            executor
                                .run_transaction(&deposit(slot))
                                .unwrap_or_else(|err| panic!("{name}: deposit {slot}: {err}"))
                        })
                        .collect();
                    late_candidate = Some(
                        executor
                            .run_transaction(&ordinary(100))
                            .unwrap_or_else(|err| panic!("{name}: nothing has committed: {err}")),
                    );
                    for (slot, outcome) in (1..).zip(deposits) {
                        assert_eq!(outcome.gas.state, gas.state, "{name}: deposit {slot}");
                        if matches!(route, Route::CheckedCommit) {
                            executor
                                .commit_transaction_outcome(outcome)
                                .unwrap_or_else(|err| panic!("{name}: deposit {slot}: {err}"));
                        } else {
                            BlockExecutor::commit_transaction(&mut executor, outcome);
                        }
                    }
                }
            }

            assert_eq!(executor.gas().execution, 3 * gas.execution, "{name}: execution counted");
            assert_eq!(executor.gas().state, 3 * gas.state, "{name}: state counted");
            assert_eq!(
                executor.limiter().usage.data_size,
                3 * usage.data_size,
                "{name}: data size counted"
            );
            assert_eq!(executor.limiter().usage.write_records, 3, "{name}: write records counted");
            assert!(
                executor.gas().execution > limits.block_execution_gas_limit ||
                    executor.gas().state > limits.block_state_gas_limit ||
                    executor.limiter().usage.data_size > limits.block_txs_data_limit ||
                    executor.limiter().usage.write_records > limits.block_kv_update_limit,
                "{name}: the deposits crossed the limit",
            );

            let err = executor
                .execute_transaction(&ordinary(101))
                .expect_err("an ordinary transaction finds the room the deposits used");
            assert!(format!("{err}").contains(cap.refusal()), "{name}: {err}");
            if let Some(candidate) = late_candidate {
                let err = executor
                    .commit_transaction_outcome(candidate)
                    .expect_err("a candidate executed before the deposits committed");
                assert!(format!("{err}").contains(cap.refusal()), "{name}: at commit: {err}");
            }

            let (_, result) = executor.finish_with_counters().expect("the block finishes");
            assert_eq!(result.receipts().len(), 3, "{name}: the three deposits and nothing else");
            assert_eq!(result.gas.execution, 3 * gas.execution, "{name}");
            assert_eq!(result.gas.state, 3 * gas.state, "{name}");
        }
    }
}
