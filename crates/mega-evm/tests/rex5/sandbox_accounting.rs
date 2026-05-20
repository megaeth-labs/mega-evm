//! Tests for sandbox resource usage propagation.
//!
//! REX5 propagates resource usage (compute gas, data size, KV updates, state growth)
//! from sandbox execution (e.g., keyless deploy) back to the parent transaction.
//! Pre-REX5 specs do not propagate sandbox resource usage.

use std::vec::Vec;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    constants,
    revm::context::result::{ExecutionResult, ResultAndState},
    sandbox::{calculate_keyless_deploy_address, decode_error_result, KeylessDeployError},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, IOracle, LimitKind, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, TestExternalEnvs, VolatileDataAccess,
    ACCOUNT_INFO_WRITE_SIZE, AUTHORIZATION_SIZE, BASE_TX_SIZE, KEYLESS_DEPLOY_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{transaction::AuthorizationTr, Transaction, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
};

// ============================================================================
// TEST CONSTANTS
// ============================================================================

/// Standard test caller address.
const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");

/// Large gas limit override for tests (10 billion gas).
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

/// Signer balance large enough that tests never hit `InsufficientBalance` on their own.
const LARGE_SIGNER_BALANCE: u128 = 1_000_000_000_000_000_000_000;

/// Outer transaction's gas limit for tests; matches `keyless_deploy_call_tx`.
const OUTER_TX_GAS_LIMIT: u64 = 1_000_000_000_000;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Builds a minimal constructor that SSTOREs a single slot and returns 1 byte of runtime
/// code. The exact stored value is arbitrary — callers pass any non-zero byte just to
/// give the constructor observable compute / state-growth work.
fn minimal_1byte_constructor(storage_value: u8) -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(storage_value))
        .push_number(1_u8)
        .push_number(0_u8) // CODECOPY offset
        .push_number(0_u8) // memory dest
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Builds a fresh `MemoryDatabase` with `signer` funded to `LARGE_SIGNER_BALANCE`.
fn funded_signer_db(signer: Address) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(signer, U256::from(LARGE_SIGNER_BALANCE));
    db
}

/// Runs a baseline Rex5 keyless deploy to measure the resource usage a successful
/// deployment consumes. Returns `(tx_bytes, signer, deploy_address, baseline_usage)`,
/// asserting the baseline succeeds. Tests use this to derive tight cap / overflow values
/// without hardcoding constants.
fn rex5_baseline(init_code: Bytes) -> (Bytes, Address, Address, LimitUsage) {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);
    let mut db = funded_signer_db(signer);
    let (result, usage) = execute_keyless_deploy(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes.clone(),
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(result.is_success(), "baseline keyless deploy should succeed: {result:?}");
    (tx_bytes, signer, deploy_address, usage)
}

/// Creates a pre-EIP-155 keyless deploy transaction with deterministic signature.
fn create_pre_eip155_deploy_tx(init_code: Bytes) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Create,
        value: U256::ZERO,
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

/// Builds the outer `KeylessDeploy` transaction used by the test helpers.
fn keyless_deploy_call_tx(
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
) -> MegaTransaction {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_deployment_tx,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Builds the sandbox create transaction shape used by `KeylessDeploy`.
fn sandbox_create_tx_for_intrinsic(init_code: Bytes, signer: Address) -> MegaTransaction {
    let tx = TxEnv {
        caller: signer,
        kind: TxKind::Create,
        data: init_code,
        value: U256::ZERO,
        gas_limit: LARGE_GAS_LIMIT_OVERRIDE,
        gas_price: 100_000_000_000,
        nonce: 0,
        ..Default::default()
    };
    MegaTransaction::new(tx)
}

/// Computes the intrinsic compute gas that Rex5 records during transaction validation.
///
/// Must match the value `MegaHandler::validate` feeds into `record_compute_gas`, which is
/// the raw revm `initial_gas`. This helper calls the same revm routine, so it stays in
/// lock-step with production without replicating logic.
fn intrinsic_compute_gas(tx: &MegaTransaction) -> u64 {
    mega_evm::revm::interpreter::gas::calculate_initial_tx_gas_for_tx(
        tx,
        MegaSpecId::REX5.into_eth_spec(),
    )
    .initial_gas
}

/// Computes the intrinsic data-size usage recorded before the first frame.
///
/// Mirrors `DataSizeTracker::before_tx_start` in `src/limit/data_size.rs`. If that
/// function grows a new contribution, update this helper in sync — otherwise this test
/// will understate the sandbox intrinsic and the "preflight rejects intrinsic overflow"
/// cases can silently fall through to the `merge_and_reject_if_overflow` safety net path
/// instead of exercising the preflight fast-path they are meant to cover.
fn intrinsic_data_size(tx: &MegaTransaction) -> u64 {
    let access_list_size = tx
        .access_list()
        .map(|item| item.map(|access| access.size() as u64).sum::<u64>())
        .unwrap_or_default();
    let authority_updates =
        tx.authorization_list().filter(|authorization| authorization.authority().is_some()).count()
            as u64 *
            ACCOUNT_INFO_WRITE_SIZE;

    BASE_TX_SIZE +
        tx.input().len() as u64 +
        access_list_size +
        tx.authorization_list_len() as u64 * AUTHORIZATION_SIZE +
        authority_updates +
        ACCOUNT_INFO_WRITE_SIZE
}

/// Computes the intrinsic KV updates recorded before the first frame.
///
/// Mirrors `KVUpdateTracker::before_tx_start`: the transaction caller update plus one
/// account update per valid EIP-7702 authority.
fn intrinsic_kv_updates(tx: &MegaTransaction) -> u64 {
    tx.authorization_list().filter(|authorization| authorization.authority().is_some()).count()
        as u64 +
        1
}

/// Executes a keyless deploy and returns the execution result and resource usage.
fn execute_keyless_deploy(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
) -> (ExecutionResult<MegaHaltReason>, LimitUsage) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(keyless_deployment_tx, gas_limit_override);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result_envelope.result, usage)
}

