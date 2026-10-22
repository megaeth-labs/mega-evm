//! Tests for non-delegating CALL/CREATE metering.
//!
//! REX5 changes the CALL and CREATE storage-gas wrappers to use `inspect_account`
//! (non-delegating) instead of `inspect_account_delegated`.
//! `StateGrowthTracker::before_frame_init` also uses non-delegating inspection.
//!
//! This matters when the CALL target is an EIP-7702 authority whose delegate is an
//! empty (non-existent) address:
//!
//! - **Pre-REX5** (`inspect_account_delegated`): follows delegation, sees the empty delegate, and
//!   treats the target as empty. This causes overcharging: new-account storage gas premium is
//!   applied even though the authority itself already exists, and state growth is incorrectly
//!   recorded.
//!
//! - **REX5** (`inspect_account`): inspects the authority directly, sees it has EIP-7702 delegation
//!   bytecode (non-empty), and correctly skips the premium.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, SaltEnv,
    TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ResultAndState, tx::TxEnvBuilder, TxEnv},
    database::AccountState,
    handler::EvmTr,
    state::Bytecode,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000810000");
const PARENT: Address = address!("0000000000000000000000000000000000810001");
/// An EIP-7702 authority that delegates to `EMPTY_DELEGATE`.
const AUTHORITY: Address = address!("0000000000000000000000000000000000810002");
/// A non-existent address that AUTHORITY delegates to.
const EMPTY_DELEGATE: Address = address!("0000000000000000000000000000000000810099");

// ============================================================================
// HELPERS
// ============================================================================

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (r, usage)
}

