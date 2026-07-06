//! End-to-end tests for the Rex6 `SequencerRegistry` rotation hardening.
//!
//! Covers the block-executor-level scenarios the unit tests in
//! `system/sequencer_registry.rs` cannot:
//!
//! - Fresh Rex6 bootstrap deploys v2.0.0 with the seeded minimum rotation delay and system address
//!   resolution keeps working.
//! - The in-place v1.0.0 → v2.0.0 upgrade preserves live storage, and a rotation scheduled under
//!   v1.0.0 (no possession proof existed back then) whose activation lands exactly on the Rex6
//!   activation block still flips normally in that same block.
//! - A full v2.0.0 rotation round trip: the admin schedules with a real EIP-712 possession proof
//!   via a normal transaction, and the rotation activates in a later block. The digest is built in
//!   Rust, independently of the contract's Solidity implementation, so the test also cross-checks
//!   the EIP-712 domain against the deployed bytecode.

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, keccak256, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::{SolCall, SolValue};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, ISequencerRegistry, MegaBlockExecutionCtx,
    MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId,
    MegaTxEnvelope, SequencerRegistryConfig, SequencerRegistryRex6Config, TestExternalEnvs,
    MEGA_SYSTEM_ADDRESS, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    SEQUENCER_REGISTRY_CODE_HASH, SEQUENCER_REGISTRY_CODE_HASH_REX6,
};
use mega_system_contracts::sequencer_registry::storage_slots::{
    ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, MIN_ROTATION_DELAY, PENDING_SEQUENCER,
    SEQUENCER_ACTIVATION_BLOCK,
};
use revm::{
    context::BlockEnv,
    database::State,
    state::{AccountInfo, Bytecode},
};

const CHAIN_ID: u64 = 8453;
const BOOTSTRAP_SEQUENCER: Address = address!("0x4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address = address!("0x5000000000000000000000000000000000000005");
const MIN_ROTATION_DELAY_BLOCKS: u64 = 100;

fn sequencer_registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
        rex5_initial_admin: BOOTSTRAP_ADMIN,
    }
}

fn rex6_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex6, ForkCondition::Timestamp(0))
        .with_params(sequencer_registry_config())
        .with_params(SequencerRegistryRex6Config {
            rex6_min_rotation_delay: MIN_ROTATION_DELAY_BLOCKS,
        })
}

fn evm_env_at_block(block_number: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX6;
    cfg_env.chain_id = CHAIN_ID;
    let block_env = BlockEnv {
        number: U256::from(block_number),
        timestamp: U256::from(1_800_000_000u64),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), BlockLimits::no_limits())
}

fn executor_factory() -> MegaBlockExecutorFactory<
    MegaHardforkConfig,
    MegaEvmFactory<TestExternalEnvs<Infallible>>,
    OpAlloyReceiptBuilder,
> {
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    MegaBlockExecutorFactory::new(rex6_chain_spec(), evm_factory, OpAlloyReceiptBuilder::default())
}

fn create_tx_from(
    sender: Address,
    nonce: u64,
    target: Address,
    data: Bytes,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 0,
        gas_limit: 1_000_000,
        to: TxKind::Call(target),
        value: U256::ZERO,
        input: data,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    alloy_consensus::transaction::Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), sender)
}

fn read_registry_slot(state: &mut State<&mut MemoryDatabase>, slot: U256) -> U256 {
    revm::Database::storage(state, SEQUENCER_REGISTRY_ADDRESS, slot).expect("read registry slot")
}

fn registry_code_hash(state: &mut State<&mut MemoryDatabase>) -> B256 {
    revm::Database::basic(state, SEQUENCER_REGISTRY_ADDRESS)
        .expect("read registry account")
        .expect("registry account must exist")
        .code_hash
}

/// Builds the EIP-712 digest for `SequencerRotation(address newSequencer,uint256
/// activationBlock)` against the registry's domain, mirroring the Solidity computation
/// independently so the test cross-checks the deployed bytecode.
fn rotation_digest(new_sequencer: Address, activation_block: U256) -> B256 {
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let rotation_typehash =
        keccak256(b"SequencerRotation(address newSequencer,uint256 activationBlock)");
    let domain_separator = keccak256(
        (
            domain_typehash,
            keccak256(b"MegaETH SequencerRegistry"),
            keccak256(b"1"),
            U256::from(CHAIN_ID),
            SEQUENCER_REGISTRY_ADDRESS,
        )
            .abi_encode(),
    );
    let struct_hash = keccak256((rotation_typehash, new_sequencer, activation_block).abi_encode());

    let mut preimage = Vec::with_capacity(2 + 32 + 32);
    preimage.extend_from_slice(b"\x19\x01");
    preimage.extend_from_slice(domain_separator.as_slice());
    preimage.extend_from_slice(struct_hash.as_slice());
    keccak256(&preimage)
}