/// Variant of [`execute_keyless_deploy`] that also returns the parent's
/// `VolatileDataAccess` bitmap after the call returns.
fn execute_keyless_deploy_with_volatile(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
) -> (ExecutionResult<MegaHaltReason>, LimitUsage, VolatileDataAccess) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(keyless_deployment_tx, gas_limit_override);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    let volatile = evm.ctx_ref().volatile_data_tracker.borrow().get_volatile_data_accessed();
    (result_envelope.result, usage, volatile)
}

/// Constructor that reads `block.timestamp` (TIMESTAMP opcode), then deploys a
/// 1-byte runtime contract. Sets `VolatileDataAccess::TIMESTAMP` in the
/// sandbox tracker.
fn constructor_reads_timestamp() -> Bytes {
    BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Constructor that reads `block.coinbase` (COINBASE opcode), then deploys.
/// Sets `VolatileDataAccess::COINBASE` only (NOT `BENEFICIARY_BALANCE`; that
/// bit requires `BALANCE` against the coinbase address).
fn constructor_reads_coinbase() -> Bytes {
    BytecodeBuilder::default()
        .append(COINBASE)
        .append(POP)
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Constructor: `COINBASE; BALANCE; POP; ...deploy...`. Reads the beneficiary
/// balance via the BALANCE opcode, which sets
/// `VolatileDataAccess::BENEFICIARY_BALANCE` via `mark_beneficiary_balance_accessed`.
fn constructor_reads_coinbase_balance() -> Bytes {
    BytecodeBuilder::default()
        .append(COINBASE)
        .append(BALANCE)
        .append(POP)
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Constructor that STATICCALLs `IOracle::getSlot(0)` on the oracle contract.
/// The oracle's bytecode executes SLOAD on its own storage to fetch the slot,
/// which goes through the host `sload` wrapper and sets
/// `VolatileDataAccess::ORACLE` on the sandbox tracker (Rex3+ marks oracle
/// access on SLOAD inside the oracle frame, not on the outer CALL).
fn constructor_calls_oracle() -> Bytes {
    // Build the calldata for `getSlot(0)` as a single 32-byte word: the
    // 4-byte selector in the high bytes, followed by 28 zero bytes which
    // overlap with the high 28 bytes of the (zero) key word. After MSTORE,
    // memory[0..4] = selector, memory[4..32] = 0, and the auto-zero memory
    // extension at offset 32..36 supplies the remaining 4 bytes of the
    // key argument.
    use alloy_sol_types::SolCall;
    let mut selector_word_bytes = [0u8; 32];
    selector_word_bytes[0..4].copy_from_slice(&IOracle::getSlotCall::SELECTOR);
    let selector_word = U256::from_be_bytes(selector_word_bytes);

    BytecodeBuilder::default()
        .push_u256(selector_word)
        .push_number(0_u8) // memory dest offset
        .append(MSTORE)
        // STATICCALL stack (top is last push): gas, address, argsOff, argsSize, retOff, retSize
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(36_u8) // argsSize (4 selector + 32 key)
        .push_number(0_u8) // argsOffset
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(100_000_u128) // forwarded gas
        .append(STATICCALL)
        .append(POP)
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Decodes the ABI-encoded `KeylessDeploy` return payload.
fn decode_keyless_deploy_return(
    result: &ExecutionResult<MegaHaltReason>,
) -> IKeylessDeploy::keylessDeployReturn {
    let output = result.output().expect("keyless deploy should return ABI-encoded output");
    IKeylessDeploy::keylessDeployCall::abi_decode_returns(output)
        .expect("keyless deploy return should decode")
}

// ============================================================================
// TESTS
// ============================================================================

/// REX5 sandbox success propagates resource usage to the parent transaction.
///
/// A keyless deploy constructor that writes storage slots should have its resource usage
/// (compute gas, data size, KV updates, state growth) reflected in the parent transaction.
#[test]
fn test_rex5_sandbox_success_propagates_resource_usage() {
    // Constructor that writes two storage slots, then returns 1 byte of runtime code.
    // Two-slot variant (vs the shared `minimal_1byte_constructor`) exercises multiple KV
    // updates so the kv_updates assertion below is non-trivial.
    let init_code = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(42))
        .sstore(U256::from(1), U256::from(43))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build();
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let mut db = funded_signer_db(signer);

    let (result, usage) =
        execute_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    assert!(result.is_success(), "keyless deploy should succeed: {result:?}");

    // REX5 should propagate sandbox resource usage to parent.
    assert!(usage.compute_gas > 0, "compute gas should include sandbox execution");
    assert!(usage.data_size > 0, "data size should include sandbox storage writes");
    assert!(usage.kv_updates > 0, "KV updates should include sandbox storage writes");
    assert!(usage.state_growth > 0, "state growth should include new account from sandbox");
}

/// REX4 sandbox does NOT propagate resource usage to the parent transaction.
///
/// The same constructor that works in REX5 should have zero state growth in REX4
/// because sandbox usage is not propagated.
#[test]
fn test_rex4_sandbox_does_not_propagate_resource_usage() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(minimal_1byte_constructor(42));
    let mut db = funded_signer_db(signer);

    let (result, usage) =
        execute_keyless_deploy(MegaSpecId::REX4, &mut db, tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    assert!(result.is_success(), "keyless deploy should succeed: {result:?}");

    // The sandbox's storage writes should NOT appear in parent usage in REX4.
    assert_eq!(
        usage.state_growth, 0,
        "REX4 should not propagate sandbox state growth, got: {}",
        usage.state_growth
    );
}

/// Executes a keyless deploy with custom runtime limits, returning the full `ResultAndState`.
fn execute_keyless_deploy_with_limits(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
    tx_limits: EvmTxRuntimeLimits,
) -> ResultAndState<MegaHaltReason> {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec)
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(tx_limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(keyless_deployment_tx, gas_limit_override);
    alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap()
}

fn assert_preflight_failure_without_sandbox_state(
    result_and_state: &ResultAndState<MegaHaltReason>,
    deploy_address: Address,
    signer: Address,
) -> KeylessDeployError {
    // Preflight failures revert (like other validation errors) since no sandbox ran.
    let revert_output = match &result_and_state.result {
        ExecutionResult::Revert { output, .. } => output,
        other => panic!("preflight failure should revert, got: {other:?}"),
    };
    let error = decode_error_result(revert_output)
        .expect("preflight revert should contain encoded ParentBudgetExceeded");
    assert!(
        matches!(error, KeylessDeployError::ParentBudgetExceeded { .. }),
        "preflight should revert with ParentBudgetExceeded, got: {error:?}"
    );

    let deployed_account = result_and_state.state.get(&deploy_address);
    let has_code = deployed_account
        .map(|acc| acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false))
        .unwrap_or(false);
    assert!(!has_code, "deployed contract must not survive preflight failure");

    let signer_nonce = result_and_state.state.get(&signer).map(|acc| acc.info.nonce).unwrap_or(0);
    assert_eq!(
        signer_nonce, 0,
        "signer nonce must not be bumped when sandbox is not started, got: {signer_nonce}"
    );

    error
}

/// REX5 must fail the sandbox internally when the parent's remaining state-growth budget is
/// smaller than the sandbox's required state growth.
///
/// Regression test for the upfront-cap design: tightening the parent state-growth limit
/// should now make the sandbox fail internally, returning success-style `errorData`
/// instead of letting the sandbox succeed and halting the outer transaction afterwards.
#[test]
fn test_rex5_sandbox_tight_state_growth_budget_fails_inside_sandbox() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(42));
    assert!(baseline_usage.state_growth > 0, "baseline deploy should consume state growth");

    // Give the parent slightly less state growth than the successful sandbox needs.
    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_state_growth_limit(baseline_usage.state_growth.saturating_sub(1));

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    // The outer transaction should still return successfully with encoded execution failure.
    assert!(
        result_and_state.result.is_success(),
        "outer transaction should return success-style failure, got: {:?}",
        result_and_state.result,
    );
    let decoded = decode_keyless_deploy_return(&result_and_state.result);
    assert_eq!(
        decoded.deployedAddress,
        Address::ZERO,
        "internal sandbox failure should not deploy"
    );
    assert!(!decoded.errorData.is_empty(), "failure path should return encoded error data");

    // The deployed contract must NOT exist in the finalized state.
    let deployed_account = result_and_state.state.get(&deploy_address);
    let has_code = deployed_account
        .map(|acc| acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false))
        .unwrap_or(false);
    assert!(!has_code, "deployed contract code must not survive sandbox-internal limit failure");
}

