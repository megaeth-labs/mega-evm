//! Tests for gas costs in the Mega EVM.

use std::convert::Infallible;

use alloy_primitives::{address, keccak256, Bytes, TxKind, U256};
use mega_evm::{
    address_to_bucket_id,
    constants::{self, mini_rex::SSTORE_SET_GAS},
    slot_to_bucket_id,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, EVMError, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionError,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, CREATE2, GAS, LOG0, PUSH0},
    context::{result::ResultAndState, ContextTr, JournalTr, TxEnv},
    interpreter::{CallInputs, CallOutcome},
    primitives::Address,
    Inspector,
};
use salt::constant::MIN_BUCKET_SIZE;

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CALLEE: Address = address!("1000000000000000000000000000000000000001");
const NESTED_CALLEE: Address = address!("1000000000000000000000000000000000000002");

/// Executes a transaction on the EVM.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &DefaultExternalEnvs,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec, external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit: 1000000000000000000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

enum UpdateMode {
    /// Set a non-zero value when the original value is zero
    Set,
    /// Set a non-zero value when the original value is non-zero
    Reset,
    /// Set a zero value when the original value is non-zero
    Clear,
}

/// Executes an SSTORE operation test case, verifying gas usage for different storage write
/// scenarios.
///
/// # Arguments
///
/// * `spec` - The `MegaSpecId` specifying the EVM spec to use.
/// * `mode` - The `UpdateMode` indicating the type of storage operation:
///     - `Set`: Set a non-zero value to an originally zero storage slot.
///     - `Reset`: Set a non-zero value to a non-zero storage slot.
///     - `Clear`: Set a zero value to a non-zero storage slot.
/// * `expansion_times` - The number of times the storage bucket should be expanded (affects gas
///   cost).
/// * `expected_gas_used` - The expected amount of gas that should be used by the transaction.
///
/// This function sets up a contract that performs an SSTORE operation, configures the storage
/// and bucket meta as needed, executes the transaction, and asserts that the gas used matches
/// the expected value.
fn sstore_test_case(
    spec: MegaSpecId,
    mode: UpdateMode,
    expansion_times: u64,
    expected_gas_used: u64,
) {
    let mut db = MemoryDatabase::default();

    // Test address and storage slot
    let storage_key = U256::from(0);
    let storage_value = match mode {
        UpdateMode::Set | UpdateMode::Reset => U256::from(6342),
        UpdateMode::Clear => U256::from(0),
    };
    let bucket_id = slot_to_bucket_id(CALLEE, storage_key);
    // An external envs with the given bucket capacity
    let external_envs = DefaultExternalEnvs::new().with_bucket_capacity(
        bucket_id,
        0,
        MIN_BUCKET_SIZE as u64 * (expansion_times + 1),
    );

    // a contract that stores a value to the storage slot
    let bytecode = BytecodeBuilder::default().sstore(storage_key, storage_value).stop().build();
    db.set_account_code(CALLEE, bytecode);
    if matches!(mode, UpdateMode::Clear | UpdateMode::Reset) {
        db.set_account_storage(CALLEE, storage_key, U256::from(2333));
    }

    let res = transact(
        spec,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Default::default(),
        U256::ZERO,
    )
    .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests SSTORE setting a zero slot to non-zero value without bucket expansion, expecting 2M+ gas
/// due to high `SSTORE_SET_GAS` cost plus EIP-2929 cold access penalty.
#[test]
fn test_sstore_no_bucket_expansion() {
    sstore_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        0,
        21_006 + SSTORE_SET_GAS + constants::equivalence::COLD_SLOAD_COST,
    );
}

/// Tests SSTORE with single bucket expansion, expecting doubled gas cost (4M+ gas) due to bucket
/// capacity doubling, plus EIP-2929 cold access penalty.
#[test]
fn test_sstore_with_bucket_expansion_once() {
    sstore_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        1,
        21_006 + SSTORE_SET_GAS * 2 + constants::equivalence::COLD_SLOAD_COST,
    );
}

/// Tests SSTORE with 10x bucket expansion, expecting 10x gas cost (20M+ gas) due to linear scaling
/// with bucket capacity, plus EIP-2929 cold access penalty.
#[test]
fn test_sstore_with_bucket_expansion_ten_times() {
    sstore_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        9,
        21_006 + SSTORE_SET_GAS * 10 + constants::equivalence::COLD_SLOAD_COST,
    );
}

