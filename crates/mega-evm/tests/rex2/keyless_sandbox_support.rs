//! Shared fixtures for keyless-deploy sandbox observer and inspector tests.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ResultAndState,
    sandbox::SandboxObserver,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, IKeylessDeploy, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
    MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::{BALANCE, CALL, MSTORE, POP, PUSH0, RETURN, SELFDESTRUCT, STATICCALL},
    context::TxEnv,
    handler::EvmTr,
    inspector::NoOpInspector,
};

pub(crate) const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");
pub(crate) const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;
pub(crate) const LARGE_SIGNER_BALANCE: u128 = 1_000_000_000_000_000_000_000;
pub(crate) const SIGNED_TX_GAS_LIMIT: u64 = 1_000_000;
pub(crate) const REVERTER: Address = address!("0000000000000000000000000000000000aaaaaa");
pub(crate) const IDENTITY_PRECOMPILE: Address =
    address!("0000000000000000000000000000000000000004");
pub(crate) const MERGE_FAIL_SENTINEL: Address =
    address!("0000000000000000000000000000000000bbbbbb");
pub(crate) const DEFAULT_OUTER_GAS_LIMIT: u64 = 1_000_000_000_000;

pub(crate) const SPECS: [MegaSpecId; 5] =
    [MegaSpecId::REX2, MegaSpecId::REX3, MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// Words of memory expanded by the constructor `MSTORE`.
///
/// Pre-REX5 sandboxes ignore the parent's compute limit and run at the spec
/// default of 200M (`rex::TX_COMPUTE_GAS_LIMIT`). An 8_000-byte code deposit
/// is only 1.6M compute, so the constructor has to land in the window
/// `(200M - 1.6M, 200M)` and then `RETURN` `8_000` bytes so the deposit charge
/// splits the CREATE. Memory-expansion gas is `3*words + words²/512`; `318_191`
/// words (~10.18 MiB) costs `198_699_714`, which sits in that window without a
/// multi-million-iteration loop.
pub(crate) const SPLIT_CREATE_MEM_WORDS: u64 = 318_191;
pub(crate) const SPLIT_CREATE_MEM_OFFSET: u32 = (SPLIT_CREATE_MEM_WORDS * 32 - 32) as u32;
pub(crate) const SPLIT_CREATE_CODE_LEN: u32 = 8_000;
pub(crate) const SPLIT_CREATE_SLOT: u64 = 2;
pub(crate) const SPLIT_CREATE_SLOT_VALUE: u64 = 0x2a;

pub(crate) fn success_constructor() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

pub(crate) fn revert_constructor() -> Bytes {
    Bytes::from_static(&hex!("60006000fd"))
}

pub(crate) fn empty_code_constructor() -> Bytes {
    BytecodeBuilder::default().append_many([PUSH0, PUSH0, RETURN]).build()
}

/// Init code that `SELFDESTRUCT`s the contract under construction with `address(0)` as the
/// beneficiary, so the sandbox emits a `selfdestruct` hook and deploys nothing.
pub(crate) fn selfdestructing_constructor() -> Bytes {
    BytecodeBuilder::default().append_many([PUSH0, PUSH0, SELFDESTRUCT]).build()
}

pub(crate) fn constructor_calls_reverter() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(REVERTER)
        .push_number(50_000_u32)
        .append(CALL)
        .append(POP)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

pub(crate) fn constructor_touches_sentinel() -> Bytes {
    BytecodeBuilder::default()
        .push_address(MERGE_FAIL_SENTINEL)
        .append(BALANCE)
        .append(POP)
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

pub(crate) fn split_create_initcode() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(SPLIT_CREATE_SLOT), U256::from(SPLIT_CREATE_SLOT_VALUE))
        .push_number(0_u8)
        .push_number(SPLIT_CREATE_MEM_OFFSET)
        .append(MSTORE)
        .push_number(SPLIT_CREATE_CODE_LEN)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Word the identity-precompile constructor sends and expects back.
pub(crate) const IDENTITY_INPUT: U256 = U256::from_limbs([0x11, 0, 0, 0]);
/// Word a short-circuiting hook answers the identity call with instead.
pub(crate) const IDENTITY_OVERRIDE: U256 = U256::from_limbs([0x42, 0, 0, 0]);

/// Constructor that STATICCALLs the identity precompile with [`IDENTITY_INPUT`] and stores
/// whatever comes back in slot 0, so a rewritten call outcome is visible in state.
pub(crate) fn constructor_calls_identity_and_stores_return() -> Bytes {
    BytecodeBuilder::default()
        .push_u256(IDENTITY_INPUT)
        .push_number(0_u8)
        .append(MSTORE)
        .push_number(32_u8)
        .push_number(0_u8)
        .push_number(32_u8)
        .push_number(0_u8)
        .push_address(IDENTITY_PRECOMPILE)
        .push_number(50_000_u32)
        .append(STATICCALL)
        .append(POP)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::MLOAD)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::SSTORE)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

pub(crate) fn constructor_calls_identity_precompile() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(IDENTITY_PRECOMPILE)
        .push_number(50_000_u32)
        .append(STATICCALL)
        .append(POP)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

pub(crate) fn create_pre_eip155_deploy_tx_with_value(
    init_code: Bytes,
    value: U256,
) -> (Bytes, Address) {
    create_pre_eip155_deploy_tx_with_value_and_gas_limit(init_code, value, SIGNED_TX_GAS_LIMIT)
}