/// REX5: when `record_deposit_caller_creation` (inside
/// `charge_caller_materialization_pre_sandbox`) latches a non-frame-local
/// `StateGrowthLimitExceeded`, the keyless-deploy interceptor MUST emit the canonical
/// exceeding-limit OOG halt — with parent gas rescued — at the latch point.
///
/// If the conversion is deferred, `before_frame_return_result::<true>` still rewrites the
/// final Revert into a Halt at tx end, but its docstring's "gas should already have been
/// rescued at the point where the limit was exceeded" precondition is violated:
/// `after_frame_run`'s `try_rescue_gas` does not run on the interceptor short-circuit
/// path, so the parent loses its entire outer gas budget.
///
/// The deploy address is pre-occupied with code so a deferred halt is observable: the
/// flow would reach the `ContractAlreadyExists` check and surface
/// `Revert(ContractAlreadyExists())` instead of the canonical state-growth Halt — which
/// the halt-vs-Revert assertion below distinguishes.
#[test]
fn test_rex5_pre_sandbox_materialization_state_growth_overflow_halts_immediately() {
    let init_code = minimal_1byte_constructor(42);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);
    // Signer must be empty (no balance, no nonce, no code) to enter the materialization
    // branch — `AccountInfo::is_empty()` is the gate. Funding it would short-circuit the
    // branch before the state-growth latch could fire.
    let mut db = MemoryDatabase::default();
    // Pre-occupy the deploy address. The state-growth halt MUST fire before the
    // `ContractAlreadyExists` check; if it ever fires later, the check surfaces
    // `Revert(ContractAlreadyExists())` and the assertions below catch the regression.
    let preexisting_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
    db.set_account_code(deploy_address, preexisting_code.clone());

    // `tx_state_growth_limit = 0` so the materialization-side `+1` immediately overflows
    // the TX-level state-growth budget on `record_deposit_caller_creation`.
    let tx_limits = EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(0);

    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    // Must be a canonical exceeding-limit Halt, NOT a business-error Revert.
    let halt_reason = match &result_and_state.result {
        ExecutionResult::Halt { reason, .. } => reason.clone(),
        ExecutionResult::Revert { output, .. } => {
            let err = decode_error_result(output);
            panic!(
                "state-growth overflow must produce a canonical Halt, not a business \
                 Revert; got Revert with selector {err:?}",
            );
        }
        other => panic!("expected Halt, got {other:?}"),
    };
    assert!(
        matches!(halt_reason, MegaHaltReason::StateGrowthLimitExceeded { limit: 0, actual: 1 }),
        "halt reason must be the canonical StateGrowthLimitExceeded {{ limit: 0, actual: 1 }}, \
         got {halt_reason:?}",
    );

    // Signer never bumped — the sandbox was never constructed.
    let signer_nonce = result_and_state.state.get(&signer).map(|acc| acc.info.nonce).unwrap_or(0);
    assert_eq!(signer_nonce, 0, "signer nonce must remain 0 when halt fires pre-sandbox");

    // Deploy address's pre-existing code is untouched.
    if let Some(acc) = result_and_state.state.get(&deploy_address) {
        if let Some(code) = &acc.info.code {
            assert_eq!(
                code.bytes_slice(),
                preexisting_code.as_ref(),
                "pre-existing deploy-address code must not be overwritten",
            );
        }
    }

    // Gas was rescued — the unspent outer budget is returned to the sender; only the
    // dispatch overhead, materialization charge, and tx intrinsic are reported as used.
    assert!(
        result_and_state.result.gas_used() < OUTER_TX_GAS_LIMIT / 2,
        "outer caller must be rescued; gas_used={} should be well below tx limit {}",
        result_and_state.result.gas_used(),
        OUTER_TX_GAS_LIMIT,
    );
}

