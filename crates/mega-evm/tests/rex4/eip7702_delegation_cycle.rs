//! Tests for EIP-7702 delegation handling.
//!
//! ## Delegation cycle tests
//!
//! `Journal::inspect_account_delegated` (host.rs) follows EIP-7702 delegation to resolve
//! the delegate.
//! If an account self-delegates (A→A) or forms a cycle (A→B→A), unbounded recursion would
//! cause a stack overflow (SIGABRT, not a catchable Rust panic).
//!
//! The function is called from `wrap_call_with_storage_gas!` (CALL, CALLCODE,
//! DELEGATECALL, STATICCALL), `create` (CREATE/CREATE2), and
//! `StateGrowthTracker::before_frame_init`.
//!
//! Direct top-level transactions are unaffected because revm's own `load_account_delegated`
//! resolves exactly one hop.
//! The issue only manifests when a *contract* performs a CALL-family opcode targeting the
//! poisoned address.
//!
//! ## Storage isolation tests
//!
//! EIP-7702 delegation only affects code — storage always belongs to the original address.
//! `inspect_storage` (used by the wrapped SLOAD/SSTORE for storage gas accounting) must
//! access the original address's storage, not the delegate's.
//!
//! ## EIP-7702 spec note
//!
//! EIP-7702 specifies that clients must resolve at most one level of delegation.
//! Recursive resolution is itself a spec violation, independent of the crash.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    database::AccountState,
    state::Bytecode,
};

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const PARENT: Address = address!("0000000000000000000000000000000000500001");
const SELF_DELEGATING: Address = address!("0000000000000000000000000000000000500002");
const CYCLE_A: Address = address!("0000000000000000000000000000000000500003");
const CYCLE_B: Address = address!("0000000000000000000000000000000000500004");
const DELEGATOR: Address = address!("0000000000000000000000000000000000500005");
const DELEGATE: Address = address!("0000000000000000000000000000000000500006");

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context =
        MegaContext::new(db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
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

/// Builds a contract that CALLs `target` with zero value and empty calldata.
fn build_call_contract(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // retLength
        .append(PUSH0) // retOffset
        .append(PUSH0) // argsLength
        .append(PUSH0) // argsOffset
        .append(PUSH0) // value
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(STOP)
        .build()
}

/// Builds a contract that STATICCALLs `target` with empty calldata.
fn build_staticcall_contract(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // retLength
        .append(PUSH0) // retOffset
        .append(PUSH0) // argsLength
        .append(PUSH0) // argsOffset
        .push_address(target)
        .append(GAS)
        .append(STATICCALL)
        .append(STOP)
        .build()
}

/// Self-delegation (A→A) via CALL causes unbounded recursion in `inspect_account_delegated`.
///
/// The `wrap_call_with_storage_gas!` macro calls `inspect_account_delegated` on the CALL
/// target to determine `is_empty` for storage gas charging.
/// With self-delegation, the function sees `Bytecode::Eip7702(self)` on every iteration
/// and recurses indefinitely.
#[test]
fn test_self_delegation_via_call() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));
    db.set_account_code(PARENT, build_call_contract(SELF_DELEGATING));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    set_eip7702_delegation(&mut db, SELF_DELEGATING, SELF_DELEGATING);
    db.set_account_balance(SELF_DELEGATING, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.is_ok(), "Should complete without stack overflow");
}

/// Two-address cycle (A→B→A) via CALL.
///
/// A delegates to B, B delegates back to A.
/// `inspect_account_delegated` follows A→B→A→B→... until the stack overflows.
#[test]
fn test_two_address_cycle_via_call() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));
    db.set_account_code(PARENT, build_call_contract(CYCLE_A));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    set_eip7702_delegation(&mut db, CYCLE_A, CYCLE_B);
    set_eip7702_delegation(&mut db, CYCLE_B, CYCLE_A);
    db.set_account_balance(CYCLE_A, U256::from(1_000_000u64));
    db.set_account_balance(CYCLE_B, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.is_ok(), "Should complete without stack overflow");
}

/// Self-delegation (A→A) via STATICCALL exercises the same `wrap_call_with_storage_gas!`
/// path through the STATICCALL variant.
#[test]
fn test_self_delegation_via_staticcall() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));
    db.set_account_code(PARENT, build_staticcall_contract(SELF_DELEGATING));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    set_eip7702_delegation(&mut db, SELF_DELEGATING, SELF_DELEGATING);
    db.set_account_balance(SELF_DELEGATING, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.is_ok(), "Should complete without stack overflow");
}

