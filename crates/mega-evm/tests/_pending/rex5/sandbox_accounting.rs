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