/// REX5 preflights sandbox intrinsic compute gas before starting sandbox execution.
///
/// If the parent's remaining compute budget cannot fit the sandbox transaction's intrinsic
/// compute gas, the sandbox must not start and signer state must not be merged.
#[test]
fn test_rex5_sandbox_intrinsic_compute_preflight_fails_without_sandbox() {
    let init_code = minimal_1byte_constructor(42);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code.clone());
    let deploy_address = calculate_keyless_deploy_address(signer);

    let outer_tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
    let sandbox_tx = sandbox_create_tx_for_intrinsic(init_code, signer);
    let outer_intrinsic_compute = intrinsic_compute_gas(&outer_tx);
    let sandbox_intrinsic_compute = intrinsic_compute_gas(&sandbox_tx);
    assert!(sandbox_intrinsic_compute > 0, "sandbox tx should have intrinsic compute gas");

    // Parent limit = outer_intrinsic + overhead + sandbox_intrinsic - 1. Just below what
    // the sandbox's own intrinsic recording would consume; preflight must reject.
    let tx_limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(
        outer_intrinsic_compute +
            constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS +
            sandbox_intrinsic_compute -
            1,
    );

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert_preflight_failure_without_sandbox_state(&result_and_state, deploy_address, signer);
}

/// REX5 preflights sandbox intrinsic data size before starting sandbox execution.
///
/// This covers the `before_tx_start()` path where the sandbox tracker records base tx size,
/// input bytes, and caller account update as tx-level persistent usage.
#[test]
fn test_rex5_sandbox_intrinsic_data_size_preflight_fails_without_sandbox() {
    let init_code = minimal_1byte_constructor(7);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code.clone());
    let deploy_address = calculate_keyless_deploy_address(signer);

    let outer_tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
    let sandbox_tx = sandbox_create_tx_for_intrinsic(init_code, signer);
    let outer_intrinsic_data_size = intrinsic_data_size(&outer_tx);
    let sandbox_intrinsic_data_size = intrinsic_data_size(&sandbox_tx);
    assert!(sandbox_intrinsic_data_size > 0, "sandbox tx should have intrinsic data size");

    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(outer_intrinsic_data_size + sandbox_intrinsic_data_size - 1);

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert_preflight_failure_without_sandbox_state(&result_and_state, deploy_address, signer);
}

/// REX5 preflights sandbox intrinsic KV updates before starting sandbox execution.
///
/// This covers the `before_tx_start()` path where the sandbox tracker records the
/// sandbox caller account update as TX-level persistent usage.
#[test]
fn test_rex5_sandbox_intrinsic_kv_update_preflight_fails_without_sandbox() {
    let init_code = minimal_1byte_constructor(7);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code.clone());
    let deploy_address = calculate_keyless_deploy_address(signer);

    let outer_tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
    let sandbox_tx = sandbox_create_tx_for_intrinsic(init_code, signer);
    let outer_intrinsic_kv_updates = intrinsic_kv_updates(&outer_tx);
    let sandbox_intrinsic_kv_updates = intrinsic_kv_updates(&sandbox_tx);
    assert!(sandbox_intrinsic_kv_updates > 0, "sandbox tx should have intrinsic KV updates");

    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_kv_updates_limit(outer_intrinsic_kv_updates + sandbox_intrinsic_kv_updates - 1);

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    let error =
        assert_preflight_failure_without_sandbox_state(&result_and_state, deploy_address, signer);
    assert!(
        matches!(error, KeylessDeployError::ParentBudgetExceeded { kind: LimitKind::KVUpdate, .. }),
        "KV preflight should reject with ParentBudgetExceeded(KVUpdate), got: {error:?}"
    );
}