/// Tests SSTORE resetting non-zero to different non-zero value, expecting standard gas cost (base +
/// `WARM_SSTORE_RESET`) plus EIP-2929 cold access penalty.
#[test]
fn test_sstore_reset() {
    sstore_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Reset,
        0,
        23_906 + constants::equivalence::COLD_SLOAD_COST,
    );
}

/// Tests SSTORE reset with bucket expansion, expecting same gas cost as without expansion since no
/// new storage allocation, plus EIP-2929 cold access penalty.
#[test]
fn test_sstore_reset_with_bucket_expansion() {
    sstore_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Reset,
        1,
        23_906 + constants::equivalence::COLD_SLOAD_COST,
    );
}

/// Tests SSTORE clearing non-zero to zero value.
///
/// Gas calculation with EIP-2929 (cold access) and EIP-3529 (refund):
/// 1. Base tx: 21,000
/// 2. SSTORE cost: `WARM_SSTORE_RESET` (2900) + `COLD_SLOAD_COST` (2100) = 5,000
/// 3. Bytecode overhead: ~6 gas (PUSH, STOP, etc.)
/// 4. Total charged: 26,006
/// 5. Refund (EIP-3529): min(4800, 26006/5) = 4,800
///    - `sstore_clears_schedule` = `SSTORE_RESET` - `COLD_SLOAD_COST` + `ACCESS_LIST_STORAGE_KEY`
///    - = 5000 - 2100 + 1900 = 4,800
/// 6. Final gas used: 26,006 - 4,800 = 21,206
#[test]
fn test_sstore_clear() {
    sstore_test_case(MegaSpecId::MINI_REX, UpdateMode::Clear, 0, 21_206);
}

/// Tests SSTORE clear with bucket expansion. Bucket expansion doesn't affect clearing operations
/// since no new storage is allocated. Same gas cost as `test_sstore_clear` (21,206) due to:
/// - SSTORE charges: `WARM_SSTORE_RESET` + `COLD_SLOAD_COST` = 5,000
/// - EIP-3529 refund: 4,800
/// - Net SSTORE cost: 200 gas (plus ~6 overhead)
#[test]
fn test_sstore_clear_with_bucket_expansion() {
    sstore_test_case(MegaSpecId::MINI_REX, UpdateMode::Clear, 1, 21_206);
}

/// Tests SSTORE uses standard EVM gas costs in equivalence spec, not `MegaETH`'s increased costs.
#[test]
fn test_sstore_gas_unchanged_in_equivalence_spec() {
    sstore_test_case(
        MegaSpecId::EQUIVALENCE,
        UpdateMode::Set,
        0,
        21_006 + constants::equivalence::SSTORE_SET + constants::equivalence::COLD_SLOAD_COST,
    );
}

/// Tests EIP-2929: First SSTORE to a slot charges cold access penalty (2100 gas), subsequent
/// SSTOREs to the same slot in the same transaction use warm pricing.
#[test]
fn test_sstore_cold_then_warm_access() {
    let mut db = MemoryDatabase::default();

    let storage_key = U256::from(0);
    let bucket_id = slot_to_bucket_id(CALLEE, storage_key);
    let external_envs =
        DefaultExternalEnvs::new().with_bucket_capacity(bucket_id, 0, MIN_BUCKET_SIZE as u64);

    // Contract that performs two SSTOREs to the same slot:
    // 1. First SSTORE (cold): should charge SSTORE_SET_GAS + COLD_SLOAD_COST
    // 2. Second SSTORE (warm): should only charge WARM_STORAGE_READ_COST (no cold penalty)
    let bytecode = BytecodeBuilder::default()
        .sstore(storage_key, U256::from(100)) // Cold access
        .sstore(storage_key, U256::from(200)) // Warm access
        .stop()
        .build();
    db.set_account_code(CALLEE, bytecode);

    let res = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Default::default(),
        U256::ZERO,
    )
    .unwrap();
    assert!(res.result.is_success());

    // Expected gas breakdown:
    // - Base: 21_000
    // - Bytecode execution overhead: ~12 gas (PUSH opcodes, STOP, etc.)
    // - First SSTORE (cold): SSTORE_SET_GAS + COLD_SLOAD_COST
    // - Second SSTORE (warm, overwriting non-zero with non-zero): WARM_STORAGE_READ_COST
    let expected_gas = 21_000
        + 12 // bytecode overhead
        + SSTORE_SET_GAS
        + constants::equivalence::COLD_SLOAD_COST
        + constants::equivalence::WARM_STORAGE_READ_COST;
    assert_eq!(res.result.gas_used(), expected_gas);
}

