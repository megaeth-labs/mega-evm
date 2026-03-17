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
// CREATE / CREATE2 tests — `inspect_account_delegated` is called on the creator
// ---------------------------------------------------------------------------

/// Builds a contract whose code executes CREATE with a minimal initcode (STOP).
///
/// The CREATE opcode triggers `inspect_account_delegated(creator_address)` to load the
/// creator's nonce for address derivation and storage gas charging.
fn build_create_contract() -> Bytes {
    // initcode: just STOP (1 byte)
    // PUSH1 0x00 PUSH1 0x00 MSTORE8  — store STOP (0x00) at memory[0]
    // PUSH1 0x01 PUSH1 0x00 PUSH1 0x00 CREATE
    BytecodeBuilder::default()
        .append(PUSH0) // value = 0x00
        .append(PUSH0) // offset = 0
        .append(MSTORE8) // memory[0] = 0x00 (STOP opcode)
        .push_number(1u8) // size = 1
        .append(PUSH0) // offset = 0
        .append(PUSH0) // value = 0 wei
        .append(CREATE)
        .append(POP) // discard created address
        .append(STOP)
        .build()
}

/// Builds a contract whose code executes CREATE2 with a minimal initcode (STOP) and salt=0.
fn build_create2_contract() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // value = 0x00
        .append(PUSH0) // offset = 0
        .append(MSTORE8) // memory[0] = 0x00 (STOP opcode)
        .append(PUSH0) // salt = 0
        .push_number(1u8) // size = 1
        .append(PUSH0) // offset = 0
        .append(PUSH0) // value = 0 wei
        .append(CREATE2)
        .append(POP) // discard created address
        .append(STOP)
        .build()
}

/// Delegation via `CREATE`: `PARENT` calls `CYCLE_A` (delegates to `CYCLE_B`).
/// `CYCLE_B`'s code runs in `CYCLE_A`'s context and executes `CREATE`.
/// `CREATE` calls `inspect_account_delegated(CYCLE_A)` to load the creator's nonce.
/// `CYCLE_A` delegates to `CYCLE_B`, so the one-hop resolution follows to `CYCLE_B`.
/// Before the fix, this would recurse if `CYCLE_B` also delegated back.
#[test]
fn test_self_delegation_via_create() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));

    // CYCLE_A delegates to CYCLE_B, CYCLE_B has CREATE code
    set_eip7702_delegation(&mut db, CYCLE_A, CYCLE_B);
    db.set_account_balance(CYCLE_A, U256::from(1_000_000_000u64));
    db.set_account_code(CYCLE_B, build_create_contract());
    db.set_account_balance(CYCLE_B, U256::from(1_000_000_000u64));

    // PARENT calls CYCLE_A
    db.set_account_code(PARENT, build_call_contract(CYCLE_A));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.is_ok(), "CREATE with delegated creator should complete without stack overflow");
}

/// Two-address delegation cycle (A→B→A) via `CREATE2`.
///
/// Same setup as `CREATE` test but uses `CREATE2` opcode.
/// `PARENT` calls `CYCLE_A` (delegates to `CYCLE_B`). `CYCLE_B`'s code runs `CREATE2` in
/// `CYCLE_A`'s context. `CREATE2` calls `inspect_account_delegated(CYCLE_A)` for nonce/gas.
#[test]
fn test_delegation_cycle_via_create2() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));

    // CYCLE_A delegates to CYCLE_B (which has CREATE2 code).
    // CYCLE_B also delegates back to CYCLE_A to form the cycle.
    set_eip7702_delegation(&mut db, CYCLE_A, CYCLE_B);
    db.set_account_balance(CYCLE_A, U256::from(1_000_000_000u64));
    db.set_account_code(CYCLE_B, build_create2_contract());
    set_eip7702_delegation(&mut db, CYCLE_B, CYCLE_A);
    db.set_account_balance(CYCLE_B, U256::from(1_000_000_000u64));

    db.set_account_code(PARENT, build_call_contract(CYCLE_A));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(
        result.is_ok(),
        "CREATE2 with cyclic delegation should complete without stack overflow"
    );
}

// ---------------------------------------------------------------------------
// Storage isolation tests — verify `inspect_storage` uses the original address
// ---------------------------------------------------------------------------

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

