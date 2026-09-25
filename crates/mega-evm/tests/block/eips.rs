//! The pre-block calls a block makes before its transactions, and what each of them records.
//!
//! Every helper hands its state back rather than committing it, and the executor is what commits:
//! a call the fork makes is in the state the block's transactions run on, and a call it does not
//! make leaves that state alone. A call that does not succeed refuses the block, and its state
//! reaches neither the observer nor the database.
//!
//! Each call runs on the pre-block budget: the block's gas limit, and never less than 30M, of
//! which at most 30M is regular gas and the rest the call's state-gas reservoir.

use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    EvmEnv, EvmFactory,
};
use alloy_hardforks::{EthereumHardfork, ForkCondition};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Address, Bytes, B256, U256};
use mega_evm::{
    constants::SLOT_STATE_GAS,
    pre_block_call_gas_limit,
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    BlockLimits, EvmTxRuntimeLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, PreBlockStateSource,
};
use revm::{
    bytecode::opcode::{CALLDATALOAD, MLOAD, SSTORE},
    database::State,
    handler::SYSTEM_CALL_REGULAR_GAS_LIMIT,
    Database,
};

use crate::common::{
    self, executor_with_env, executor_with_env_and_spec, pre_block_states, record_pre_block,
    BLOCK_NUMBER,
};

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

/// The pre-block calls a block at `number` on the chain `spec` describes handed to the observer,
/// in order, and whether the block started.
fn observed_calls(
    spec: MegaHardforkConfig,
    number: u64,
    ctx: MegaBlockExecutionCtx,
) -> (Vec<PreBlockStateSource>, Result<(), BlockExecutionError>) {
    let mut state = state_with_recorders();
    let mut executor = executor_with_env_and_spec(&mut state, ctx, env_at_block(number), spec);
    let log = record_pre_block(&mut executor);
    let started = executor.apply_pre_execution_changes();
    let calls = pre_block_states(&log)
        .into_iter()
        .map(|(source, _)| source)
        .filter(|source| {
            matches!(source, PreBlockStateSource::Eip2935 | PreBlockStateSource::Eip4788)
        })
        .collect();
    (calls, started)
}

/// The environment of a block at [`BLOCK_NUMBER`] whose gas limit is `gas_limit`.
fn env_with_gas_limit(gas_limit: u64) -> EvmEnv<MegaSpecId> {
    let mut env = env_at_block(BLOCK_NUMBER);
    env.block_env.gas_limit = gas_limit;
    env
}

/// Code whose one instruction expands memory past what 30M of regular gas pays for: it halts out
/// of gas on the regular budget, however much reservoir the call carries.
fn memory_hog() -> Bytes {
    BytecodeBuilder::default().push_number(0x40_0000_u32).append(MLOAD).stop().build()
}

/// Code that writes `slots` fresh storage slots, `SSTORE(i, 1)` for each.
fn fresh_writes(slots: u16) -> Bytes {
    (0..slots)
        .fold(BytecodeBuilder::default(), |code, slot| code.sstore(U256::from(slot), U256::from(1)))
        .stop()
        .build()
}

/// Starts a block at [`BLOCK_NUMBER`] in `env` over `db` and reports the pre-block calls the
/// observer received, and how the block's start ended.
fn start_block(
    db: MemoryDatabase,
    env: EvmEnv<MegaSpecId>,
) -> (Vec<PreBlockStateSource>, Result<(), BlockExecutionError>) {
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor_with_env(&mut state, ctx(), env);
    let log = record_pre_block(&mut executor);
    let started = executor.apply_pre_execution_changes();
    let calls = pre_block_states(&log)
        .into_iter()
        .map(|(source, _)| source)
        .filter(|source| {
            matches!(source, PreBlockStateSource::Eip2935 | PreBlockStateSource::Eip4788)
        })
        .collect();
    (calls, started)
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
        pre_block_calls(before_prague.clone(), BLOCK_NUMBER, ctx()),
        (B256::ZERO, PARENT_BEACON_ROOT),
        "nothing was recorded for the parent hash"
    );

    let (calls, started) = observed_calls(before_prague, BLOCK_NUMBER, ctx());
    started.expect("a block before Prague starts");
    assert_eq!(calls, [PreBlockStateSource::Eip4788], "no block hashes call reached the observer");
}

/// A chain that has not reached Cancun makes no beacon root call, and needs no root; the block
/// hashes call is unaffected.
#[test]
fn test_the_beacon_root_call_waits_for_cancun() {
    let before_cancun = common::chain_spec().with(EthereumHardfork::Cancun, ForkCondition::Never);

    assert_eq!(
        pre_block_calls(before_cancun.clone(), BLOCK_NUMBER, ctx_without_beacon_root()),
        (PARENT_HASH, B256::ZERO),
        "nothing was recorded for the beacon root"
    );

    let (calls, started) = observed_calls(before_cancun, BLOCK_NUMBER, ctx_without_beacon_root());
    started.expect("a block before Cancun starts");
    assert_eq!(calls, [PreBlockStateSource::Eip2935], "no beacon root call reached the observer");
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
        pre_block_calls(common::chain_spec(), 0, genesis.clone()),
        (B256::ZERO, B256::ZERO),
        "neither contract was called"
    );

    let (calls, started) = observed_calls(common::chain_spec(), 0, genesis);
    started.expect("the genesis block starts");
    assert!(calls.is_empty(), "no pre-block call reached the observer: {calls:?}");
}