/// A fixed secp256k1 keypair for the rotated-in sequencer; the address is recovered from a
/// probe signature so no extra signing-crate dev-dependency is needed.
fn new_sequencer_keypair() -> (Address, B256) {
    let secret = B256::from(U256::from(0x5ec5ec5ec5ecu64));
    (recover_probe_address(secret), secret)
}

fn recover_probe_address(secret: B256) -> Address {
    let probe = B256::from(U256::from(1));
    let sig = alloy_consensus::crypto::secp256k1::sign_message(secret, probe)
        .expect("probe signing must succeed");
    alloy_consensus::crypto::secp256k1::recover_signer(&sig, probe)
        .expect("probe recovery must succeed")
}

/// Signs the rotation digest with `secret` and returns the 65-byte `(r, s, v)` blob the
/// contract expects.
fn sign_rotation(secret: B256, new_sequencer: Address, activation_block: U256) -> Bytes {
    let digest = rotation_digest(new_sequencer, activation_block);
    let sig = alloy_consensus::crypto::secp256k1::sign_message(secret, digest)
        .expect("rotation signing must succeed");
    let mut blob = Vec::with_capacity(65);
    blob.extend_from_slice(&sig.r().to_be_bytes::<32>());
    blob.extend_from_slice(&sig.s().to_be_bytes::<32>());
    blob.push(27 + sig.v() as u8);
    Bytes::from(blob)
}

/// Seeds a live v1.0.0 registry as it would exist on a Rex5 chain right before the Rex6
/// upgrade: bootstrap roles plus a pending sequencer rotation scheduled without any proof.
fn seed_v1_registry_with_pending_rotation(
    db: &mut MemoryDatabase,
    pending_sequencer: Address,
    activation_block: u64,
) {
    db.insert_account_info(
        SEQUENCER_REGISTRY_ADDRESS,
        AccountInfo {
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        },
    );
    for (slot, value) in [
        (CURRENT_SYSTEM_ADDRESS, U256::from_be_bytes(MEGA_SYSTEM_ADDRESS.into_word().0)),
        (CURRENT_SEQUENCER, U256::from_be_bytes(BOOTSTRAP_SEQUENCER.into_word().0)),
        (ADMIN, U256::from_be_bytes(BOOTSTRAP_ADMIN.into_word().0)),
        (PENDING_SEQUENCER, U256::from_be_bytes(pending_sequencer.into_word().0)),
        (SEQUENCER_ACTIVATION_BLOCK, U256::from(activation_block)),
    ] {
        db.insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, slot, value).unwrap();
    }
}

/// Fresh chain at Rex6: the pre-block deploy installs v2.0.0 directly with the full seed
/// (including `_minRotationDelay`), and system-address resolution succeeds against the v2
/// code hash.
#[test]
fn test_rex6_bootstrap_deploys_v2_and_resolves_system_address() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let mut executor =
        executor_factory().create_executor(&mut state, block_ctx(), evm_env_at_block(1000));
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");
    drop(executor);

    assert_eq!(registry_code_hash(&mut state), SEQUENCER_REGISTRY_CODE_HASH_REX6);
    assert_eq!(
        read_registry_slot(&mut state, MIN_ROTATION_DELAY),
        U256::from(MIN_ROTATION_DELAY_BLOCKS),
        "_minRotationDelay must be seeded on the fresh Rex6 bootstrap"
    );
    assert_eq!(
        read_registry_slot(&mut state, CURRENT_SEQUENCER),
        U256::from_be_bytes(BOOTSTRAP_SEQUENCER.into_word().0),
    );
}

