//! Tests for block hash access tracking on `MegaBlockExecutor`.
//!
//! `get_accessed_block_hashes` exposes the executor database's block hash record,
//! which accumulates across every transaction executed so far.
//! `clear_accessed_block_hashes` resets it so callers can attribute BLOCKHASH
//! reads to a single transaction (the replay fixture dump relies on this).

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{BLOCKHASH, POP},
    context::BlockEnv,
    database::State,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const BLOCKHASH_CONTRACT: Address = address!("1000000000000000000000000000000000000001");
const PLAIN_CONTRACT: Address = address!("1000000000000000000000000000000000000002");

fn create_transaction(
    nonce: u64,
    to: Address,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Contract that reads `BLOCKHASH(999)` (one below the block number used in the
/// test) and discards it.
fn create_blockhash_contract() -> Bytes {
    BytecodeBuilder::default().push_number(999u16).append(BLOCKHASH).append(POP).stop().build()
}

#[test]
fn test_accessed_block_hashes_track_and_clear_per_transaction() {
    let mut db = MemoryDatabase::default();
    db.set_account_code(BLOCKHASH_CONTRACT, create_blockhash_contract());
    db.set_account_code(PLAIN_CONTRACT, BytecodeBuilder::default().stop().build());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm = evm_factory.create_evm(&mut state, EvmEnv::new(cfg_env, block_env));

    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let mut executor =
        MegaBlockExecutor::new(evm, block_ctx, chain_spec, OpAlloyReceiptBuilder::default());

    // A transaction that reads BLOCKHASH leaves its access in the record.
    executor
        .execute_transaction(&create_transaction(0, BLOCKHASH_CONTRACT))
        .expect("blockhash tx should succeed");
    let accessed = executor.get_accessed_block_hashes();
    assert_eq!(accessed.len(), 1, "BLOCKHASH read should be recorded");
    assert!(accessed.contains_key(&999), "the queried block number should be recorded");

    // Clearing resets the record without affecting subsequent execution.
    executor.clear_accessed_block_hashes();
    assert!(executor.get_accessed_block_hashes().is_empty(), "record should be empty after clear");

    // A transaction that never reads BLOCKHASH leaves the cleared record empty,
    // even though an earlier transaction in the same block read one.
    executor
        .execute_transaction(&create_transaction(1, PLAIN_CONTRACT))
        .expect("plain tx should succeed");
    assert!(
        executor.get_accessed_block_hashes().is_empty(),
        "a transaction without BLOCKHASH reads must not repopulate the record"
    );

    // A fresh BLOCKHASH read after the clear is attributed to that transaction.
    executor
        .execute_transaction(&create_transaction(2, BLOCKHASH_CONTRACT))
        .expect("second blockhash tx should succeed");
    assert_eq!(executor.get_accessed_block_hashes().len(), 1);
}