/// A genesis block carrying a non-zero parent beacon block root is refused, as EIP-4788
/// requires. The block hashes call makes no call at genesis, so nothing reached the observer.
#[test]
fn test_the_genesis_block_refuses_a_non_zero_beacon_root() {
    let (calls, started) = observed_calls(common::chain_spec(), 0, ctx());
    let err = started.expect_err("the genesis block's parent beacon block root must be zero");
    assert!(calls.is_empty(), "{calls:?}");

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

/// A block of a chain past Cancun that carries no parent beacon block root is refused. The block
/// hashes call ran first, and succeeded.
#[test]
fn test_a_block_without_a_parent_beacon_root_is_refused() {
    let (calls, started) =
        observed_calls(common::chain_spec(), BLOCK_NUMBER, ctx_without_beacon_root());
    let err = started.expect_err("a Cancun block carries a parent beacon block root");
    assert_eq!(calls, [PreBlockStateSource::Eip2935]);

    assert!(
        matches!(
            err,
            BlockExecutionError::Validation(BlockValidationError::MissingParentBeaconBlockRoot)
        ),
        "{err}"
    );
}

/// With the contracts' own code, both calls succeed, reach the observer in order and are
/// committed: the parent hash in the EIP-2935 ring buffer, the timestamp and the root in the
/// EIP-4788 one.
#[test]
fn test_the_successful_pre_block_calls_commit_normally() {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone());
    let mut state = State::builder().with_database(db).build();
    let mut executor = executor_with_env(&mut state, ctx(), env_at_block(BLOCK_NUMBER));
    let log = record_pre_block(&mut executor);
    executor.apply_pre_execution_changes().expect("both calls succeed");
    let calls: Vec<_> = pre_block_states(&log).into_iter().map(|(source, _)| source).collect();
    assert_eq!(&calls[..2], [PreBlockStateSource::Eip2935, PreBlockStateSource::Eip4788]);
    drop(executor);

    let ring = 8191;
    let parent = state.storage(HISTORY_STORAGE_ADDRESS, U256::from(BLOCK_NUMBER - 1)).unwrap();
    assert_eq!(B256::from(parent.to_be_bytes()), PARENT_HASH);
    let timestamp_slot = U256::from(common::BLOCK_TIMESTAMP % ring);
    let root_slot = timestamp_slot + U256::from(ring);
    assert_eq!(
        state.storage(BEACON_ROOTS_ADDRESS, timestamp_slot).unwrap(),
        U256::from(common::BLOCK_TIMESTAMP)
    );
    assert_eq!(
        B256::from(state.storage(BEACON_ROOTS_ADDRESS, root_slot).unwrap().to_be_bytes()),
        PARENT_BEACON_ROOT
    );
}

/// A block hashes call that halts refuses the block with the EIP-2935 error, and its state
/// reaches neither the observer nor the database; the beacon root call after it never runs. It
/// halts on regular gas, which is at most 30M however large the block: the pre-block budget's
/// part above 30M is reservoir, and no regular charge draws it.
#[test]
fn test_a_block_is_refused_when_its_block_hashes_call_halts() {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, memory_hog());
    db.set_account_code(BEACON_ROOTS_ADDRESS, recorder());
    let (calls, started) = start_block(db, env_with_gas_limit(250_000_000));

    let err = started.expect_err("a halted block hashes call refuses the block");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BlockHashContractCall { message })
                if message.contains("did not succeed") && message.contains("OutOfGas")
        ),
        "{err}"
    );
    assert!(calls.is_empty(), "{calls:?}");
}

/// A beacon root call that halts refuses the block with the EIP-4788 error, naming the root. The
/// block hashes call before it succeeded: its contract has no code.
#[test]
fn test_a_block_is_refused_when_its_beacon_root_call_halts() {
    let mut db = common::database();
    db.set_account_code(BEACON_ROOTS_ADDRESS, memory_hog());
    let (calls, started) = start_block(db, env_with_gas_limit(250_000_000));

    let err = started.expect_err("a halted beacon root call refuses the block");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BeaconRootContractCall {
                parent_beacon_block_root,
                message,
            }) if **parent_beacon_block_root == PARENT_BEACON_ROOT
                && message.contains("did not succeed")
        ),
        "{err}"
    );
    assert_eq!(calls, [PreBlockStateSource::Eip2935]);
}