/// The grandfathering boundary case: a rotation scheduled under v1.0.0 whose activation block
/// is exactly the Rex6 activation block. In that single block the executor must both upgrade
/// the bytecode in place (preserving all live storage) and apply the due rotation.
#[test]
fn test_rex6_upgrade_block_applies_v1_scheduled_rotation() {
    let (new_sequencer, _) = new_sequencer_keypair();
    let activation_block = 1000u64;

    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    seed_v1_registry_with_pending_rotation(&mut db, new_sequencer, activation_block);

    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = executor_factory().create_executor(
        &mut state,
        block_ctx(),
        evm_env_at_block(activation_block),
    );
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");
    drop(executor);

    // The upgrade landed...
    assert_eq!(
        registry_code_hash(&mut state),
        SEQUENCER_REGISTRY_CODE_HASH_REX6,
        "registry must run v2.0.0 after the Rex6 activation block"
    );
    assert_eq!(
        read_registry_slot(&mut state, MIN_ROTATION_DELAY),
        U256::from(MIN_ROTATION_DELAY_BLOCKS),
    );
    // ...the grandfathered rotation flipped in the same block...
    assert_eq!(
        read_registry_slot(&mut state, CURRENT_SEQUENCER),
        U256::from_be_bytes(new_sequencer.into_word().0),
        "the v1-scheduled rotation must activate normally across the upgrade"
    );
    assert_eq!(read_registry_slot(&mut state, PENDING_SEQUENCER), U256::ZERO);
    assert_eq!(read_registry_slot(&mut state, SEQUENCER_ACTIVATION_BLOCK), U256::ZERO);
    // ...and untouched live storage survived the upgrade.
    assert_eq!(
        read_registry_slot(&mut state, ADMIN),
        U256::from_be_bytes(BOOTSTRAP_ADMIN.into_word().0),
        "live admin slot must survive the in-place upgrade"
    );
    assert_eq!(
        read_registry_slot(&mut state, CURRENT_SYSTEM_ADDRESS),
        U256::from_be_bytes(MEGA_SYSTEM_ADDRESS.into_word().0),
    );
}

/// Full v2.0.0 round trip through normal transactions: the admin schedules a rotation with
/// the new key's EIP-712 possession proof in one block, and the rotation activates in a
/// later block's pre-execution phase.
#[test]
fn test_rex6_rotation_with_valid_proof_end_to_end() {
    let (new_sequencer, secret) = new_sequencer_keypair();
    let schedule_block = 1000u64;
    let activation_block = schedule_block + MIN_ROTATION_DELAY_BLOCKS;

    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(BOOTSTRAP_ADMIN, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let factory = executor_factory();

    // Block 1000: bootstrap deploy + admin schedules the rotation with a valid proof.
    let mut executor =
        factory.create_executor(&mut state, block_ctx(), evm_env_at_block(schedule_block));
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    let calldata = ISequencerRegistry::scheduleNextSequencerChangeCall {
        newSequencer: new_sequencer,
        activationBlock: U256::from(activation_block),
        newSequencerSignature: sign_rotation(secret, new_sequencer, U256::from(activation_block)),
    }
    .abi_encode();
    let tx = create_tx_from(BOOTSTRAP_ADMIN, 0, SEQUENCER_REGISTRY_ADDRESS, Bytes::from(calldata));
    executor.execute_transaction(&tx).expect("schedule tx should be accepted");
    drop(executor);

    assert_eq!(
        read_registry_slot(&mut state, PENDING_SEQUENCER),
        U256::from_be_bytes(new_sequencer.into_word().0),
        "the proof-carrying schedule call must record the pending rotation"
    );
    assert_eq!(
        read_registry_slot(&mut state, SEQUENCER_ACTIVATION_BLOCK),
        U256::from(activation_block),
    );

    // Activation block: the pre-execution system call applies the rotation.
    let mut executor =
        factory.create_executor(&mut state, block_ctx(), evm_env_at_block(activation_block));
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");
    drop(executor);

    assert_eq!(
        read_registry_slot(&mut state, CURRENT_SEQUENCER),
        U256::from_be_bytes(new_sequencer.into_word().0),
        "the rotation must flip at its activation block"
    );
    assert_eq!(read_registry_slot(&mut state, PENDING_SEQUENCER), U256::ZERO);
}

/// The hardened entry point rejects a schedule without a possession proof: the transaction
/// executes but the call reverts and no pending rotation is recorded.
#[test]
fn test_rex6_schedule_without_proof_reverts() {
    let (new_sequencer, _) = new_sequencer_keypair();

    let mut db = MemoryDatabase::default();
    db.set_account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(BOOTSTRAP_ADMIN, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let mut executor =
        executor_factory().create_executor(&mut state, block_ctx(), evm_env_at_block(1000));
    executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

    let calldata = ISequencerRegistry::scheduleNextSequencerChangeCall {
        newSequencer: new_sequencer,
        activationBlock: U256::from(1000 + MIN_ROTATION_DELAY_BLOCKS),
        newSequencerSignature: Bytes::new(),
    }
    .abi_encode();
    let tx = create_tx_from(BOOTSTRAP_ADMIN, 0, SEQUENCER_REGISTRY_ADDRESS, Bytes::from(calldata));
    // The executor accepts the transaction; the contract call itself reverts, so the pending
    // rotation must remain unset.
    executor.execute_transaction(&tx).expect("tx should be accepted even though the call reverts");
    drop(executor);

    assert_eq!(
        read_registry_slot(&mut state, PENDING_SEQUENCER),
        U256::ZERO,
        "a schedule without a possession proof must not record a pending rotation"
    );
}