/// REX5 must fail the sandbox internally when the parent's remaining KV-update budget is
/// smaller than the sandbox's required KV updates.
///
/// This covers execution-time KV updates after intrinsic preflight has passed, so the
/// sandbox should return a success-style failure and must not deploy the contract.
#[test]
fn test_rex5_sandbox_tight_kv_update_budget_fails_inside_sandbox() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(7));
    assert!(baseline_usage.kv_updates > 0, "baseline deploy should consume KV updates");

    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_kv_updates_limit(baseline_usage.kv_updates.saturating_sub(1));

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert!(
        result_and_state.result.is_success(),
        "outer transaction should return success-style failure, got: {:?}",
        result_and_state.result,
    );
    let decoded = decode_keyless_deploy_return(&result_and_state.result);
    assert_eq!(
        decoded.deployedAddress,
        Address::ZERO,
        "internal sandbox failure should not deploy"
    );
    assert!(!decoded.errorData.is_empty(), "failure path should return encoded error data");

    let deployed_account = result_and_state.state.get(&deploy_address);
    let has_code = deployed_account
        .map(|acc| acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false))
        .unwrap_or(false);
    assert!(!has_code, "deployed contract code must not survive sandbox KV limit failure");
}

/// REX5 must reject the outer transaction cleanly when a sandbox TX-level overflow on the
/// compute-gas dimension pushes the parent over its limit.
///
/// The safety net in `merge_and_reject_if_overflow` detects post-merge overflow, refuses to
/// merge sandbox state (preventing the deployed contract from surviving via the unchecked
/// direct journal write), absorbs the sandbox's EVM gas via `record_cost` on the outer
/// caller, and rescues the remaining gas. Without this safety net the outer caller would
/// be seized the full parent gas AND the deployed contract would persist on a "halt" tx.
#[test]
fn test_rex5_sandbox_compute_gas_overflow_rejects_cleanly() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(42));
    assert!(baseline_usage.compute_gas > 0, "baseline deploy should consume compute gas");

    // Tighten parent's compute gas limit just below what the full deploy needs so the
    // sandbox's merged usage overshoots parent's remaining envelope by a single opcode.
    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_compute_gas_limit(baseline_usage.compute_gas.saturating_sub(1));

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    // Outer tx must halt — the parent's compute budget does not fit the sandbox.
    assert!(
        result_and_state.result.is_halt(),
        "outer transaction should halt on sandbox compute gas overflow, got: {:?}",
        result_and_state.result,
    );

    // The halt must NOT consume the full parent tx gas: the safety net rescues remaining
    // gas and only charges the caller for the KeylessDeploy overhead plus sandbox gas.
    assert!(
        result_and_state.result.gas_used() < OUTER_TX_GAS_LIMIT / 2,
        "outer caller must be rescued; gas_used={} should be well below tx limit {}",
        result_and_state.result.gas_used(),
        OUTER_TX_GAS_LIMIT,
    );

    // Deployed contract must NOT survive — the safety net skipped `apply_sandbox_state`.
    let deployed_account = result_and_state.state.get(&deploy_address);
    let has_code = deployed_account
        .map(|acc| acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false))
        .unwrap_or(false);
    assert!(!has_code, "deployed contract must not survive outer reject");

    // Signer state must NOT be modified — sandbox state was never merged.
    let signer_nonce = result_and_state.state.get(&signer).map(|acc| acc.info.nonce).unwrap_or(0);
    assert_eq!(
        signer_nonce, 0,
        "signer nonce must not be bumped on sandbox overflow reject, got: {signer_nonce}"
    );
}

/// REX5 must reject cleanly when a sandbox TX-level data-size overflow — either intrinsic
/// or opcode-level — pushes the parent over its limit.
///
/// The sandbox's `DataSizeTracker::before_tx_start` records the base transaction size,
/// the input length, and the caller account update as non-discardable TX-level usage.
/// Even if the frame-local check would normally revert a sandbox frame on data-size
/// exceed, TX-level overflow (when persistent usage alone is above cap) takes the
/// non-frame-local path and must be rejected by the safety net.
#[test]
fn test_rex5_sandbox_data_size_overflow_rejects_cleanly() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(7));
    assert!(baseline_usage.data_size > 0, "baseline deploy should consume data size");

    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(baseline_usage.data_size.saturating_sub(1));

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    // Either outer halts (TX-level overflow path) or outer returns a success-style failure
    // (frame-local revert path inside sandbox). Both are acceptable, but in the halt case
    // the safety net must have rescued gas and prevented state survival.
    if result_and_state.result.is_halt() {
        assert!(
            result_and_state.result.gas_used() < OUTER_TX_GAS_LIMIT / 2,
            "outer caller must be rescued; gas_used={} should be well below tx limit {}",
            result_and_state.result.gas_used(),
            OUTER_TX_GAS_LIMIT,
        );
        let deployed_account = result_and_state.state.get(&deploy_address);
        let has_code = deployed_account
            .map(|acc| acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false))
            .unwrap_or(false);
        assert!(!has_code, "deployed contract must not survive outer reject");
        let signer_nonce =
            result_and_state.state.get(&signer).map(|acc| acc.info.nonce).unwrap_or(0);
        assert_eq!(
            signer_nonce, 0,
            "signer nonce must not be bumped on sandbox overflow reject, got: {signer_nonce}"
        );
    } else {
        // Frame-local revert path: sandbox failed internally, returned errorData.
        assert!(
            result_and_state.result.is_success(),
            "outer transaction should be success-style failure if not halted, got: {:?}",
            result_and_state.result,
        );
        let decoded = decode_keyless_deploy_return(&result_and_state.result);
        assert_eq!(
            decoded.deployedAddress,
            Address::ZERO,
            "internal sandbox failure should not deploy"
        );
        assert!(!decoded.errorData.is_empty(), "failure path should return encoded error data");
    }
}

