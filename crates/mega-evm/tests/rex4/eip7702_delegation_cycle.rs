//! Tests for EIP-7702 delegation cycle handling in `inspect_account_delegated`.
//!
//! ## Background
//!
//! `Journal::inspect_account_delegated` (host.rs) recursively follows EIP-7702 delegation
//! chains to resolve the ultimate delegate.
//! If an account self-delegates (A→A) or forms a cycle (A→B→A), the recursion never
//! terminates — there is no visited-set, depth limit, or self-referential check.
//! The resulting stack overflow kills the process with SIGABRT, which is not a catchable
//! Rust panic.
//!
//! The vulnerable function is called from `wrap_call_with_storage_gas!` (CALL, CALLCODE,
//! DELEGATECALL, STATICCALL), `create` (CREATE/CREATE2), `inspect_storage` (SLOAD/SSTORE),
//! and `StateGrowthTracker::before_frame_init`.
//!
//! Direct top-level transactions are unaffected because revm's own `load_account_delegated`
//! resolves exactly one hop.
//! The issue only manifests when a *contract* performs a CALL-family opcode targeting the
//! poisoned address.
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
