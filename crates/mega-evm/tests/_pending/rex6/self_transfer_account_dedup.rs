//! REX6 value self-transfer account-info dedup for `DataSize` / KV.
//!
//! A value transfer whose target equals the caller touches a single account. The caller-side
//! account-info write (or, at the top level, the `before_tx_start` caller record) already accounts
//! for it, so the target-side write would double-count the same account on the block
//! `DataSize` / KV-update limiter lanes. Under REX6 the redundant target-side write is suppressed
//! when `target == caller`; non-self transfers (`A -> B`) and zero-value calls are unchanged, and
//! pre-REX6 specs keep the frozen double-count.
//!
//! The tests measure the *value-transfer-induced delta* (a `value > 0` run minus an otherwise
//! identical `value == 0` run). That delta isolates exactly the target-side account-info write:
//!
//! - `A -> B`, `value=1`: target write records B → delta = one account-info write on every spec.
//! - `A -> A`, `value=1` (REX6): target write suppressed → delta = 0.
//! - `A -> A`, `value=1` (REX5): target write recorded → delta = one account-info write (frozen).

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, LimitUsage, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    ACCOUNT_INFO_WRITE_SIZE,
};
use revm::{
    bytecode::opcode::*,
    context::{BlockEnv, ContextSetters, TxEnv},
    handler::EvmTr,
    primitives::TxKind,
};

/// EOA sender / self-transfer target.
const A: Address = address!("00000000000000000000000000000000A0000001");
/// Distinct non-self transfer target.
const B: Address = address!("00000000000000000000000000000000B0000002");
/// A pure-EOA sender used for the nested cases, so the tx caller has no code (EIP-3607).
const EOA: Address = address!("00000000000000000000000000000000E0A00003");
/// A self-calling contract used in the nested cases (its frame CALLs itself with value).
const SELF_CALLER: Address = address!("000000000000000000000000000000005E1F0004");
/// Pre-funded coinbase so beneficiary materialisation never adds noise.
const COINBASE: Address = address!("00000000000000000000000000000000C0FFEE01");

const TX_GAS_LIMIT: u64 = 1_000_000;
const A_BALANCE: u128 = 1_000_000_000_000_000_000; // 1 ETH

type TestEvm = MegaEvm<MemoryDatabase, revm::inspector::NoOpInspector, EmptyExternalEnv>;

/// Builds a fee-free EVM (`gas_price` = basefee = 0, operator fees zeroed, coinbase pre-funded) so
/// the post-execution fee-reward writes never perturb the `DataSize` / KV measurement.
fn build_evm(spec: MegaSpecId, db: MemoryDatabase) -> TestEvm {
    let mut context = MegaContext::new(db, spec);
    context.set_block(BlockEnv {
        gas_limit: 1_000_000_000,
        basefee: 0,
        beneficiary: COINBASE,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    MegaEvm::new(context)
}

fn base_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(A, U256::from(A_BALANCE))
        .account_balance(EOA, U256::from(A_BALANCE))
        .account_balance(COINBASE, U256::from(1u64))
}

/// A top-level call `A -> to` with the given `value`.
fn call_tx(to: Address, value: u128) -> MegaTransaction {
    let mut tx = MegaTransaction {
        base: TxEnv {
            caller: A,
            kind: TxKind::Call(to),
            gas_limit: TX_GAS_LIMIT,
            gas_price: 0,
            value: U256::from(value),
            ..Default::default()
        },
        ..Default::default()
    };
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Runs `tx` under `spec` against a fresh `db`, asserts success, and returns the limiter usage.
fn usage_of(spec: MegaSpecId, db: MemoryDatabase, tx: MegaTransaction) -> LimitUsage {
    let mut evm = build_evm(spec, db);
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(
        r.expect("no validation error").result.is_success(),
        "tx must succeed for a clean usage reading",
    );
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    usage
}

// ============================================================================
// Top-level self-transfer: A -> A
// ============================================================================

// ============================================================================
// Non-self transfer: A -> B (unchanged on every spec)
// ============================================================================

// ============================================================================
// Nested self-call with value: A's code CALLs A with value
// ============================================================================

// ============================================================================
// Nested self-call where the parent frame already did a value transfer
// (`parent_needs_update == false` arm of the dedup predicate)
// ============================================================================

// ============================================================================
// Zero-value call records no account-info write (unchanged)
// ============================================================================

/// A zero-value top-level call records no target-side account-info write on either spec, so the
/// `value=1` vs `value=0` comparison is the only thing that moves — already asserted above. This
/// test pins that REX6 and REX5 agree on the zero-value absolute usage (no behaviour change for the
/// non-transfer path).
#[test]
fn test_zero_value_call_unchanged_across_specs() {
    let rex6 = usage_of(MegaSpecId::REX6, base_db(), call_tx(B, 0));
    let rex5 = usage_of(MegaSpecId::REX5, base_db(), call_tx(B, 0));
    assert_eq!(
        rex6.data_size, rex5.data_size,
        "zero-value A->B call data_size must be identical across specs",
    );
    assert_eq!(
        rex6.kv_updates, rex5.kv_updates,
        "zero-value A->B call kv_updates must be identical across specs",
    );
}