/// Sets up and runs the `inspect_storage` pollution scenario for a given spec.
///
/// Setup:
/// - DELEGATOR delegates to DELEGATE via EIP-7702.
/// - DELEGATE's code does `SSTORE(slot=1, 0x77)` then `SLOAD(1)` then RETURN.
/// - DELEGATOR has storage[1] = 42, DELEGATE has storage[1] = 99.
///
/// PARENT's code does two sequential CALLs in one transaction:
///   1. CALL DELEGATOR — runs DELEGATE's code in DELEGATOR's context. The wrapped SSTORE calls
///      `inspect_storage(DELEGATOR, 1)`.
///   2. CALL DELEGATE directly — runs DELEGATE's code in its own context.
///
/// Returns the `ResultAndState` for spec-specific assertions.
fn run_inspect_storage_pollution_scenario(spec: MegaSpecId) -> ResultAndState<MegaHaltReason> {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000u64));

    // DELEGATE: code does SSTORE(1, 0x77) then SLOAD(1) then RETURN.
    // Has storage[1] = 99 which should never be confused with DELEGATOR's.
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
    db.set_account_storage(DELEGATE, U256::from(1), U256::from(99));
    db.set_account_balance(DELEGATE, U256::from(1_000_000u64));

    // DELEGATOR: delegates to DELEGATE, storage[1] = 42
    set_eip7702_delegation(&mut db, DELEGATOR, DELEGATE);
    db.set_account_storage(DELEGATOR, U256::from(1), U256::from(42));
    db.set_account_balance(DELEGATOR, U256::from(1_000_000u64));

    // PARENT: calls DELEGATOR first (triggers inspect_storage), then DELEGATE directly
    db.set_account_code(PARENT, build_two_calls_return_second(DELEGATOR, DELEGATE));
    db.set_account_balance(PARENT, U256::from(1_000_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let result = transact(spec, &mut db, tx).expect("transaction should succeed");
    assert!(result.result.is_success(), "execution should succeed: {:?}", result.result);

    let output = result.result.output().expect("should have output");
    let second_call_sload = U256::from_be_slice(output);
    // After SSTORE(1, 0x77), SLOAD(1) should return 0x77 (the just-written value)
    // regardless of spec — the pollution only affects original_value, not present_value.
    assert_eq!(
        second_call_sload,
        U256::from(0x77),
        "SLOAD after SSTORE should return the just-written value"
    );

    result
}

/// REX4+: `inspect_storage` no longer follows EIP-7702 delegation, so the delegate's
/// journal is not polluted.
/// DELEGATE's slot 1 `original_value` should be 99 (its own DB value).
#[test]
fn test_inspect_storage_does_not_pollute_delegate_journal_rex4() {
    let result = run_inspect_storage_pollution_scenario(MegaSpecId::REX4);

    let delegate_account = result.state.get(&DELEGATE).expect("DELEGATE should be in state");
    let slot =
        delegate_account.storage.get(&U256::from(1)).expect("slot 1 should exist in DELEGATE");
    assert_eq!(
        slot.original_value(),
        U256::from(99),
        "REX4: DELEGATE's slot 1 original_value should be 99 (its own), not 42 (DELEGATOR's)"
    );
}

/// Pre-REX4: `inspect_storage` follows EIP-7702 delegation (original behavior), so the
/// delegate's journal gets polluted with the delegator's storage value.
/// DELEGATE's slot 1 `original_value` will be 42 (DELEGATOR's value) instead of 99.
#[test]
fn test_inspect_storage_pollutes_delegate_journal_pre_rex4() {
    let result = run_inspect_storage_pollution_scenario(MegaSpecId::REX3);

    let delegate_account = result.state.get(&DELEGATE).expect("DELEGATE should be in state");
    let slot =
        delegate_account.storage.get(&U256::from(1)).expect("slot 1 should exist in DELEGATE");
    // Pre-REX4 preserves the old behavior: inspect_storage follows delegation and
    // pollutes the delegate's journal with the delegator's storage value.
    assert_eq!(
        slot.original_value(),
        U256::from(42),
        "Pre-REX4: DELEGATE's slot 1 original_value should be 42 (polluted from DELEGATOR)"
    );
}