/// Executes an ether transfer test case, verifying gas usage for account creation and bucket
/// expansion scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `mode`: The `UpdateMode` indicating whether the transfer is to a new account (`Set`) or an
///   existing one (`Reset`).
/// - `expansion_times`: Number of times to expand the storage bucket (affects gas cost for new
///   account creation).
/// - `expected_gas_used`: The expected gas usage for the transfer, including any additional costs.
///
/// # Panics
/// Panics if `mode` is not `Set` or `Reset`.
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn ether_transfer_test_case(
    spec: MegaSpecId,
    mode: UpdateMode,
    expansion_times: u64,
    expected_gas_used: u64,
) {
    assert!(matches!(mode, UpdateMode::Set | UpdateMode::Reset), "Mode must be Set or Reset");

    let mut db = MemoryDatabase::default();

    // Determine the bucket for the callee and set up the external envs with the required capacity.
    let bucket_id = address_to_bucket_id(CALLEE);
    let external_envs = DefaultExternalEnvs::new().with_bucket_capacity(
        bucket_id,
        0,
        MIN_BUCKET_SIZE as u64 * (expansion_times + 1),
    );

    // Allocate initial balance to the caller.
    db.set_account_balance(CALLER, U256::from(1000));
    // If testing transfer to an existing account, allocate balance to the callee as well.
    if matches!(mode, UpdateMode::Reset) {
        db.set_account_balance(CALLEE, U256::from(2333));
    }

    // Perform the ether transfer (1 wei) from caller to callee.
    let res = transact(
        spec,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Default::default(),
        U256::from(1),
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests ether transfer to new account charges increased `NEW_ACCOUNT_GAS` in `MegaETH`.
#[test]
fn test_ether_transfer_to_non_existent_account() {
    ether_transfer_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        0,
        21_000 + constants::mini_rex::NEW_ACCOUNT_GAS,
    );
}

/// Tests ether transfer with bucket expansion doubles the `NEW_ACCOUNT_GAS` cost.
#[test]
fn test_ether_transfer_to_non_existent_account_with_bucket_expansion() {
    ether_transfer_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        1,
        21_000 + constants::mini_rex::NEW_ACCOUNT_GAS * 2,
    );
}

/// Tests ether transfer to existing account only charges base transaction gas.
#[test]
fn test_ether_transfer_to_existent_account() {
    ether_transfer_test_case(MegaSpecId::MINI_REX, UpdateMode::Reset, 0, 21_000);
}

/// Tests ether transfer uses standard EVM gas costs in equivalence spec.
#[test]
fn test_ether_transfer_gas_unchanged_in_equivalence_spec() {
    ether_transfer_test_case(MegaSpecId::EQUIVALENCE, UpdateMode::Reset, 0, 21_000);
}

