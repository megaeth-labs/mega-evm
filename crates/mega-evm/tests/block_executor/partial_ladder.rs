//! End-to-end tests for pre-block setup on partial and rollback hardfork ladders.
//!
//! Pre-block setup (system-contract predeploys, the EIP-2935/EIP-4788 fail-closed checks, the
//! `SequencerRegistry` bootstrap) is gated on the **activated-spec floor**
//! (`MegaHardforks::max_activated_spec_id`), not on per-fork registration and not on the
//! reversible executing spec. These tests pin both directions of that choice:
//!
//! - A *partial* ladder — a config scheduling a later fork without its predecessors — must still
//!   run every lower fork's setup, rather than silently skipping it.
//! - A *rollback* window — mainnet's `MiniRex1`, which maps back to `EQUIVALENCE` — must keep
//!   running the setup that earlier forks already activated, rather than retracting it.

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};

use alloy_evm::{
    block::{BlockExecutor, BlockValidationError, OnStateHook, StateChangeSource},
    EvmEnv,
};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, bytes, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory,
    MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaHardforks, MegaSpecId,
    SequencerRegistryConfig, SequencerRegistryRex6Config, TestExternalEnvs, ACCESS_CONTROL_ADDRESS,
    HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, KEYLESS_DEPLOY_ADDRESS, LIMIT_CONTROL_ADDRESS,
    ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH, ORACLE_CONTRACT_CODE_HASH_REX5,
    SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE_HASH_REX6,
};
use mega_system_contracts::sequencer_registry::storage_slots::MIN_ROTATION_DELAY;
use revm::{
    context::BlockEnv,
    database::{Database as _, State},
    state::{AccountInfo, Bytecode, EvmState},
};

const BLOCK_NUMBER: u64 = 1000;
const BLOCK_GAS_LIMIT: u64 = 30_000_000;
const MIN_ROTATION_DELAY_BLOCKS: u64 = 100;

const BOOTSTRAP_SEQUENCER: Address = address!("0x4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address = address!("0x5000000000000000000000000000000000000005");

/// Runtime bytecode that unconditionally reverts (`PUSH0 PUSH0 REVERT`). Installed at the
/// EIP-2935 history-storage address to force a failing pre-block system call.
///
/// A heavy SALT bucket — the mechanism `tests/rex5/pre_block_system_calls.rs` uses — cannot force
/// the failure here: the pre-block call's caller is `eip4788::SYSTEM_ADDRESS`, which counts as
/// system-originated, so from Rex6 the transaction is exempt from resource metering and its
/// storage writes are charged un-scaled. Bucket capacity no longer influences the outcome.
const ALWAYS_REVERT_CODE: Bytes = bytes!("0x5f5ffd");

fn sequencer_registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig {
        rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
        rex5_initial_admin: BOOTSTRAP_ADMIN,
    }
}

/// A partial ladder: Rex6 activates while Rex5 is registered but never activates, and no earlier
/// fork is registered at all. `Rex5` must be present as an entry to carry
/// [`SequencerRegistryConfig`] — parameter lookup is activation-independent, but `with_params`
/// requires the entry to exist.
fn rex6_only_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with(MegaHardfork::Rex5, ForkCondition::Never)
        .with_params(sequencer_registry_config())
        .with(MegaHardfork::Rex6, ForkCondition::Timestamp(0))
        .with_params(SequencerRegistryRex6Config {
            rex6_min_rotation_delay: MIN_ROTATION_DELAY_BLOCKS,
        })
}

fn create_evm_env(spec: MegaSpecId, timestamp: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    let block_env = BlockEnv {
        number: U256::from(BLOCK_NUMBER),
        timestamp: U256::from(timestamp),
        gas_limit: BLOCK_GAS_LIMIT,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

/// Non-zero parent hash and beacon root on a non-genesis block, so both EIP helpers actually
/// issue their system call rather than taking the genesis-skip early return.
fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        B256::from([0x29; 32]),
        Some(B256::from([0x47; 32])),
        Bytes::new(),
        BlockLimits::no_limits(),
    )
}

fn install_code(db: &mut MemoryDatabase, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_raw(code);
    db.insert_account_info(
        address,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );
}

fn install_eip_contracts(db: &mut MemoryDatabase) {
    install_code(
        db,
        alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
        alloy_eips::eip2935::HISTORY_STORAGE_CODE.clone(),
    );
    install_code(
        db,
        alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
        alloy_eips::eip4788::BEACON_ROOTS_CODE.clone(),
    );
}

