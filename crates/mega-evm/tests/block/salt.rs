//! A block whose own bookkeeping lands in a crowded SALT region.
//!
//! The two pre-block calls write into whatever part of the trie the EIP-2935 and EIP-4788
//! contracts live in, and the protocol has no say in how crowded that part is. Both calls are
//! system-originated, so they price at the minimum bucket and read no capacity at all: a block
//! cannot fail to start because the region its own history buffers live in grew.
//!
//! The control arm is a user transaction in the same block: it pays the crowded price, and at
//! the capacity these tests use it cannot afford it — so the environment the two calls ran
//! against really is crowded.

use alloy_eips::{
    eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{block::BlockExecutor, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Bytes, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockGasCounters, BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvm,
    MegaEvmFactory, MegaHardforkConfig, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{database::State, inspector::NoOpInspector, Database};

use crate::common::{self, BLOCK_NUMBER, BLOCK_TIMESTAMP, CONTRACT};

/// The parent hash the EIP-2935 call records. Non-zero, so the call really writes a fresh slot.
const PARENT_HASH: B256 = B256::repeat_byte(0x29);

/// The parent beacon block root the EIP-4788 call records, non-zero for the same reason.
const PARENT_BEACON_ROOT: B256 = B256::repeat_byte(0x47);

/// The ring buffer the EIP-4788 contract keeps its two arrays in, as its own bytecode counts it.
/// The timestamp lands at `timestamp % LENGTH` and the root one whole buffer further on.
const BEACON_ROOTS_BUFFER_LENGTH: u64 = 8191;

/// The capacity the crowded arm puts every bucket at. A slot's first write would cost this many
/// times the schedule's own entry, far beyond what any transaction of this block brought.
const HEAVY: u64 = 100_000;

/// The gas limit the control arm's user transaction carries: plenty at the minimum bucket, and
/// nowhere near enough at [`HEAVY`]. 1,000,000 of regular gas on top of what the slot, its record
/// and the body cost at the minimum bucket and the byte prices in effect.
fn user_gas_limit() -> u64 {
    1_000_000 +
        common::slot_state_gas() +
        common::body_history(0) +
        mega_evm::write_record_history_gas(1).expect("a record has a price")
}

/// The external environments of these tests: every bucket at `m` times the minimum, so the
/// capacity bites wherever a charge lands.
type SaltEnvs = TestExternalEnvs;

/// The EVM a crowded block runs on.
type SaltEvm<'a> = MegaEvm<&'a mut State<MemoryDatabase>, NoOpInspector, SaltEnvs>;

/// The executor these tests drive.
type SaltExecutor<'a> = MegaBlockExecutor<SaltEvm<'a>, OpAlloyReceiptBuilder, MegaHardforkConfig>;

/// Every bucket at `m` times the minimum capacity.
fn envs_at(m: u64) -> SaltEnvs {
    TestExternalEnvs::new().with_default_bucket_capacity(MIN_BUCKET_SIZE as u64 * m)
}

/// A state holding the two pre-block contracts' own code and a contract a user transaction can
/// write a slot in.
fn state_with_contracts() -> State<MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone());
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
    );
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

/// An executor over `state` reading `envs`.
fn executor(state: &mut State<MemoryDatabase>, envs: SaltEnvs) -> SaltExecutor<'_> {
    let evm =
        MegaEvmFactory::new().with_external_env_factory(envs).create_evm(state, common::evm_env());
    MegaBlockExecutor::new(evm, ctx(), common::chain_spec(), OpAlloyReceiptBuilder::default())
}

/// The ring-buffer slot the EIP-2935 call writes the parent hash into.
fn history_slot() -> U256 {
    U256::from(BLOCK_NUMBER - 1) % U256::from(HISTORY_SERVE_WINDOW)
}

/// The ring-buffer slot the EIP-4788 call writes the beacon root into.
fn beacon_root_slot() -> U256 {
    U256::from(BLOCK_TIMESTAMP) % U256::from(BEACON_ROOTS_BUFFER_LENGTH) +
        U256::from(BEACON_ROOTS_BUFFER_LENGTH)
}

/// What one crowded block's pre-block phase left behind.
struct PreBlock {
    /// The parent hash the EIP-2935 contract holds, as committed.
    parent_hash: B256,
    /// The beacon root the EIP-4788 contract holds, as committed.
    beacon_root: B256,
    /// The block's three ledgers after the pre-block phase.
    counters: BlockGasCounters,
    /// How many bucket capacities the phase read.
    queries: u32,
}

/// Runs the pre-block phase of a block against an environment at `m` and reports what it left.
fn pre_block_at(m: u64) -> PreBlock {
    let envs = envs_at(m);
    let mut state = state_with_contracts();
    let counters = {
        let mut executor = executor(&mut state, envs.clone());
        executor.apply_pre_execution_changes().expect("the block starts");
        *executor.gas()
    };
    let mut read = |address, slot| {
        B256::from(state.storage(address, slot).expect("the slot is readable").to_be_bytes())
    };
    PreBlock {
        parent_hash: read(HISTORY_STORAGE_ADDRESS, history_slot()),
        beacon_root: read(BEACON_ROOTS_ADDRESS, beacon_root_slot()),
        counters,
        queries: envs.total_bucket_queries(),
    }
}

/// Both pre-block calls run and commit whatever the capacity of the region they write into, and
/// neither of them reads that capacity.
#[test]
fn test_the_pre_block_calls_commit_at_any_capacity() {
    let minimum = pre_block_at(1);
    let crowded = pre_block_at(HEAVY);

    for (m, block) in [(1, &minimum), (HEAVY, &crowded)] {
        assert_eq!(block.parent_hash, PARENT_HASH, "at m = {m}: the EIP-2935 write committed");
        assert_eq!(block.beacon_root, PARENT_BEACON_ROOT, "at m = {m}: and the EIP-4788 write");
        assert_eq!(block.queries, 0, "at m = {m}: a pre-block call reads no capacity at all");
    }
    assert_eq!(
        minimum.counters, crowded.counters,
        "the block's ledgers are the same at both capacities",
    );
}

/// The control: the very same block's first user transaction does pay the crowded price, and at
/// this capacity cannot afford the one slot it writes. Without this the arms above could be
/// passing because the environment was never crowded.
#[test]
fn test_a_user_transaction_in_the_crowded_block_pays_the_crowded_price() {
    // A slot that costs nothing costs nothing in a crowded bucket either.
    if common::state_is_free() {
        return;
    }
    let affordable = {
        let mut state = state_with_contracts();
        let mut executor = executor(&mut state, envs_at(1));
        executor.apply_pre_execution_changes().expect("the block starts");
        executor.run_transaction(&common::user_tx(0, user_gas_limit())).expect("it executes")
    };
    assert!(affordable.result.is_success(), "{:?}", affordable.result);
    assert_eq!(affordable.gas.state, common::slot_state_gas(), "the write is charged state gas");

    let envs = envs_at(HEAVY);
    let crowded = {
        let mut state = state_with_contracts();
        let mut executor = executor(&mut state, envs.clone());
        executor.apply_pre_execution_changes().expect("the block starts");
        executor.run_transaction(&common::user_tx(0, user_gas_limit())).expect("it executes")
    };
    assert!(
        crowded.result.is_halt(),
        "a user transaction cannot afford the crowded write: {:?}",
        crowded.result,
    );
    assert!(
        envs.total_bucket_queries() > 0,
        "and it is the user transaction, not the pre-block phase, that reads a capacity",
    );
}