/// Executes a nested ether transfer test case, verifying gas usage for account creation and bucket
/// expansion scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `mode`: The `UpdateMode` indicating whether the transfer is to a new account (`Set`) or an
///   existing one (`Reset`).
/// - `expansion_times`: Number of times to expand the storage bucket (affects gas cost for new
///   account creation).
/// - `expected_gas_used`: The expected gas usage for the transfer, including any additional costs.
///
/// # Panics
/// Panics if `mode` is not `Set` or `Reset`.
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn nested_ether_transfer_test_case(
    spec: MegaSpecId,
    mode: UpdateMode,
    expansion_times: u64,
    expected_gas_used: u64,
) {
    assert!(matches!(mode, UpdateMode::Set | UpdateMode::Reset), "Mode must be Set or Reset");

    let mut db = MemoryDatabase::default();

    // Test address and storage slot
    let bucket_id = address_to_bucket_id(NESTED_CALLEE);
    // An external envs with the given bucket capacity
    let external_envs = DefaultExternalEnvs::new().with_bucket_capacity(
        bucket_id,
        0,
        MIN_BUCKET_SIZE as u64 * (expansion_times + 1),
    );

    // allocate some balance to callee, which will transfer the ether to the nested callee
    db.set_account_balance(CALLEE, U256::from(1000));
    if matches!(mode, UpdateMode::Reset) {
        db.set_account_balance(NESTED_CALLEE, U256::from(2333));
    }

    // set the code of the calee that transfers ether to the nested callee
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // args and returns memory
        .push_number(1_u8) // value: 1 wei
        .push_address(NESTED_CALLEE) // callee
        .append(GAS)
        .append(CALL)
        .stop()
        .build();
    db.set_account_code(CALLEE, bytecode);

    // transfer 1 wei from caller to callee
    let res = transact(
        spec,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests nested ether transfer (via CALL) to new account charges increased `NEW_ACCOUNT_GAS`.
#[test]
fn test_nested_ether_transfer_to_non_existent_account() {
    nested_ether_transfer_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        0,
        30_316 + constants::mini_rex::NEW_ACCOUNT_GAS,
    );
}

/// Tests nested ether transfer with bucket expansion doubles the `NEW_ACCOUNT_GAS` cost.
#[test]
fn test_nested_ether_transfer_to_non_existent_account_with_bucket_expansion() {
    nested_ether_transfer_test_case(
        MegaSpecId::MINI_REX,
        UpdateMode::Set,
        1,
        30_316 + constants::mini_rex::NEW_ACCOUNT_GAS * 2,
    );
}

/// Tests nested ether transfer to existing account only charges base CALL gas.
#[test]
fn test_nested_ether_transfer_to_existent_account() {
    nested_ether_transfer_test_case(MegaSpecId::MINI_REX, UpdateMode::Reset, 0, 30_316);
}

/// Tests nested ether transfer uses standard EVM gas costs in equivalence spec.
#[test]
fn test_nested_ether_transfer_gas_unchanged_in_equivalence_spec() {
    nested_ether_transfer_test_case(MegaSpecId::EQUIVALENCE, UpdateMode::Reset, 0, 30_316);
}

/// Executes a create contract test case, verifying gas usage for account creation and bucket
/// expansion scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `expansion_times`: Number of times to expand the storage bucket (affects gas cost for new
///   account creation).
/// - `expected_gas_used`: The expected gas usage for the contract creation, including any
///   additional costs.
///
/// # Panics
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn create_contract_test_case(spec: MegaSpecId, expansion_times: u64, expected_gas_used: u64) {
    let mut db = MemoryDatabase::default();

    // Test address and storage slot
    let callee = CALLER.create(0);
    let bucket_id = address_to_bucket_id(callee);
    // An external envs with the given bucket capacity
    let external_envs = DefaultExternalEnvs::new().with_bucket_capacity(
        bucket_id,
        0,
        MIN_BUCKET_SIZE as u64 * (expansion_times + 1),
    );

    // constructor code
    let constructor_code = BytecodeBuilder::default().return_with_data([0x00]).build();
    // let tokens = revm::interpreter::gas::get_tokens_in_calldata(&constructor_code, true);
    // println!("calldata_tokens: {:?}", tokens);

    // create contract
    let res = transact(spec, &mut db, &external_envs, CALLER, None, constructor_code, U256::ZERO)
        .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests CREATE contract charges increased `NEW_ACCOUNT_GAS` and additional code deposit gas.
#[test]
fn test_create_contract_to_non_existent_account() {
    create_contract_test_case(
        MegaSpecId::MINI_REX,
        0,
        53_554 +
            constants::mini_rex::NEW_ACCOUNT_GAS +
            constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS +
            constants::mini_rex::CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS * 83,
    );
}

/// Tests CREATE with bucket expansion doubles the `NEW_ACCOUNT_GAS` cost.
#[test]
fn test_create_contract_to_non_existent_account_with_bucket_expansion() {
    create_contract_test_case(
        MegaSpecId::MINI_REX,
        1,
        53_554 +
            constants::mini_rex::NEW_ACCOUNT_GAS * 2 +
            constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS +
            constants::mini_rex::CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS * 83,
    );
}

/// Tests CREATE uses standard EVM gas costs in equivalence spec.
#[test]
fn test_create_contract_gas_unchanged_in_equivalence_spec() {
    create_contract_test_case(MegaSpecId::EQUIVALENCE, 0, 53_554);
}

/// Executes a nested create contract test case, verifying gas usage for account creation and bucket
/// expansion scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `is_create2`: Whether to use CREATE2 for the contract creation.
/// - `expansion_times`: Number of times to expand the storage bucket (affects gas cost for new
///   account creation).
/// - `expected_gas_used`: The expected gas usage for the contract creation, including any
///   additional costs.
///
/// # Panics
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn nested_create_contract_test_case(
    spec: MegaSpecId,
    is_create2: bool,
    expansion_times: u64,
    expected_gas_used: u64,
) {
    let mut db = MemoryDatabase::default();
    let create2_salt = U256::from(6342);
    // constructor code
    let constructor_code = BytecodeBuilder::default().return_with_data([0x00]).build();
    let constructor_len = constructor_code.len();

    // Test address and storage slot
    let nested_callee = if is_create2 {
        CALLEE.create2(create2_salt.to_be_bytes(), keccak256(&constructor_code))
    } else {
        CALLEE.create(0)
    };
    let bucket_id = address_to_bucket_id(nested_callee);
    // An external envs with the given bucket capacity
    let external_envs = DefaultExternalEnvs::new().with_bucket_capacity(
        bucket_id,
        0,
        MIN_BUCKET_SIZE as u64 * (expansion_times + 1),
    );

    // set the code of the calee that transfers ether to the nested callee
    let mut bytecode = BytecodeBuilder::default();
    if is_create2 {
        bytecode = bytecode.push_u256(create2_salt);
    }
    let bytecode = bytecode
        .mstore(0, constructor_code)
        .push_number(constructor_len as u64) // call data
        .push_number(0u8)
        .push_number(0u8) // 0 value
        .append(if is_create2 { CREATE2 } else { CREATE })
        .stop()
        .build();
    db.set_account_code(CALLEE, bytecode);

    // transfer 1 wei from caller to callee
    let res = transact(
        spec,
        &mut db,
        &external_envs,
        CALLER,
        Some(CALLEE),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests nested CREATE (via CALL) charges `CREATE_GAS` plus `NEW_ACCOUNT_GAS`.
#[test]
fn test_nested_create_contract_to_non_existent_account() {
    nested_create_contract_test_case(
        MegaSpecId::MINI_REX,
        false,
        0,
        21_255 +
            constants::mini_rex::CREATE_GAS +
            constants::mini_rex::NEW_ACCOUNT_GAS +
            constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS,
    );
}

/// Tests nested CREATE with bucket expansion doubles the `NEW_ACCOUNT_GAS` cost.
#[test]
fn test_nested_create_contract_to_non_existent_account_with_bucket_expansion() {
    nested_create_contract_test_case(
        MegaSpecId::MINI_REX,
        false,
        1,
        21_255 +
            constants::mini_rex::CREATE_GAS +
            constants::mini_rex::NEW_ACCOUNT_GAS * 2 +
            constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS,
    );
}

/// Tests nested CREATE uses standard EVM gas costs in equivalence spec.
#[test]
fn test_nested_create_contract_gas_unchanged_in_equivalence_spec() {
    nested_create_contract_test_case(
        MegaSpecId::EQUIVALENCE,
        false,
        0,
        21_255 + constants::equivalence::CREATE,
    );
}

/// Tests nested CREATE2 charges `CREATE_GAS` plu`NEW_ACCOUNT_GAS`AS plus KECCAK256 cost.
#[test]
fn test_nested_create2_contract_to_non_existent_account() {
    nested_create_contract_test_case(
        MegaSpecId::MINI_REX,
        true,
        0,
        21_270 +
            constants::mini_rex::CREATE_GAS +
            constants::mini_rex::NEW_ACCOUNT_GAS +
            constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS,
    );
}

/// Tests nested CREATE2 with bucket expansion doubles the `NEW_ACCOUNT_GAS` cost.
#[test]
fn test_nested_create2_contract_to_non_existent_account_with_bucket_expansion() {
    nested_create_contract_test_case(
        MegaSpecId::MINI_REX,
        true,
        1,
        21_270 +
            constants::mini_rex::CREATE_GAS +
            constants::mini_rex::NEW_ACCOUNT_GAS * 2 +
            constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS,
    );
}

/// Tests nested CREATE2 uses standard EVM gas costs in equivalence spec.
#[test]
fn test_nested_create2_contract_gas_unchanged_in_equivalence_spec() {
    nested_create_contract_test_case(
        MegaSpecId::EQUIVALENCE,
        true,
        0,
        21_270 + constants::equivalence::CREATE,
    );
}

/// Executes a calldata test case, verifying gas usage for calldata scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `expected_gas_used`: The expected gas usage for the calldata, including any additional costs.
///
/// # Panics
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn calldata_test_case<const CALLDATA_LEN: usize>(spec: MegaSpecId, expected_gas_used: u64) {
    // calldata
    let calldata = Bytes::from([0x00; CALLDATA_LEN]);

    let mut db = MemoryDatabase::default();
    let res = transact(
        spec,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        calldata,
        U256::ZERO,
    )
    .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests calldata charges additional per-byte cost in `MegaETH` (100x standard cost).
#[test]
fn test_calldata_additional_cost() {
    calldata_test_case::<1024>(
        MegaSpecId::MINI_REX,
        123_400, // This test uses a different calldata pattern than our floor gas tests
    );
}

/// Tests calldata uses standard EVM gas costs in equivalence spec.
#[test]
fn test_calldata_no_additional_cost_in_equivalence_spec() {
    calldata_test_case::<1024>(MegaSpecId::EQUIVALENCE, 31_240);
}

/// Executes a log test case, verifying gas usage for log scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `expected_gas_used`: The expected gas usage for the log, including any additional costs.
///
/// # Panics
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn log_test_case<const TOPIC_COUNT: usize, const DATA_LEN: usize>(
    spec: MegaSpecId,
    expected_gas_used: u64,
) {
    assert!(TOPIC_COUNT <= 4, "Topic count must be less than or equal to 4");
    let mut db = MemoryDatabase::default();

    // contract code that log some data
    let mut bytecode = BytecodeBuilder::default();
    for _ in 0..TOPIC_COUNT {
        bytecode = bytecode.push_number(1u8); // topic
    }
    bytecode = bytecode.push_number(DATA_LEN as u64); // data length
    bytecode = bytecode.push_number(0u8); // data offset
    let bytecode = bytecode.append(LOG0 + TOPIC_COUNT as u8).stop().build();
    db.set_account_code(CALLEE, bytecode);

    let res = transact(
        spec,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Default::default(),
        U256::ZERO,
    )
    .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests LOG charges additional per-byte and per-topic cost in `MegaETH` (100x standard cost).
#[test]
fn test_log_additional_cost() {
    log_test_case::<1, 1024>(
        MegaSpecId::MINI_REX,
        21_482 + constants::mini_rex::LOG_TOPIC_GAS + constants::mini_rex::LOG_DATA_GAS * 1024,
    );
}

/// Tests LOG uses standard EVM gas costs in equivalence spec.
#[test]
fn test_log_no_additional_cost_in_equivalence_spec() {
    log_test_case::<1, 1024>(MegaSpecId::EQUIVALENCE, 30_049);
}

/// Executes a gas forward test case, verifying gas usage for gas forwarding scenarios.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `is_create`: Whether to use CREATE for the contract creation.
/// - `approx_expected_forwarded_gas`: The expected gas usage for the gas forwarding, including any
///   additional costs.
///
/// # Panics
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn gas_forward_test_case(spec: MegaSpecId, is_create: bool, approx_expected_forwarded_gas: u64) {
    let mut db = MemoryDatabase::default();

    struct CallGasInspector {
        approx_expected_forwarded_gas: u64,
        reached: bool,
    }

    impl<CTX: ContextTr> Inspector<CTX> for CallGasInspector {
        fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
            let depth = context.journal().depth();
            if depth == 1 {
                self.reached = true;
                // inner call
                assert!(
                    inputs.gas_limit >= self.approx_expected_forwarded_gas * 99 / 100 &&
                        inputs.gas_limit <= self.approx_expected_forwarded_gas * 101 / 100,
                    "expected forwarded gas is not correct"
                );
            }
            None
        }
        fn create(
            &mut self,
            context: &mut CTX,
            inputs: &mut revm::interpreter::CreateInputs,
        ) -> Option<revm::interpreter::CreateOutcome> {
            let depth = context.journal().depth();
            if depth == 1 {
                self.reached = true;
                // inner create
                assert!(
                    inputs.gas_limit >= self.approx_expected_forwarded_gas * 99 / 100 &&
                        inputs.gas_limit <= self.approx_expected_forwarded_gas * 101 / 100,
                    "expected forwarded gas is not correct"
                );
            }
            None
        }
    }

    let bytecode = if is_create {
        BytecodeBuilder::default()
            .push_number(10u8)
            .append_many([PUSH0, PUSH0, CREATE])
            .stop()
            .build()
    } else {
        BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(NESTED_CALLEE)
            .append(GAS)
            .append(CALL)
            .stop()
            .build()
    };
    db.set_account_code(CALLEE, bytecode);

    let mut context = MegaContext::new(db, spec, DefaultExternalEnvs::default());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut inspector = CallGasInspector { approx_expected_forwarded_gas, reached: false };
    let mut evm = MegaEvm::new(context).with_inspector(&mut inspector);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: 1_024_000_000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let res = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(res.is_ok());
    assert!(inspector.reached);
}

/// Tests gas forwarding to CALL uses 98/100 rule in `MegaETH` (instead of 63/64).
#[test]
fn test_gas_forward_to_call_in_mini_rex() {
    gas_forward_test_case(MegaSpecId::MINI_REX, false, 1_003_520_000);
}

/// Tests gas forwarding to CREATE uses 98/100 rule in `MegaETH` (instead of 63/64).
#[test]
fn test_gas_forward_to_create_in_mini_rex() {
    gas_forward_test_case(MegaSpecId::MINI_REX, true, 1_003_520_000);
}

/// Tests gas forwarding to CALL uses standard 63/64 rule in equivalence spec.
#[test]
fn test_gas_forward_to_call_in_equivalence_spec() {
    gas_forward_test_case(MegaSpecId::EQUIVALENCE, false, 1_008_000_000);
}

/// Tests gas forwarding to CREATE uses standard 63/64 rule in equivalence spec.
#[test]
fn test_gas_forward_to_create_in_equivalence_spec() {
    gas_forward_test_case(MegaSpecId::EQUIVALENCE, true, 1_008_000_000);
}

/// Executes a floor gas test case, verifying gas usage includes additional floor gas costs.
///
/// # Arguments
/// - `spec`: The `MegaSpecId` specifying the EVM spec to use (e.g., `MINI_REX`, `EQUIVALENCE`).
/// - `calldata_size`: Size of calldata in bytes to test floor gas calculation.
/// - `expected_gas_used`: The expected gas usage including floor gas costs.
///
/// # Panics
/// Panics if the transaction fails or if the actual gas used does not match `expected_gas_used`.
fn floor_gas_test_case(spec: MegaSpecId, calldata_size: usize, expected_gas_used: u64) {
    let mut db = MemoryDatabase::default();

    // Create calldata of specified size
    let calldata = Bytes::from(vec![0x42; calldata_size]);

    let res = transact(
        spec,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        calldata,
        U256::ZERO,
    )
    .unwrap();
    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    assert_eq!(gas_used, expected_gas_used);
}

/// Tests floor gas charges additional cost for calldata in `MINI_REX` spec.
#[test]
fn test_floor_gas_calldata_mini_rex() {
    // Test with 100 bytes of calldata
    floor_gas_test_case(MegaSpecId::MINI_REX, 100, 61_000);
}

/// Tests floor gas charges additional cost for large calldata in `MINI_REX` spec.
#[test]
fn test_floor_gas_large_calldata_mini_rex() {
    // Test with 1024 bytes of calldata
    floor_gas_test_case(MegaSpecId::MINI_REX, 1024, 430_600);
}

/// Tests floor gas is not charged in EQUIVALENCE spec.
#[test]
fn test_floor_gas_no_additional_cost_in_equivalence_spec() {
    // Test with 100 bytes of calldata - should not include additional floor gas
    floor_gas_test_case(MegaSpecId::EQUIVALENCE, 100, 25_000);
}

/// Tests floor gas with empty calldata (edge case).
#[test]
fn test_floor_gas_empty_calldata() {
    floor_gas_test_case(MegaSpecId::MINI_REX, 0, 21_000);
}

/// Tests floor gas with minimal calldata (1 byte).
#[test]
fn test_floor_gas_minimal_calldata() {
    floor_gas_test_case(MegaSpecId::MINI_REX, 1, 21_400);
}