/// Records every account observed by the on-state hook, so a test can assert that a read-only
/// pre-block outcome reached the witness path.
#[derive(Debug, Default, Clone)]
struct RecordingStateHook {
    accounts: Arc<Mutex<Vec<Address>>>,
}

impl OnStateHook for RecordingStateHook {
    fn on_state(&mut self, _source: StateChangeSource, state: &EvmState) {
        self.accounts.lock().unwrap().extend(state.keys().copied());
    }
}

/// A config that activates Rex6 without ever activating Rex5 must still materialize every
/// lower-spec predeploy — at the Rex5 Oracle version — and bootstrap the `SequencerRegistry` at
/// v2.0.0. Under per-fork gating none of this ran.
#[test]
fn test_partial_ladder_runs_lower_fork_setup() {
    let chain_spec = rex6_only_chain_spec();
    // Precondition: this really is a partial ladder (no activation event below Rex6), and the
    // executing spec is Rex6. The `is_*_active_at_timestamp` predicates cannot state this — they
    // project the activated-spec floor, which a partial ladder keeps high by design.
    assert_eq!(chain_spec.mega_fork_activation(MegaHardfork::Rex5), ForkCondition::Never);
    assert_eq!(chain_spec.mega_fork_activation(MegaHardfork::MiniRex), ForkCondition::Never);
    assert_eq!(chain_spec.spec_id(0), MegaSpecId::REX6);

    let mut db = MemoryDatabase::default();
    install_eip_contracts(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory =
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new());
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, OpAlloyReceiptBuilder::default());
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX6, 0),
    );

    executor.apply_pre_execution_changes().expect("partial-ladder pre-block setup must succeed");

    for address in [
        ORACLE_CONTRACT_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        KEYLESS_DEPLOY_ADDRESS,
        ACCESS_CONTROL_ADDRESS,
        LIMIT_CONTROL_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
    ] {
        let info = state.basic(address).unwrap();
        assert!(
            info.is_some_and(|i| i.code_hash != revm::primitives::KECCAK_EMPTY),
            "{address} must be deployed on a partial ladder"
        );
    }

    // The Oracle takes its Rex5 bytecode, and the registry its Rex6 bytecode with the seeded
    // rotation delay — i.e. the floor drives version selection too, not just the on/off gate.
    assert_eq!(
        state.basic(ORACLE_CONTRACT_ADDRESS).unwrap().unwrap().code_hash,
        ORACLE_CONTRACT_CODE_HASH_REX5
    );
    assert_eq!(
        state.basic(SEQUENCER_REGISTRY_ADDRESS).unwrap().unwrap().code_hash,
        SEQUENCER_REGISTRY_CODE_HASH_REX6
    );
    assert_eq!(
        state.storage(SEQUENCER_REGISTRY_ADDRESS, MIN_ROTATION_DELAY).unwrap(),
        U256::from(MIN_ROTATION_DELAY_BLOCKS)
    );
}

/// The Rex5 fail-closed rule on pre-block system calls must be enforced on a partial ladder too.
/// Under per-fork gating the Rex5 check was skipped and a reverting EIP-2935 call was silently
/// accepted.
///
/// Kept separate from the deployment test above: a pre-block EIP failure returns before the
/// predeploy loop is reached, so one block cannot assert both.
#[test]
fn test_partial_ladder_enforces_pre_block_fail_closed() {
    let mut db = MemoryDatabase::default();
    install_eip_contracts(&mut db);
    // Break EIP-2935 specifically.
    install_code(&mut db, alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS, ALWAYS_REVERT_CODE);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory =
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new());
    let block_executor_factory = MegaBlockExecutorFactory::new(
        rex6_only_chain_spec(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX6, 0),
    );

    let err = executor
        .apply_pre_execution_changes()
        .expect_err("a reverting EIP-2935 pre-block call must reject the block");
    let alloy_evm::block::BlockExecutionError::Validation(validation_err) = &err else {
        panic!("expected a validation error, got: {err:?}");
    };
    assert!(
        matches!(validation_err, BlockValidationError::BlockHashContractCall { .. }),
        "expected BlockHashContractCall, got: {validation_err:?}"
    );
    // Assert the *specific* failure. Both gating styles reject this block, but for different
    // reasons: under per-fork gating the Rex5 check is skipped and the reverting call is
    // accepted, only for `resolve_system_address` to later trip over the registry that was
    // never deployed. Matching on the error variant alone would pass either way.
    assert!(
        format!("{validation_err:?}").contains("EIP-2935 pre-block system call did not succeed"),
        "block must be rejected by the EIP-2935 fail-closed check itself, got: {validation_err:?}"
    );
}