/// REX5 should succeed when the parent's remaining compute-gas budget exactly matches the
/// sandbox's baseline compute usage.
///
/// Companion to the state-growth exact-fit boundary test: covers compute gas, the other
/// dimension where overshoot from a single opcode would break the cap invariant.
#[test]
fn test_rex5_sandbox_exact_remaining_compute_gas_succeeds() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(7));
    assert!(baseline_usage.compute_gas > 0, "baseline deploy should consume compute gas");

    let tx_limits =
        EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(baseline_usage.compute_gas);

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert_exact_fit_success(&result_and_state, deploy_address);
}

/// REX5 should succeed when the parent's remaining data-size budget exactly matches the
/// sandbox's baseline data-size usage.
#[test]
fn test_rex5_sandbox_exact_remaining_data_size_succeeds() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(7));
    assert!(baseline_usage.data_size > 0, "baseline deploy should consume data size");

    let tx_limits =
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(baseline_usage.data_size);

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert_exact_fit_success(&result_and_state, deploy_address);
}

/// REX5 should succeed when the parent's remaining KV-update budget exactly matches the
/// sandbox's baseline KV usage.
#[test]
fn test_rex5_sandbox_exact_remaining_kv_updates_succeeds() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(7));
    assert!(baseline_usage.kv_updates > 0, "baseline deploy should consume KV updates");

    let tx_limits =
        EvmTxRuntimeLimits::no_limits().with_tx_kv_updates_limit(baseline_usage.kv_updates);

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert_exact_fit_success(&result_and_state, deploy_address);
}

/// REX5 should still succeed when the parent's remaining state-growth budget exactly matches
/// the sandbox's required state growth.
///
/// This boundary test guards the capped-limit math: the sandbox should be allowed to
/// consume the full remaining budget, and merging its usage back must not make the outer
/// transaction fail afterwards.
#[test]
fn test_rex5_sandbox_exact_remaining_budget_succeeds() {
    let (tx_bytes, signer, deploy_address, baseline_usage) =
        rex5_baseline(minimal_1byte_constructor(99));
    assert!(baseline_usage.state_growth > 0, "baseline deploy should consume state growth");

    let tx_limits =
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(baseline_usage.state_growth);

    let mut db = funded_signer_db(signer);
    let result_and_state = execute_keyless_deploy_with_limits(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        tx_limits,
    );

    assert_exact_fit_success(&result_and_state, deploy_address);
}

/// Shared assertion: the outer tx succeeds, returns the expected deploy address with
/// empty `errorData`, and the deployed bytecode survives in the finalized state.
/// Used by both exact-remaining-budget boundary tests.
fn assert_exact_fit_success(
    result_and_state: &ResultAndState<MegaHaltReason>,
    deploy_address: Address,
) {
    assert!(
        result_and_state.result.is_success(),
        "outer transaction should succeed when sandbox exactly fits the remaining budget: {:?}",
        result_and_state.result,
    );
    let decoded = decode_keyless_deploy_return(&result_and_state.result);
    assert_eq!(
        decoded.deployedAddress, deploy_address,
        "successful deploy should keep expected address"
    );
    assert!(decoded.errorData.is_empty(), "successful deploy should not return error data");

    let deployed_account = result_and_state
        .state
        .get(&deploy_address)
        .expect("deployed contract should exist in finalized state");
    assert!(
        deployed_account.info.code.as_ref().is_some_and(|code| !code.is_empty()),
        "deployed contract code should survive when the sandbox fits exactly in the remaining budget"
    );
}

// ============================================================================
// VOLATILE-ACCESS FOOTPRINT MERGE (REX5+)
// ============================================================================

/// REX5 sandbox: a constructor that reads TIMESTAMP propagates the bit into
/// the parent `volatile_data_tracker` via the footprint merge.
#[test]
fn test_rex5_sandbox_block_env_access_merged_into_parent_volatile_tracker() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_reads_timestamp());
    let mut db = funded_signer_db(signer);
    let (result, _usage, volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(result.is_success(), "sandbox should succeed: {result:?}");
    assert!(
        volatile.contains(VolatileDataAccess::TIMESTAMP),
        "parent tracker should include sandbox TIMESTAMP read; got {:?}",
        volatile,
    );
}

/// REX5 sandbox: COINBASE opcode marks the COINBASE block-env bit only. Pins
/// the separation between COINBASE (block-env bitfield) and `BENEFICIARY_BALANCE`
/// (set only when BALANCE is read against the coinbase address).
#[test]
fn test_rex5_sandbox_coinbase_access_merged_into_parent_volatile_tracker() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_reads_coinbase());
    let mut db = funded_signer_db(signer);
    let (result, _usage, volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(result.is_success(), "sandbox should succeed: {result:?}");
    assert!(
        volatile.contains(VolatileDataAccess::COINBASE),
        "parent tracker should include COINBASE bit; got {:?}",
        volatile,
    );
    assert!(
        !volatile.contains(VolatileDataAccess::BENEFICIARY_BALANCE),
        "COINBASE alone must NOT set BENEFICIARY_BALANCE; got {:?}",
        volatile,
    );
}