/// Sets EIP-7702 delegation bytecode on an account, making it delegate to `delegate_to`.
///
/// This produces the same `0xef0100 || address` designator that revm's
/// `apply_eip7702_auth_list` writes during Type 4 transaction processing.
fn set_eip7702_delegation(db: &mut MemoryDatabase, address: Address, delegate_to: Address) {
    let bytecode = Bytecode::new_eip7702(delegate_to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).unwrap();
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

/// Builds a contract that CALLs `target` with 1 wei value and empty calldata.
///
/// The value transfer is necessary to trigger the new-account storage gas logic in
/// `wrap_call_with_storage_gas!` — only CALLs with value to empty accounts are
/// charged the new-account premium.
fn build_call_with_value_contract(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // retLength
        .append(PUSH0) // retOffset
        .append(PUSH0) // argsLength
        .append(PUSH0) // argsOffset
        .push_number(1u8) // value = 1 wei
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(STOP)
        .build()
}

// ============================================================================
// TESTS
// ============================================================================

/// CALL with value to an EIP-7702 authority (whose delegate is empty) should
/// record LESS state growth in REX5 than REX4.
///
/// - REX4: `inspect_account_delegated` follows delegation to the empty delegate, treating the
///   target as empty. This records state growth for a "new account" even though the authority
///   itself exists.
/// - REX5: `inspect_account` sees the authority itself with EIP-7702 code (non-empty). No
///   new-account state growth is recorded.
///
/// Note: the new-account *storage gas* premium is also affected, but with the test's
/// `EmptyExternalEnv` (min bucket, multiplier=1), the premium formula yields zero.
/// The observable difference is in the state growth tracker.
#[test]
fn test_call_with_value_to_eip7702_authority_rex5_vs_rex4() {
    let build_db = || {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_balance(PARENT, U256::from(1_000_000_000u64));
        db.set_account_code(PARENT, build_call_with_value_contract(AUTHORITY));
        // Set up authority: delegates to EMPTY_DELEGATE (which does not exist).
        // Give authority some balance so it is a "real" account in the DB.
        set_eip7702_delegation(&mut db, AUTHORITY, EMPTY_DELEGATE);
        db.set_account_balance(AUTHORITY, U256::from(1_000u64));
        db
    };

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let (result_rex5, usage_rex5) = transact(MegaSpecId::REX5, &mut build_db(), tx.clone());
    assert!(result_rex5.result.is_success(), "REX5 should succeed: {result_rex5:?}");

    let (result_rex4, usage_rex4) = transact(MegaSpecId::REX4, &mut build_db(), tx);
    assert!(result_rex4.result.is_success(), "REX4 should succeed: {result_rex4:?}");

    // REX4 should record state growth for the "new account" (via delegation to empty).
    // REX5 should NOT record state growth because the authority itself is non-empty.
    assert!(
        usage_rex4.state_growth > usage_rex5.state_growth,
        "REX4 state growth ({}) should exceed REX5 ({}) — \
         REX4 incorrectly counts authority as new account via delegation",
        usage_rex4.state_growth,
        usage_rex5.state_growth,
    );

    // REX5 should have zero state growth from the CALL target.
    assert_eq!(
        usage_rex5.state_growth, 0,
        "REX5 should not record state growth for CALL to existing EIP-7702 authority: {}",
        usage_rex5.state_growth
    );

    // REX4 should have non-zero state growth because it follows delegation to the empty
    // delegate and treats the target as a new account.
    assert!(
        usage_rex4.state_growth > 0,
        "REX4 should record state growth (treats delegated-to-empty as new account): {}",
        usage_rex4.state_growth
    );
}

/// CALL with value to an EIP-7702 authority whose delegate is empty should NOT
/// record state growth in REX5.
///
/// The authority itself has delegation bytecode so `inspect_account` sees it as
/// non-empty.
/// No new-account creation occurs, so state growth should be zero from this path.
#[test]
fn test_rex5_call_to_eip7702_authority_no_state_growth() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(PARENT, U256::from(1_000_000_000u64));
    db.set_account_code(PARENT, build_call_with_value_contract(AUTHORITY));
    set_eip7702_delegation(&mut db, AUTHORITY, EMPTY_DELEGATE);
    db.set_account_balance(AUTHORITY, U256::from(1_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // The authority is non-empty (has EIP-7702 code), so no new account is created.
    assert_eq!(
        usage.state_growth, 0,
        "REX5 should not record state growth for CALL to existing EIP-7702 authority: {}",
        usage.state_growth
    );
}

/// CREATE inside an EIP-7702 delegated frame: REX5 uses the authority's nonce
/// for SALT bucket lookup, REX4 uses the delegate's nonce (wrong).
///
/// To make the difference observable, we set up divergent SALT bucket capacities:
/// - The address computed from authority nonce gets a LOW bucket (1x multiplier).
/// - The address computed from delegate nonce gets a HIGH bucket (2x multiplier).
///
/// REX4 follows delegation, uses the delegate's nonce → queries the high-capacity
/// bucket → charges more storage gas.
/// REX5 inspects the authority directly → queries the low-capacity bucket → charges less.
#[test]
fn test_rex5_create_inside_delegated_frame_uses_authority_nonce() {
    // Init code: returns 1 byte of runtime code.
    let init_code_bytes: &[u8] = &[
        PUSH1, 1, // size = 1
        PUSH1, 0, // offset = 0
        PUSH1, 0, // destOffset = 0
        CODECOPY, PUSH1, 1, // size = 1
        PUSH1, 0, // offset = 0
        RETURN, STOP,
    ];

    // Delegate bytecode: stores init code in memory, then CREATE.
    let delegate_code = BytecodeBuilder::default()
        .mstore(0, init_code_bytes)
        .push_number(0u64) // value = 0
        .push_number(init_code_bytes.len() as u64) // size
        .push_number((32 - init_code_bytes.len()) as u64) // offset (right-aligned in 32-byte word)
        .append(CREATE)
        .append(POP)
        .stop()
        .build();

    let delegate_addr = address!("0000000000000000000000000000000000810050");
    let authority_nonce: u64 = 5;
    let delegate_nonce: u64 = 0; // delegate contract has nonce 0 in DB

    // Compute the two candidate created addresses.
    // Revm always creates at AUTHORITY.create(authority_nonce) regardless of metering bug.
    // The metering code queries create_contract_storage_gas for one of these addresses:
    let addr_from_authority_nonce = AUTHORITY.create(authority_nonce);
    let addr_from_delegate_nonce = AUTHORITY.create(delegate_nonce);
    assert_ne!(
        addr_from_authority_nonce, addr_from_delegate_nonce,
        "Divergent nonces must produce different addresses"
    );

    // Set up SALT buckets: high capacity for the wrong address (delegate nonce path).
    let high_capacity = (MIN_BUCKET_SIZE as u64) * 3;
    let wrong_bucket =
        <TestExternalEnvs<std::convert::Infallible> as SaltEnv>::bucket_id_for_account(
            addr_from_delegate_nonce,
        );
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_bucket_capacity(wrong_bucket, high_capacity);

    let build_db = || {
        let mut db =
            MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000u64));
        db.set_account_code(delegate_addr, delegate_code.clone());
        set_eip7702_delegation(&mut db, AUTHORITY, delegate_addr);
        db.set_account_balance(AUTHORITY, U256::from(1_000_000u64));
        db.set_account_nonce(AUTHORITY, authority_nonce);

        // CALL the authority → code executes from delegate, target_address = AUTHORITY.
        let caller_code = BytecodeBuilder::default()
            .append(PUSH0)
            .append(PUSH0)
            .append(PUSH0)
            .append(PUSH0)
            .append(PUSH0)
            .push_address(AUTHORITY)
            .append(GAS)
            .append(CALL)
            .append(STOP)
            .build();
        db.set_account_code(PARENT, caller_code);
        db
    };

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    // REX5: uses authority's nonce → correct bucket (low capacity, 1x multiplier).
    let mut db_rex5 = build_db();
    let mut ctx_rex5 = MegaContext::new(&mut db_rex5, MegaSpecId::REX5)
        .with_external_envs((&external_envs).into());
    ctx_rex5.modify_chain(|c| {
        c.operator_fee_scalar = Some(U256::from(0));
        c.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm_rex5 = MegaEvm::new(ctx_rex5);
    let mut tx_rex5 = MegaTransaction::new(tx.clone());
    tx_rex5.enveloped_tx = Some(Bytes::new());
    let r5 = alloy_evm::Evm::transact_raw(&mut evm_rex5, tx_rex5).unwrap();
    assert!(r5.result.is_success(), "REX5 should succeed: {:?}", r5.result);
    let gas_rex5 = r5.result.gas_used();

    // REX4: uses delegate's nonce → wrong bucket (high capacity, 3x multiplier).
    let mut db_rex4 = build_db();
    let mut ctx_rex4 = MegaContext::new(&mut db_rex4, MegaSpecId::REX4)
        .with_external_envs((&external_envs).into());
    ctx_rex4.modify_chain(|c| {
        c.operator_fee_scalar = Some(U256::from(0));
        c.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm_rex4 = MegaEvm::new(ctx_rex4);
    let mut tx_rex4 = MegaTransaction::new(tx);
    tx_rex4.enveloped_tx = Some(Bytes::new());
    let r4 = alloy_evm::Evm::transact_raw(&mut evm_rex4, tx_rex4).unwrap();
    assert!(r4.result.is_success(), "REX4 should succeed: {:?}", r4.result);
    let gas_rex4 = r4.result.gas_used();

    // REX4 queries the high-capacity bucket (3x multiplier) → more gas.
    // REX5 queries the correct bucket (1x multiplier) → less gas.
    assert!(
        gas_rex4 > gas_rex5,
        "REX4 should use more gas ({gas_rex4}) than REX5 ({gas_rex5}) because \
         REX4 uses delegate's nonce for SALT lookup (hitting high-capacity bucket)"
    );
}

/// CALL with value to a genuinely empty address should STILL charge the
/// new-account premium in REX5.
///
/// This is the control case: `inspect_account` on a truly empty address correctly
/// sees it as empty and charges accordingly.
#[test]
fn test_rex5_call_to_truly_empty_address_charges_premium() {
    let empty_target = address!("0000000000000000000000000000000000819999");
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_balance(PARENT, U256::from(1_000_000_000u64));
    db.set_account_code(PARENT, build_call_with_value_contract(empty_target));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // A genuinely empty address should still incur new-account state growth.
    assert!(
        usage.state_growth > 0,
        "CALL with value to truly empty address should record state growth: {}",
        usage.state_growth
    );
}