/// A config that activates Rex6 with no Rex5 entry at all cannot carry the
/// [`SequencerRegistryConfig`] the Rex5 bootstrap requires. Pre-block setup must fail closed
/// with a configuration error rather than silently skipping the registry.
///
/// This drives `pre_execution_changes` directly rather than `apply_pre_execution_changes`: the
/// latter also runs `resolve_system_address`, which gates on the executing spec and so raises the
/// same error under either gating style. Only the pre-block setup step distinguishes them —
/// under per-fork gating it returns `Ok`, having quietly skipped the registry entirely.
#[test]
fn test_rex6_without_any_rex5_entry_fails_closed() {
    let chain_spec = MegaHardforkConfig::default()
        .with(MegaHardfork::Rex6, ForkCondition::Timestamp(0))
        .with_params(SequencerRegistryRex6Config {
            rex6_min_rotation_delay: MIN_ROTATION_DELAY_BLOCKS,
        });

    let mut db = MemoryDatabase::default();
    install_eip_contracts(&mut db);
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory =
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new());
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, OpAlloyReceiptBuilder::default());
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::REX6, 0),
    );

    let err = executor
        .pre_execution_changes()
        .expect_err("Rex5 setup without its config must fail rather than be skipped");
    assert!(
        format!("{err:?}").contains("SequencerRegistryConfig not configured"),
        "expected the missing-config error, got: {err:?}"
    );
}

/// Mainnet's `MiniRex1` window rolls the executing spec back to `EQUIVALENCE` while `MiniRex`'s
/// predeploys remain installed. Pre-block setup must keep emitting their read-only outcomes:
/// dropping them would remove the accounts from the on-state hook that feeds the stateless
/// witness and the state-sync transition shard — a replay-observable change on frozen history.
#[test]
fn test_rollback_window_still_emits_predeploy_witness_entries() {
    let chain_spec = mega_evm::mainnet_hardforks();
    let ForkCondition::Timestamp(timestamp) =
        chain_spec.mega_fork_activation(MegaHardfork::MiniRex1)
    else {
        panic!("mainnet must schedule MiniRex1 by timestamp");
    };
    // Precondition: inside the window the scheduled spec is the alias rung and its behavior
    // projects back to EQUIVALENCE.
    assert_eq!(chain_spec.spec_id(timestamp), MegaSpecId::MINI_REX_1);
    assert_eq!(chain_spec.spec_id(timestamp).behavior(), MegaSpecId::EQUIVALENCE);

    let mut db = MemoryDatabase::default();
    install_eip_contracts(&mut db);
    // Both MiniRex predeploys are already installed, as they would be on mainnet by then, so the
    // deploy takes its idempotent read-only path.
    install_code(&mut db, ORACLE_CONTRACT_ADDRESS, mega_evm::ORACLE_CONTRACT_CODE);
    install_code(
        &mut db,
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        mega_evm::HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
    );
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory =
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new());
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, OpAlloyReceiptBuilder::default());
    let mut executor = block_executor_factory.create_executor(
        &mut state,
        block_ctx(),
        create_evm_env(MegaSpecId::MINI_REX_1, timestamp),
    );

    let recorder = RecordingStateHook::default();
    let accounts = recorder.accounts.clone();
    BlockExecutor::set_state_hook(&mut executor, Some(Box::new(recorder)));
    executor.apply_pre_execution_changes().expect("rollback-window pre-block setup must succeed");

    let observed = accounts.lock().unwrap();
    for address in [ORACLE_CONTRACT_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS] {
        assert!(
            observed.contains(&address),
            "{address} must still reach the witness hook during the rollback window"
        );
    }

    // The rollback window is pre-Rex2, so the Oracle keeps its v1.0.0 bytecode.
    assert_eq!(
        state.basic(ORACLE_CONTRACT_ADDRESS).unwrap().unwrap().code_hash,
        ORACLE_CONTRACT_CODE_HASH
    );
}