/// Direct transaction to a self-delegating address (control case).
///
/// Top-level transaction target resolution uses revm's `load_account_delegated`, which
/// resolves exactly one hop — not our recursive `inspect_account_delegated`.
/// This test should pass regardless of whether the bug is fixed.
#[test]
fn test_direct_tx_to_self_delegating() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));

    set_eip7702_delegation(&mut db, SELF_DELEGATING, SELF_DELEGATING);
    db.set_account_balance(SELF_DELEGATING, U256::from(1_000_000u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(SELF_DELEGATING)
        .gas_limit(100_000_000)
        .build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.is_ok(), "Direct TX should use safe single-hop resolution");
}

// ---------------------------------------------------------------------------
// Storage isolation tests — verify `inspect_storage` uses the original address
// ---------------------------------------------------------------------------

/// Builds a contract whose code does `SLOAD(0)`, stores the result at memory[0], and
/// returns it as 32-byte returndata.
fn build_sload_and_return() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // slot 0
        .append(SLOAD)
        .append(PUSH0) // mem offset 0
        .append(MSTORE)
        .push_number(32u8) // 32 bytes
        .append(PUSH0) // mem offset 0
        .append(RETURN)
        .build()
}

/// Builds a contract whose code does `SSTORE(slot=1, value=0x77)` then STOPs.
/// This triggers the wrapped SSTORE which calls `inspect_storage(target_address, 1)`.
fn build_sstore_contract() -> Bytes {
    BytecodeBuilder::default().sstore(U256::from(1), U256::from(0x77)).append(STOP).build()
}

/// Builds a contract that sequentially CALLs `first` then `second`, returning the
/// 32-byte returndata from the second call.
fn build_two_calls_return_second(first: Address, second: Address) -> Bytes {
    BytecodeBuilder::default()
        // CALL first target (no returndata needed)
        .append(PUSH0) // retLength
        .append(PUSH0) // retOffset
        .append(PUSH0) // argsLength
        .append(PUSH0) // argsOffset
        .append(PUSH0) // value
        .push_address(first)
        .append(GAS)
        .append(CALL)
        .append(POP) // discard success flag
        // CALL second target — capture 32 bytes of returndata at memory[0]
        .push_number(32u8) // retLength
        .append(PUSH0) // retOffset = 0
        .append(PUSH0) // argsLength
        .append(PUSH0) // argsOffset
        .append(PUSH0) // value
        .push_address(second)
        .append(GAS)
        .append(CALL)
        .append(POP) // discard success flag
        // Return memory[0..32]
        .push_number(32u8) // size
        .append(PUSH0) // offset
        .append(RETURN)
        .build()
}