/// REX5 sandbox: BALANCE-on-coinbase reads the beneficiary balance via the
/// beneficiary-aware host wrapper, which calls `mark_beneficiary_balance_accessed`.
/// The `BENEFICIARY_BALANCE` bit must propagate to the parent — separate path from
/// the plain COINBASE block-env bit covered above.
#[test]
fn test_rex5_sandbox_beneficiary_balance_access_merged_into_parent_volatile_tracker() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_reads_coinbase_balance());
    let mut db = funded_signer_db(signer);
    let (result, _usage, volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(result.is_success(), "sandbox should succeed: {result:?}");
    assert!(
        volatile.contains(VolatileDataAccess::BENEFICIARY_BALANCE),
        "parent tracker should include BENEFICIARY_BALANCE; got {:?}",
        volatile,
    );
}

/// REX5 sandbox: STATICCALL `IOracle::getSlot(0)` runs the oracle bytecode
/// which SLOADs slot 0; the host's `sload` wrapper sets
/// `VolatileDataAccess::ORACLE` on the sandbox tracker. The bit must
/// propagate to the parent.
///
/// The oracle contract bytecode is predeployed in the test database because
/// `MegaContext::new` does not run the block-level system-contract
/// deployment that production does.
#[test]
fn test_rex5_sandbox_oracle_access_merged_into_parent_volatile_tracker() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_calls_oracle());
    let mut db = funded_signer_db(signer);
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, mega_evm::ORACLE_CONTRACT_CODE_REX5);
    let (result, _usage, volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(result.is_success(), "sandbox should succeed: {result:?}");
    assert!(
        volatile.contains(VolatileDataAccess::ORACLE),
        "parent tracker should include ORACLE access; got {:?}",
        volatile,
    );
}

/// REX5 sandbox: a constructor that returns empty code triggers the in-sandbox
/// `EmptyCodeDeployed` failure. The volatile bits set during constructor
/// execution must still propagate — the user code ran and accessed the data
/// regardless of the final outcome.
#[test]
fn test_rex5_sandbox_volatile_merge_runs_on_in_sandbox_failure_empty_code() {
    let init_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).append(STOP).build();
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let mut db = funded_signer_db(signer);
    let (result, _usage, volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(
        result.is_success(),
        "outer call returns Success with errorData for in-sandbox failure: {result:?}"
    );
    let decoded = decode_keyless_deploy_return(&result);
    assert!(
        !decoded.errorData.is_empty(),
        "errorData should encode the in-sandbox EmptyCodeDeployed failure"
    );
    assert!(
        volatile.contains(VolatileDataAccess::TIMESTAMP),
        "parent tracker should include sandbox TIMESTAMP read even on in-sandbox \
         failure; got {:?}",
        volatile,
    );
}

// ============================================================================
// OUTER EVM GAS DEBIT (REX5+)
// ============================================================================

