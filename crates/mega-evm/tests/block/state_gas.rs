//! The block's state gas: what its transactions spent on the state ledger, from either pool. No
//! block limit holds it: state gas prices the state a block adds, so the block's gas limit bounds
//! it.

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{constants::TX_GAS_LIMIT_CAP, test_utils::BytecodeBuilder, MegaTxEnvelope};
use revm::{
    bytecode::opcode::{CALLDATALOAD, PUSH0, SSTORE},
    context::BlockEnv,
    database::State,
};

use crate::common::{self, CALLER, CHAIN_ID, CONTRACT};

/// Above the execution cap, where the reservoir pays state gas first.
const ABOVE_CAP: u64 = TX_GAS_LIMIT_CAP + 100_000_000;

/// Code that sets the slot the first calldata word names to one.
fn slot_writer() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u8)
        .append(PUSH0)
        .append(CALLDATALOAD)
        .append(SSTORE)
        .stop()
        .build()
}

fn state() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    State::builder().with_database(db).build()
}

/// A transaction from `caller` to `to`, carrying `input`.
fn tx_from(
    caller: Address,
    nonce: u64,
    to: Address,
    input: Bytes,
    gas_limit: u64,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input,
    };
    let hash = B256::from(U256::from(nonce) + (U256::from_be_slice(caller.as_slice()) << 64));
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), hash)),
        caller,
    )
}

/// A transaction from [`CALLER`] that fills `slot`, or leaves it as it is when it is already set.
fn writes(nonce: u64, slot: u64, gas_limit: u64) -> Recovered<MegaTxEnvelope> {
    let input = Bytes::from(U256::from(slot).to_be_bytes::<32>());
    tx_from(CALLER, nonce, CONTRACT, input, gas_limit)
}

/// Above the execution cap the reservoir pays the state gas, and the block counts it all the same:
/// the ledger is the state gas spent, whichever pool paid it. No limit refuses a transaction for
/// the state gas the block has spent.
#[test]
fn test_the_blocks_state_gas_counts_what_the_reservoir_paid() {
    let slot = common::slot_state_gas();
    let mut state = state();
    let mut env = common::evm_env();
    env.block_env = BlockEnv { gas_limit: 10_000_000_000, ..env.block_env };
    let mut executor = common::executor_with_env(&mut state, common::unlimited_ctx(), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    for (nonce, slot_index) in [(0, 1), (1, 2), (2, 3)] {
        let outcome =
            executor.run_transaction(&writes(nonce, slot_index, ABOVE_CAP)).expect("it executes");
        assert_eq!(outcome.gas.state, slot, "the same state gas as below the cap");
        assert!(outcome.gas.reservoir_remaining > 0, "the reservoir paid it");
        assert_eq!(
            outcome.gas.reservoir_remaining,
            ABOVE_CAP - TX_GAS_LIMIT_CAP - outcome.gas.state - outcome.gas.history,
        );
        executor.commit_transaction_outcome(outcome).expect("no block limit refuses it");
    }

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 3);
    assert_eq!(result.gas.state, 3 * slot);
}