/// `inspect_storage` must not pollute the delegate's journal entry with the delegator's
/// storage.
///
/// The bug: `inspect_storage` followed EIP-7702 delegation, so when called with address A
/// (which delegates to B), it loaded A's storage from DB but cached it in B's journal
/// account.
/// A subsequent `sload(B, slot)` would find the polluted cache and return A's value.
///
/// Setup:
/// - DELEGATOR delegates to DELEGATE via EIP-7702.
/// - DELEGATE's code does `SSTORE(slot=1, 0x77)`, triggering the wrapped SSTORE which calls
///   `inspect_storage(DELEGATOR, 1)` (because `target_address` = DELEGATOR).
/// - DELEGATOR has storage[1] = 42, DELEGATE has storage[1] = 99.
///
/// PARENT's code does two sequential CALLs in one transaction:
///   1. CALL DELEGATOR — runs DELEGATE's code in DELEGATOR's context. The wrapped SSTORE calls
///      `inspect_storage(DELEGATOR, 1)`. With the bug: loads `db.storage(DELEGATOR, 1)` = 42,
///      caches it in DELEGATE's journal account at slot 1.
///   2. CALL DELEGATE directly — runs DELEGATE's SLOAD(1) code (different code here) and returns
///      the value. With the bug: revm's `sload(DELEGATE, 1)` finds the polluted cache entry (42)
///      and returns it instead of going to DB to get 99.
///
/// We verify the second call returns 99 (DELEGATE's own storage).
#[test]
fn test_inspect_storage_does_not_pollute_delegate_journal() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));

    // DELEGATE: code does SSTORE(1, 0x77) to trigger inspect_storage via wrapped SSTORE.
    // It also has storage[1] = 99 which should never be confused with DELEGATOR's.
    db.set_account_code(DELEGATE, build_sstore_contract());
    db.set_account_storage(DELEGATE, U256::from(1), U256::from(99));
    db.set_account_balance(DELEGATE, U256::from(1_000_000u64));

    // DELEGATOR: delegates to DELEGATE, storage[1] = 42
    set_eip7702_delegation(&mut db, DELEGATOR, DELEGATE);
    db.set_account_storage(DELEGATOR, U256::from(1), U256::from(42));
    db.set_account_balance(DELEGATOR, U256::from(1_000_000u64));

    // READER: a separate contract that does SLOAD(1) and returns the value.
    // We use a different address so it has no EIP-7702 involvement.
    const READER: Address = address!("0000000000000000000000000000000000500007");
    db.set_account_code(READER, build_sload_and_return());

    // We want READER's code deployed at DELEGATE for the second call, but DELEGATE
    // already has SSTORE code. Instead, we set up a different approach:
    // PARENT calls DELEGATOR first (SSTORE triggers inspect_storage pollution),
    // then calls DELEGATE directly. For the second call, DELEGATE's code runs in
    // DELEGATE's context. But DELEGATE's code is the SSTORE contract, not SLOAD...
    //
    // Better approach: use two separate DELEGATE contracts.
    // Actually, the simplest approach: DELEGATE's code does SSTORE(1, 0x77) then
    // SLOAD(1) then RETURN. When called via DELEGATOR, the SSTORE triggers pollution
    // and SLOAD reads DELEGATOR's storage. When called directly, SLOAD should read
    // DELEGATE's storage.

    // Rebuild DELEGATE with code that does SSTORE(1, 0x77) then SLOAD(1) then RETURN
    let delegate_code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(0x77))
        .push_number(1u8) // slot 1
        .append(SLOAD)
        .append(PUSH0)
        .append(MSTORE)
        .push_number(32u8)
        .append(PUSH0)
        .append(RETURN)
        .build();
    db.set_account_code(DELEGATE, delegate_code);

    // PARENT: calls DELEGATOR first (triggers inspect_storage), then DELEGATE directly
    db.set_account_code(PARENT, build_two_calls_return_second(DELEGATOR, DELEGATE));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx).expect("transaction should succeed");
    assert!(result.result.is_success(), "execution should succeed: {:?}", result.result);

    let output = result.result.output().expect("should have output");
    let second_call_sload = U256::from_be_slice(output);

    // The second call runs DELEGATE's code in DELEGATE's own context.
    // SSTORE(1, 0x77) writes to DELEGATE's slot 1 (overwriting 99 with 0x77).
    // SLOAD(1) should then return 0x77 (the just-written value).
    //
    // With the bug: inspect_storage during the first call cached DELEGATOR's slot 1
    // (value=42) into DELEGATE's journal. When the second call's SSTORE runs,
    // inspect_storage finds the polluted entry and uses wrong original_value for gas
    // accounting. More importantly, if SLOAD happens before the journal processes
    // the SSTORE, it returns the polluted value.
    //
    // After SSTORE(1, 0x77), SLOAD(1) should return 0x77 regardless. The pollution
    // affects original_value tracking (gas refund calculations), not present_value.
    // So we check original_value in the final state instead.
    assert_eq!(
        second_call_sload,
        U256::from(0x77),
        "SLOAD after SSTORE should return the just-written value"
    );

    // The real check: verify DELEGATE's slot 1 has the correct original_value.
    // With the bug, original_value would be 42 (DELEGATOR's) instead of 99 (DELEGATE's).
    let delegate_account = result.state.get(&DELEGATE).expect("DELEGATE should be in state");
    let slot =
        delegate_account.storage.get(&U256::from(1)).expect("slot 1 should exist in DELEGATE");
    assert_eq!(
        slot.original_value(),
        U256::from(99),
        "DELEGATE's slot 1 original_value should be 99 (its own), not 42 (DELEGATOR's)"
    );
}