/// REX5 sandbox: the outer keyless-deploy call's `gas_used` must include the
/// sandbox's `gas_used`; otherwise only the 100K dispatch overhead is counted.
#[test]
fn test_rex5_sandbox_outer_gas_used_includes_sandbox_gas_used_on_success() {
    let init_code = minimal_1byte_constructor(42);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let mut db = funded_signer_db(signer);
    let (result, _usage, _volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    let outer_gas_used = result.gas_used();
    let decoded = decode_keyless_deploy_return(&result);
    let sandbox_gas_used = decoded.gasUsed;

    assert!(
        outer_gas_used >= constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS + sandbox_gas_used,
        "outer gas_used ({}) must be >= overhead ({}) + sandbox gas_used ({})",
        outer_gas_used,
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
        sandbox_gas_used,
    );
}

/// REX4 parity: outer `gas_used` does NOT include `sandbox_gas_used`. Replay
/// determinism — pre-REX5 behavior must be preserved byte-for-byte.
#[test]
fn test_rex4_sandbox_outer_gas_used_excludes_sandbox_gas_used() {
    let init_code = minimal_1byte_constructor(42);
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let mut db = funded_signer_db(signer);
    let (result, _usage, _volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX4,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    let outer_gas_used = result.gas_used();
    let decoded = decode_keyless_deploy_return(&result);
    let sandbox_gas_used = decoded.gasUsed;

    assert!(
        outer_gas_used < constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS + sandbox_gas_used,
        "REX4: outer gas_used ({}) must NOT include sandbox gas_used ({}); only \
         dispatch overhead ({}) plus relayer-frame ops should be charged",
        outer_gas_used,
        sandbox_gas_used,
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
    );
}

/// REX5 sandbox: on in-sandbox failure (`EmptyCodeDeployed` via STOP-only
/// constructor), the outer `gas_used` must still include `sandbox_gas_used`.
#[test]
fn test_rex5_sandbox_outer_gas_used_includes_sandbox_gas_used_on_in_sandbox_failure() {
    let init_code = BytecodeBuilder::default().append(STOP).build();
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let mut db = funded_signer_db(signer);
    let (result, _usage, _volatile) = execute_keyless_deploy_with_volatile(
        MegaSpecId::REX5,
        &mut db,
        tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    let outer_gas_used = result.gas_used();
    let decoded = decode_keyless_deploy_return(&result);
    let sandbox_gas_used = decoded.gasUsed;
    assert!(!decoded.errorData.is_empty(), "in-sandbox failure should be encoded");
    assert!(
        outer_gas_used >= constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS + sandbox_gas_used,
        "in-sandbox failure: outer gas_used ({}) must include sandbox gas_used \
         ({}) + overhead ({})",
        outer_gas_used,
        sandbox_gas_used,
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
    );
}

/// REX5 sandbox: when the merged sandbox usage pushes the parent over a
/// TX-level compute-gas cap, `reject_if_tx_limit_overflow` halts the outer
/// call. Per the halt invariant the tx footprint side effects survive while
/// the state diff does not:
/// - Sandbox state diff NOT merged (deploy address has no code).
/// - Parent volatile bitmap IS merged (footprint survives halt).
/// - Parent multidim usage IS merged (footprint survives halt).
///
/// Uses `constructor_reads_timestamp` so the sandbox actually accesses
/// volatile data — without that the volatile assertion would be a no-op.
/// The tight `tx_compute_gas_limit` is derived from a successful baseline
/// of the same constructor, then shaved by 1 so the merged usage overshoots
/// by exactly one opcode.
///
/// Note: the alternative outer-OOG path (where the outer `Gas` cannot absorb the
/// sandbox's eventual cost) is unreachable under REX5. The sandbox enters under a
/// `gas.record_cost(gas_limit_override)` reservation that is capped to the outer's
/// remaining gas, so `sandbox_gas_used ≤ reservation` is structural — the post-frame
/// step is a pure `erase_cost` refund of the unused tail, never a `record_cost`.
#[test]
fn test_rex5_sandbox_volatile_bitmap_survives_residual_overflow_halt() {
    let init_code = constructor_reads_timestamp();
    // Get baseline usage for the same constructor on a successful deploy.
    let (tx_bytes, signer, deploy_address, baseline_usage) = rex5_baseline(init_code);
    assert!(baseline_usage.compute_gas > 0, "baseline should consume compute gas");

    // Tighten the parent's compute_gas budget so the sandbox's merged usage
    // overshoots by a single opcode, forcing the residual-overflow safety net
    // to halt the outer call after the sandbox has already merged its
    // multidim usage and volatile bitmap.
    let tx_limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_compute_gas_limit(baseline_usage.compute_gas.saturating_sub(1));

    // Run the keyless deploy with the tight limits AND extract the parent's
    // volatile tracker bitmap after the call returns. (`execute_keyless_deploy_with_limits`
    // drops the EVM internally, so we inline the EVM construction here to keep
    // the tracker reachable.)
    let mut db = funded_signer_db(signer);
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5)
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(tx_limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let outer_tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    let result_and_state = alloy_evm::Evm::transact_raw(&mut evm, outer_tx).unwrap();
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    let volatile = evm.ctx_ref().volatile_data_tracker.borrow().get_volatile_data_accessed();
    let result = result_and_state.result;

    // Outer halts at `reject_if_tx_limit_overflow` because the parent's
    // compute-gas cap was overshot by the merged sandbox usage.
    assert!(
        result.is_halt(),
        "outer should halt on residual compute-gas overflow, got: {result:?}"
    );

    // The halt reason must be the generic `ComputeGasLimitExceeded`, NOT the
    // volatile-specific `VolatileDataAccessOutOfGas`. The footprint merge
    // propagates the bitmap into the parent's volatile tracker but does NOT
    // call `set_compute_gas_limit` on the parent's `AdditionalLimit`; the
    // halt-reason classifier at `MegaHandler::execution_result` therefore
    // takes the generic `check_limit().maybe_halt_reason()` branch rather
    // than the `detained_compute_gas_halt_reason` branch. See the doc comment
    // on `VolatileDataAccessTracker::merge_accesses_from_bitmap` for the
    // intentional footprint-only scoping.
    match &result {
        ExecutionResult::Halt { reason, .. } => {
            assert!(
                matches!(reason, MegaHaltReason::ComputeGasLimitExceeded { .. }),
                "halt reason should be generic ComputeGasLimitExceeded; got {reason:?}"
            );
            assert!(
                !matches!(reason, MegaHaltReason::VolatileDataAccessOutOfGas { .. }),
                "halt reason MUST NOT be remapped to volatile-specific variant for \
                 a sandbox-only volatile access — detention state is not merged; \
                 got {reason:?}"
            );
        }
        other => panic!("expected Halt, got: {other:?}"),
    }

    // (1) State diff NOT merged — deployed account must not survive.
    let deployed_account = result_and_state.state.get(&deploy_address);
    let has_code = deployed_account
        .map(|acc| acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false))
        .unwrap_or(false);
    assert!(!has_code, "sandbox state must NOT be merged on residual overflow halt");

    // (2) Parent volatile bitmap IS merged — footprint survives the halt.
    assert!(
        volatile.contains(VolatileDataAccess::TIMESTAMP),
        "parent volatile tracker MUST include sandbox TIMESTAMP read on \
         residual overflow halt; got {:?}",
        volatile,
    );

    // (3) Parent's multidim usage IS merged. The dispatch path records the
    // fixed overhead into compute_gas BEFORE entering the sandbox, so any
    // proof that the sandbox's contribution survived must show compute_gas
    // strictly greater than that overhead alone.
    assert!(
        usage.compute_gas > constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
        "parent additional_limit.compute_gas must include sandbox contribution \
         on top of the {} dispatch overhead; got {} (multidim merge should \
         survive halt)",
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
        usage.compute_gas,
    );
}
