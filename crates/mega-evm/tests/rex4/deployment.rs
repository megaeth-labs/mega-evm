//! Tests for Rex4 system contract deployment wiring.

use alloy_evm::{block::BlockExecutor, Database, Evm, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Address, Bytes, B256};
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, EmptyExternalEnv, MegaBlockExecutionCtx,
    MegaBlockExecutor, MegaEvm, MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId,
    ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, ACCESS_CONTROL_CODE_HASH, LIMIT_CONTROL_ADDRESS,
    LIMIT_CONTROL_CODE, LIMIT_CONTROL_CODE_HASH,
};
use revm::{
    context::BlockEnv,
    database::State,
    inspector::NoOpInspector,
    primitives::{KECCAK_EMPTY, U256},
};

type TestState<'db> = State<&'db mut MemoryDatabase>;
type TestEvm<'a, 'db> = MegaEvm<&'a mut TestState<'db>, NoOpInspector, EmptyExternalEnv>;
type TestExecutor<'a, 'db> =
    MegaBlockExecutor<MegaHardforkConfig, TestEvm<'a, 'db>, OpAlloyReceiptBuilder>;

fn rex4_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated()
}

fn rex3_chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated().without(MegaHardfork::Rex4)
}

fn make_block_env(timestamp: u64) -> BlockEnv {
    BlockEnv {
        number: U256::from(1_u64),
        timestamp: U256::from(timestamp),
        gas_limit: 30_000_000,
        ..Default::default()
    }
}

fn with_executor<R>(
    spec: MegaSpecId,
    hardforks: MegaHardforkConfig,
    f: impl for<'a, 'db> FnOnce(&mut TestExecutor<'a, 'db>) -> R,
) -> R {
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();

    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    let evm_env = EvmEnv::new(cfg_env, make_block_env(0));

    let evm_factory = MegaEvmFactory::new();
    let evm = evm_factory.create_evm(&mut state, evm_env);
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO),
        Default::default(),
        BlockLimits::no_limits(),
    );
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, hardforks, receipt_builder);

    f(&mut executor)
}

fn assert_contract_deployed<DB: Database>(
    db: &mut State<DB>,
    address: Address,
    expected_code_hash: B256,
    expected_code: Bytes,
    name: &str,
) {
    let cache_acc = db.load_cache_account(address).expect("should load cache account");
    let acc_info = cache_acc.account_info().expect("system contract account should exist");

    assert_eq!(
        acc_info.code_hash, expected_code_hash,
        "{name} code hash should match expected bytecode"
    );
    assert!(acc_info.code.is_some(), "{name} code should be set");
    let deployed_code = acc_info.code.as_ref().expect("code should be present");
    assert_eq!(
        deployed_code.original_bytes(),
        expected_code,
        "{name} bytecode should match the embedded system contract code"
    );
}

fn assert_contract_not_deployed<DB: Database>(db: &mut State<DB>, address: Address, name: &str) {
    let cache_acc = db.load_cache_account(address).expect("should load cache account");
    let code_hash = cache_acc.account_info().map_or(KECCAK_EMPTY, |info| info.code_hash);
    assert_eq!(code_hash, KECCAK_EMPTY, "{name} should not have deployed code for this spec");
}

#[test]
fn test_rex4_system_contracts_deployed_on_activation() {
    with_executor(MegaSpecId::REX4, rex4_chain_spec(), |executor| {
        executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

        let db_ref = executor.evm_mut().db_mut();
        assert_contract_deployed(
            db_ref,
            ACCESS_CONTROL_ADDRESS,
            ACCESS_CONTROL_CODE_HASH,
            ACCESS_CONTROL_CODE,
            "MegaAccessControl",
        );
        assert_contract_deployed(
            db_ref,
            LIMIT_CONTROL_ADDRESS,
            LIMIT_CONTROL_CODE_HASH,
            LIMIT_CONTROL_CODE,
            "MegaLimitControl",
        );
    });
}

#[test]
fn test_rex4_system_contract_deployment_is_idempotent() {
    with_executor(MegaSpecId::REX4, rex4_chain_spec(), |executor| {
        executor.apply_pre_execution_changes().expect("first pre-execution changes should succeed");

        {
            let db_ref = executor.evm_mut().db_mut();
            assert_contract_deployed(
                db_ref,
                ACCESS_CONTROL_ADDRESS,
                ACCESS_CONTROL_CODE_HASH,
                ACCESS_CONTROL_CODE,
                "MegaAccessControl",
            );
            assert_contract_deployed(
                db_ref,
                LIMIT_CONTROL_ADDRESS,
                LIMIT_CONTROL_CODE_HASH,
                LIMIT_CONTROL_CODE,
                "MegaLimitControl",
            );
        }

        executor
            .apply_pre_execution_changes()
            .expect("second pre-execution changes should also succeed");

        let db_ref = executor.evm_mut().db_mut();
        assert_contract_deployed(
            db_ref,
            ACCESS_CONTROL_ADDRESS,
            ACCESS_CONTROL_CODE_HASH,
            ACCESS_CONTROL_CODE,
            "MegaAccessControl after second apply",
        );
        assert_contract_deployed(
            db_ref,
            LIMIT_CONTROL_ADDRESS,
            LIMIT_CONTROL_CODE_HASH,
            LIMIT_CONTROL_CODE,
            "MegaLimitControl after second apply",
        );
    });
}

#[test]
fn test_rex3_boundary_does_not_deploy_rex4_system_contracts() {
    with_executor(MegaSpecId::REX3, rex3_chain_spec(), |executor| {
        executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

        let db_ref = executor.evm_mut().db_mut();
        assert_contract_not_deployed(db_ref, ACCESS_CONTROL_ADDRESS, "MegaAccessControl");
        assert_contract_not_deployed(db_ref, LIMIT_CONTROL_ADDRESS, "MegaLimitControl");
    });
}
