//! The pre-block calls a block makes before its transactions, and what each of them records.
//!
//! Every helper hands its state back rather than committing it, and the executor is what commits:
//! a call the fork makes is in the state the block's transactions run on, and a call it does not
//! make leaves that state alone.

use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    EvmEnv,
};
use alloy_hardforks::{EthereumHardfork, ForkCondition};
use alloy_primitives::{Bytes, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, EvmTxRuntimeLimits, MegaBlockExecutionCtx, MegaHardforkConfig, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALLDATALOAD, SSTORE},
    database::State,
    Database,
};

use crate::common::{self, executor_with_env, executor_with_env_and_spec, BLOCK_NUMBER};

/// The parent hash the EIP-2935 call records.
const PARENT_HASH: B256 = B256::repeat_byte(0xab);

/// The parent beacon block root the EIP-4788 call records.
const PARENT_BEACON_ROOT: B256 = B256::repeat_byte(0xcd);

/// Code that stores the word it was called with in slot zero, so a pre-block call to it leaves
/// what it was given behind.
fn recorder() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u32)
        .append(CALLDATALOAD)
        .push_number(0_u32)
        .append(SSTORE)
        .stop()
        .build()
}

/// A state whose two pre-block system contracts record what they are called with.
fn state_with_recorders() -> State<MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, recorder());
    db.set_account_code(BEACON_ROOTS_ADDRESS, recorder());
    State::builder().with_database(db).build()
}

/// The context of a block whose parent hash and parent beacon block root are the two above.
fn ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        PARENT_HASH,
        Some(PARENT_BEACON_ROOT),
        Bytes::new(),
        BlockLimits::no_limits(),
    )
}

/// The context of a block that carries no parent beacon block root.
fn ctx_without_beacon_root() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(PARENT_HASH, None, Bytes::new(), BlockLimits::no_limits())
}

/// The environment of a block at `number`.
fn env_at_block(number: u64) -> EvmEnv<MegaSpecId> {
    let mut env = common::evm_env();
    env.block_env.number = U256::from(number);
    env
}

/// What the two contracts hold in slot zero: the parent hash and the parent beacon block root,
/// as far as the pre-block calls recorded them.
fn recorded(state: &mut State<MemoryDatabase>) -> (B256, B256) {
    let parent_hash = state.storage(HISTORY_STORAGE_ADDRESS, U256::ZERO).expect("readable");
    let beacon_root = state.storage(BEACON_ROOTS_ADDRESS, U256::ZERO).expect("readable");
    (B256::from(parent_hash.to_be_bytes()), B256::from(beacon_root.to_be_bytes()))
}

/// Runs the pre-block calls of a block at `number` on the chain `spec` describes, and reports
/// what the two contracts recorded.
fn pre_block_calls(
    spec: MegaHardforkConfig,
    number: u64,
    ctx: MegaBlockExecutionCtx,
) -> (B256, B256) {
    let mut state = state_with_recorders();
    {
        let mut executor = executor_with_env_and_spec(&mut state, ctx, env_at_block(number), spec);
        executor.apply_pre_execution_changes().expect("the block starts");
    }
    recorded(&mut state)
}

/// Both calls run on a block of a chain past Prague and Cancun, and the executor commits what
/// they produced: the block's transactions see the parent hash and the parent beacon root.
#[test]
fn test_the_pre_block_calls_record_the_parent_hash_and_the_beacon_root() {
    assert_eq!(
        pre_block_calls(common::chain_spec(), BLOCK_NUMBER, ctx()),
        (PARENT_HASH, PARENT_BEACON_ROOT)
    );
}

/// The pre-block calls are the protocol's own work, held to no per-transaction limit: under
/// limits their bodies, their writes and their state gas each cross, both calls still record.
#[test]
fn test_the_pre_block_calls_are_held_to_no_limit() {
    let limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(0)
        .with_frame_data_size_limit(0)
        .with_tx_kv_update_limit(0)
        .with_frame_kv_update_limit(0)
        .with_tx_state_gas_limit(0);
    let ctx = MegaBlockExecutionCtx::new(
        PARENT_HASH,
        Some(PARENT_BEACON_ROOT),
        Bytes::new(),
        BlockLimits::no_limits().with_tx_runtime_limits(limits),
    );
    assert_eq!(
        pre_block_calls(common::chain_spec(), BLOCK_NUMBER, ctx),
        (PARENT_HASH, PARENT_BEACON_ROOT)
    );
}

/// A chain that has not reached Prague makes no block hashes call; the beacon root call is
/// unaffected.
#[test]
fn test_the_block_hashes_call_waits_for_prague() {
    let before_prague = common::chain_spec().with(EthereumHardfork::Prague, ForkCondition::Never);

    assert_eq!(
        pre_block_calls(before_prague, BLOCK_NUMBER, ctx()),
        (B256::ZERO, PARENT_BEACON_ROOT),
        "nothing was recorded for the parent hash"
    );
}

/// A chain that has not reached Cancun makes no beacon root call, and needs no root; the block
/// hashes call is unaffected.
#[test]
fn test_the_beacon_root_call_waits_for_cancun() {
    let before_cancun = common::chain_spec().with(EthereumHardfork::Cancun, ForkCondition::Never);

    assert_eq!(
        pre_block_calls(before_cancun, BLOCK_NUMBER, ctx_without_beacon_root()),
        (PARENT_HASH, B256::ZERO),
        "nothing was recorded for the beacon root"
    );
}

/// The genesis block makes neither call: it has no parent to record. Its parent beacon root is
/// zero, which is what EIP-4788 requires of it, and that is not an error.
#[test]
fn test_the_genesis_block_makes_no_pre_block_call() {
    let genesis = MegaBlockExecutionCtx::new(
        PARENT_HASH,
        Some(B256::ZERO),
        Bytes::new(),
        BlockLimits::no_limits(),
    );

    assert_eq!(
        pre_block_calls(common::chain_spec(), 0, genesis),
        (B256::ZERO, B256::ZERO),
        "neither contract was called"
    );
}

/// A genesis block carrying a non-zero parent beacon block root is refused, as EIP-4788
/// requires.
#[test]
fn test_the_genesis_block_refuses_a_non_zero_beacon_root() {
    let mut state = state_with_recorders();
    let mut executor = executor_with_env(&mut state, ctx(), env_at_block(0));

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("the genesis block's parent beacon block root must be zero");

    assert!(
        matches!(
            err,
            BlockExecutionError::Validation(
                BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
                    parent_beacon_block_root,
                },
            ) if parent_beacon_block_root == PARENT_BEACON_ROOT
        ),
        "{err}"
    );
}

/// A block of a chain past Cancun that carries no parent beacon block root is refused.
#[test]
fn test_a_block_without_a_parent_beacon_root_is_refused() {
    let mut state = state_with_recorders();
    let mut executor =
        executor_with_env(&mut state, ctx_without_beacon_root(), env_at_block(BLOCK_NUMBER));

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("a Cancun block carries a parent beacon block root");

    assert!(
        matches!(
            err,
            BlockExecutionError::Validation(BlockValidationError::MissingParentBeaconBlockRoot)
        ),
        "{err}"
    );
}