/// A pre-block call that reverts refuses the block just as one that halts does.
#[test]
fn test_a_block_is_refused_when_a_pre_block_call_reverts() {
    let reverts = BytecodeBuilder::default().revert().build();

    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, reverts.clone());
    let (calls, started) = start_block(db, common::evm_env());
    let err = started.expect_err("a reverted block hashes call refuses the block");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BlockHashContractCall { message })
                if message.contains("Revert")
        ),
        "{err}"
    );
    assert!(calls.is_empty(), "{calls:?}");

    let mut db = common::database();
    db.set_account_code(BEACON_ROOTS_ADDRESS, reverts);
    let (calls, started) = start_block(db, common::evm_env());
    let err = started.expect_err("a reverted beacon root call refuses the block");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BeaconRootContractCall { .. })
        ),
        "{err}"
    );
    assert_eq!(calls, [PreBlockStateSource::Eip2935]);
}

/// The pre-block budget follows the block's gas limit, so a pre-block call that costs more than
/// 30M in all still succeeds in a block that can hold it: 400 fresh slots are 39M of state gas,
/// which the reservoir above 30M pays. In a block of 30M the call has no reservoir, the state
/// gas spills onto the 30M of regular gas, and the call runs out of it.
#[test]
fn test_the_block_aware_budget_accepts_a_pre_block_call_above_30m() {
    const SLOTS: u16 = 400;
    assert!(u64::from(SLOTS) * SLOT_STATE_GAS > SYSTEM_CALL_REGULAR_GAS_LIMIT);
    assert_eq!(pre_block_call_gas_limit(250_000_000), 250_000_000);
    assert_eq!(pre_block_call_gas_limit(1_000_000), SYSTEM_CALL_REGULAR_GAS_LIMIT);

    let database = || {
        let mut db = common::database();
        db.set_account_code(HISTORY_STORAGE_ADDRESS, fresh_writes(SLOTS));
        db.set_account_code(BEACON_ROOTS_ADDRESS, fresh_writes(SLOTS));
        db
    };

    let mut state = State::builder().with_database(database()).build();
    let mut executor = executor_with_env(&mut state, ctx(), env_with_gas_limit(250_000_000));
    let log = record_pre_block(&mut executor);
    executor.apply_pre_execution_changes().expect("a 250M block holds both calls");
    let states = pre_block_states(&log);
    assert_eq!(states[0].0, PreBlockStateSource::Eip2935);
    assert_eq!(states[0].1[&HISTORY_STORAGE_ADDRESS].storage.len(), usize::from(SLOTS));
    assert_eq!(states[1].0, PreBlockStateSource::Eip4788);
    assert_eq!(states[1].1[&BEACON_ROOTS_ADDRESS].storage.len(), usize::from(SLOTS));
    drop(executor);

    let (calls, started) = start_block(database(), common::evm_env());
    assert_eq!(common::BLOCK_GAS_LIMIT, SYSTEM_CALL_REGULAR_GAS_LIMIT);
    let err = started.expect_err("a 30M block gives the call no reservoir");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BlockHashContractCall { message })
                if message.contains("OutOfGas")
        ),
        "{err}"
    );
    assert!(calls.is_empty(), "{calls:?}");
}

/// Starts a block at [`BLOCK_NUMBER`] over `db`.
fn start_block_over(db: ErrorInjectingDatabase) -> Result<(), BlockExecutionError> {
    let mut state = State::builder().with_database(db).build();
    let evm = MegaEvmFactory::new().create_evm(&mut state, env_at_block(BLOCK_NUMBER));
    let mut executor =
        MegaBlockExecutor::new(evm, ctx(), common::chain_spec(), OpAlloyReceiptBuilder::default());
    executor.apply_pre_execution_changes()
}

/// A database that fails to load `address`, over contracts that record what they are called
/// with.
fn failing_on(address: Address) -> ErrorInjectingDatabase {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, recorder());
    db.set_account_code(BEACON_ROOTS_ADDRESS, recorder());
    let mut db = ErrorInjectingDatabase::new(db);
    db.fail_on_account = Some(address);
    db
}

/// A database error in the block hashes call refuses the block with the EIP-2935 error, carrying
/// the database's message.
#[test]
fn test_blockhashes_pre_block_call_db_error_returns_validation_error() {
    let err = start_block_over(failing_on(HISTORY_STORAGE_ADDRESS))
        .expect_err("a failed read refuses the block");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BlockHashContractCall { message })
                if message.contains("injected basic() error")
        ),
        "{err}"
    );
}

/// A database error in the beacon root call refuses the block with the EIP-4788 error; the block
/// hashes call before it read what it needed.
#[test]
fn test_beacon_root_pre_block_call_db_error_returns_validation_error() {
    let err = start_block_over(failing_on(BEACON_ROOTS_ADDRESS))
        .expect_err("a failed read refuses the block");
    assert!(
        matches!(
            &err,
            BlockExecutionError::Validation(BlockValidationError::BeaconRootContractCall {
                message,
                ..
            }) if message.contains("injected basic() error")
        ),
        "{err}"
    );
}