/// Like [`create_pre_eip155_deploy_tx_with_value`] with an explicit inner gas limit, for
/// shapes that need the sandbox to run out of gas.
pub(crate) fn create_pre_eip155_deploy_tx_with_value_and_gas_limit(
    init_code: Bytes,
    value: U256,
    gas_limit: u64,
) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit,
        to: TxKind::Create,
        value,
        input: init_code,
        chain_id: None,
    };
    let r = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let s = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let sig = Signature::new(r, s, false);
    let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    let tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("should recover signer");
    (tx_bytes, signer)
}

pub(crate) fn create_pre_eip155_deploy_tx(init_code: Bytes) -> (Bytes, Address) {
    create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO)
}

pub(crate) fn keyless_deploy_call_tx(
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
) -> MegaTransaction {
    keyless_deploy_call_tx_with_outer_gas(
        keyless_deployment_tx,
        gas_limit_override,
        DEFAULT_OUTER_GAS_LIMIT,
    )
}

pub(crate) fn keyless_deploy_call_tx_with_outer_gas(
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
    outer_gas_limit: u64,
) -> MegaTransaction {
    keyless_deploy_call_tx_with_override_u256(
        keyless_deployment_tx,
        U256::from(gas_limit_override),
        outer_gas_limit,
    )
}

pub(crate) fn keyless_deploy_call_tx_with_override_u256(
    keyless_deployment_tx: Bytes,
    gas_limit_override: U256,
    outer_gas_limit: u64,
) -> MegaTransaction {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_deployment_tx,
        gasLimitOverride: gas_limit_override,
    }
    .abi_encode();
    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: outer_gas_limit,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

pub(crate) fn funded_db(signer: Address) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(signer, U256::from(LARGE_SIGNER_BALANCE));
    db
}

pub(crate) struct RunConfig<'a, O> {
    pub spec: MegaSpecId,
    pub db: &'a mut MemoryDatabase,
    pub tx_bytes: Bytes,
    pub gas_limit_override: u64,
    pub observer: Option<Rc<RefCell<O>>>,
    pub tx_limits: Option<EvmTxRuntimeLimits>,
    pub outer_gas_limit: u64,
}

pub(crate) fn run_keyless<O>(config: RunConfig<'_, O>) -> ResultAndState<MegaHaltReason>
where
    O: SandboxObserver<EmptyExternalEnv> + 'static,
{
    run_keyless_with_usage(config).0
}

pub(crate) fn run_keyless_with_usage<O>(
    config: RunConfig<'_, O>,
) -> (ResultAndState<MegaHaltReason>, LimitUsage)
where
    O: SandboxObserver<EmptyExternalEnv> + 'static,
{
    let mut context = MegaContext::new(config.db, config.spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    if let Some(limits) = config.tx_limits {
        context = context.with_tx_runtime_limits(limits);
    }
    if let Some(observer) = config.observer {
        context.set_keyless_sandbox_observer(observer);
    }
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx_with_outer_gas(
        config.tx_bytes,
        config.gas_limit_override,
        config.outer_gas_limit,
    );
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact");
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result, usage)
}

pub(crate) fn assert_result_and_state_eq(
    with_hook: &ResultAndState<MegaHaltReason>,
    without_hook: &ResultAndState<MegaHaltReason>,
    case: &str,
) {
    assert_eq!(with_hook.result, without_hook.result, "{case}: execution result must match");
    assert_eq!(
        with_hook.result.gas_used(),
        without_hook.result.gas_used(),
        "{case}: gas_used must match"
    );
    assert_eq!(
        with_hook.state.len(),
        without_hook.state.len(),
        "{case}: state account count must match"
    );
    for (addr, account) in &with_hook.state {
        let other = without_hook
            .state
            .get(addr)
            .unwrap_or_else(|| panic!("{case}: missing account {addr:?} in no-hook state"));
        assert_eq!(account, other, "{case}: account {addr:?}");
    }
}

pub(crate) fn assert_usage_eq(got: LimitUsage, expected: LimitUsage, case: &str) {
    assert_eq!(got.data_size, expected.data_size, "{case}: data_size");
    assert_eq!(got.kv_updates, expected.kv_updates, "{case}: kv_updates");
    assert_eq!(got.compute_gas, expected.compute_gas, "{case}: compute_gas");
    assert_eq!(got.state_growth, expected.state_growth, "{case}: state_growth");
}

pub(crate) fn crowded_parent_env() -> TestExternalEnvs {
    TestExternalEnvs::new()
        .with_default_bucket_capacity((MIN_BUCKET_SIZE as u64) * 8)
        .with_oracle_storage(U256::ZERO, U256::from(0x42))
}

pub(crate) fn run_keyless_with_parent_env<O>(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    env: TestExternalEnvs,
    observer: Option<Rc<RefCell<O>>>,
) -> ResultAndState<MegaHaltReason>
where
    O: SandboxObserver<TestExternalEnvs> + SandboxObserver<EmptyExternalEnv> + 'static,
{
    run_keyless_with_parent_env_usage(spec, db, tx_bytes, env, observer).0
}

pub(crate) fn run_keyless_with_parent_env_usage<O>(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    env: TestExternalEnvs,
    observer: Option<Rc<RefCell<O>>>,
) -> (ResultAndState<MegaHaltReason>, LimitUsage)
where
    O: SandboxObserver<TestExternalEnvs> + SandboxObserver<EmptyExternalEnv> + 'static,
{
    let mut context = MegaContext::new(db, spec).with_external_envs(env.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    if let Some(observer) = observer {
        context.set_keyless_sandbox_observer(observer);
    }
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact");
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result, usage)
}

pub(crate) fn parent_compute_gas_used(spec: MegaSpecId, signer: Address, tx_bytes: Bytes) -> u64 {
    let mut usage_db = funded_db(signer);
    let mut context = MegaContext::new(&mut usage_db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let used = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    used
}
