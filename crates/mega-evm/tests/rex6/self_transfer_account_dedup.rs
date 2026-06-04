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

/// Returns `(data_size_delta, kv_delta)` between a `value=1` and a `value=0` top-level call to
/// `to`. The delta isolates the target-side account-info write.
fn value_transfer_delta(spec: MegaSpecId, to: Address) -> (u64, u64) {
    let with_value = usage_of(spec, base_db(), call_tx(to, 1));
    let zero_value = usage_of(spec, base_db(), call_tx(to, 0));
    (
        with_value.data_size.saturating_sub(zero_value.data_size),
        with_value.kv_updates.saturating_sub(zero_value.kv_updates),
    )
}

// ============================================================================
// Top-level self-transfer: A -> A
// ============================================================================

/// REX6: a top-level `A -> A` value transfer adds NO extra account-info write — A is already
/// recorded by `before_tx_start`, so the target-side write is suppressed.
#[test]
fn test_rex6_top_level_self_transfer_no_double_count() {
    let (data_delta, kv_delta) = value_transfer_delta(MegaSpecId::REX6, A);
    assert_eq!(
        data_delta, 0,
        "REX6 A->A value transfer must not add a duplicate account-info write (data_size)",
    );
    assert_eq!(
        kv_delta, 0,
        "REX6 A->A value transfer must not add a duplicate account-info write (kv_updates)",
    );
}

/// REX5 baseline: the same top-level `A -> A` transfer still double-counts (frozen).
#[test]
fn test_rex5_top_level_self_transfer_still_double_counts() {
    let (data_delta, kv_delta) = value_transfer_delta(MegaSpecId::REX5, A);
    assert_eq!(
        data_delta, ACCOUNT_INFO_WRITE_SIZE,
        "REX5 A->A value transfer keeps the frozen duplicate account-info write (data_size)",
    );
    assert_eq!(
        kv_delta, 1,
        "REX5 A->A value transfer keeps the frozen duplicate account-info write (kv_updates)",
    );
}

// ============================================================================
// Non-self transfer: A -> B (unchanged on every spec)
// ============================================================================

#[test]
fn test_rex6_non_self_transfer_counts_target() {
    let (data_delta, kv_delta) = value_transfer_delta(MegaSpecId::REX6, B);
    assert_eq!(
        data_delta, ACCOUNT_INFO_WRITE_SIZE,
        "REX6 A->B value transfer must record the target write (data_size)",
    );
    assert_eq!(kv_delta, 1, "REX6 A->B value transfer must record the target write (kv_updates)");
}

#[test]
fn test_rex5_non_self_transfer_counts_target() {
    let (data_delta, kv_delta) = value_transfer_delta(MegaSpecId::REX5, B);
    assert_eq!(
        data_delta, ACCOUNT_INFO_WRITE_SIZE,
        "REX5 A->B value transfer must record the target write (data_size)",
    );
    assert_eq!(kv_delta, 1, "REX5 A->B value transfer must record the target write (kv_updates)");
}

// ============================================================================
// Nested self-call with value: A's code CALLs A with value
// ============================================================================

/// A contract at `SELF_CALLER` that self-calls **exactly once** with `value` wei.
///
/// To bound the recursion the contract branches on `CALLDATASIZE`: the outer invocation (reached
/// from the EOA tx with 1 byte of calldata) takes the self-call branch and CALLs itself with an
/// EMPTY args buffer (`argsSize = 0`); the inner invocation therefore sees `CALLDATASIZE == 0` and
/// STOPs immediately. The result is a single `caller == target == SELF_CALLER` value frame.
fn self_calling_code(value: u64) -> Bytes {
    // Layout (byte offsets matter for the JUMPDEST target):
    //   CALLDATASIZE              ; cond
    //   PUSH1 <jumpdest>          ; dest
    //   JUMPI                     ; if calldatasize != 0 -> self-call branch
    //   STOP                      ; inner invocation (calldatasize == 0) returns here
    //   JUMPDEST                  ; <- self-call branch
    //   <CALL(self, value, argsSize=0)>
    //   POP STOP
    //
    // Prefix bytes: CALLDATASIZE(1) PUSH1(2) JUMPI(1) STOP(1) = 5 bytes, so the JUMPDEST is at
    // offset 5.
    const JUMPDEST_OFFSET: u8 = 5;
    BytecodeBuilder::default()
        .append(CALLDATASIZE)
        .push_number(JUMPDEST_OFFSET) // u8 -> single-byte PUSH1
        .append(JUMPI)
        .append(STOP)
        .append(JUMPDEST)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize (empty -> inner invocation sees CALLDATASIZE == 0)
        .push_number(0_u64) // argsOffset
        .push_number(value) // value
        .push_address(SELF_CALLER) // target == self
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

/// A top-level no-value call `EOA -> SELF_CALLER` carrying 1 byte of calldata (so the outer
/// invocation takes the self-call branch).
fn nested_tx() -> MegaTransaction {
    let mut tx = MegaTransaction {
        base: TxEnv {
            caller: EOA,
            kind: TxKind::Call(SELF_CALLER),
            gas_limit: TX_GAS_LIMIT,
            gas_price: 0,
            value: U256::ZERO,
            data: Bytes::from_static(&[0x01]),
            ..Default::default()
        },
        ..Default::default()
    };
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Reads `(data_size, kv_updates)` for a top-level no-value `EOA -> SELF_CALLER` call where the
/// contract frame CALLs itself once with `inner_value` wei. `SELF_CALLER` is pre-funded so the
/// inner value transfer always succeeds.
fn nested_usage(spec: MegaSpecId, inner_value: u64) -> (u64, u64) {
    let db = base_db()
        .account_balance(SELF_CALLER, U256::from(A_BALANCE))
        .account_code(SELF_CALLER, self_calling_code(inner_value));
    let u = usage_of(spec, db, nested_tx());
    (u.data_size, u.kv_updates)
}

/// REX6: a nested `A -> A` value call records A only once — the caller-side write covers it, the
/// target-side write is suppressed. The delta over an inner `value=0` self-call is therefore one
/// account-info write (the caller-side write), not two.
#[test]
fn test_rex6_nested_self_call_with_value_counts_once() {
    let (data_value, kv_value) = nested_usage(MegaSpecId::REX6, 1);
    let (data_zero, kv_zero) = nested_usage(MegaSpecId::REX6, 0);
    assert_eq!(
        data_value.saturating_sub(data_zero),
        ACCOUNT_INFO_WRITE_SIZE,
        "REX6 nested A->A value call must add exactly one account-info write (data_size)",
    );
    assert_eq!(
        kv_value.saturating_sub(kv_zero),
        1,
        "REX6 nested A->A value call must add exactly one account-info write (kv_updates)",
    );
}

/// REX5 baseline: the nested `A -> A` value call double-counts A — the delta over an inner
/// `value=0` self-call is two account-info writes (caller + target), frozen.
#[test]
fn test_rex5_nested_self_call_with_value_double_counts() {
    let (data_value, kv_value) = nested_usage(MegaSpecId::REX5, 1);
    let (data_zero, kv_zero) = nested_usage(MegaSpecId::REX5, 0);
    assert_eq!(
        data_value.saturating_sub(data_zero),
        2 * ACCOUNT_INFO_WRITE_SIZE,
        "REX5 nested A->A value call double-counts (caller + target) in data_size",
    );
    assert_eq!(
        kv_value.saturating_sub(kv_zero),
        2,
        "REX5 nested A->A value call double-counts (caller + target) in kv_updates",
    );
}

// ============================================================================
// Nested self-call where the parent frame already did a value transfer
// (`parent_needs_update == false` arm of the dedup predicate)
// ============================================================================

/// A contract at `SELF_CALLER` that, on its outer invocation, first CALLs `B` with `value` wei and
/// then CALLs **itself** with `value` wei. The first value-transferring call sets the frame's
/// `target_updated` flag, so the second (self) call has `parent_needs_update == false` — the
/// caller-side write is already skipped there, and the dedup predicate (`target == caller`) is what
/// suppresses the redundant target-side write under REX6. Recursion is bounded the same way as
/// [`self_calling_code`]: the self-call passes `argsSize = 0`, so the inner invocation sees
/// `CALLDATASIZE == 0` and STOPs.
fn self_calling_code_two(value: u64) -> Bytes {
    const JUMPDEST_OFFSET: u8 = 5;
    let mut b = BytecodeBuilder::default()
        .append(CALLDATASIZE)
        .push_number(JUMPDEST_OFFSET)
        .append(JUMPI)
        .append(STOP)
        .append(JUMPDEST);
    // CALL #1: SELF_CALLER -> B (distinct target), value. Sets `target_updated` on this frame.
    b = b
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(value) // value
        .push_address(B) // target != self
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP);
    // CALL #2: SELF_CALLER -> SELF_CALLER (self), value. `parent_needs_update == false` here.
    b.push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize (empty -> inner invocation STOPs)
        .push_number(0_u64) // argsOffset
        .push_number(value) // value
        .push_address(SELF_CALLER) // target == self
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

/// `(data_size, kv_updates)` for `EOA -> SELF_CALLER` where the contract frame CALLs `B` then
/// itself, each with `inner_value` wei.
fn nested_two_call_usage(spec: MegaSpecId, inner_value: u64) -> (u64, u64) {
    let db = base_db()
        .account_balance(SELF_CALLER, U256::from(A_BALANCE))
        .account_code(SELF_CALLER, self_calling_code_two(inner_value));
    let u = usage_of(spec, db, nested_tx());
    (u.data_size, u.kv_updates)
}

/// REX6: the `parent_needs_update == false` self-transfer is still deduped. The value-transfer
/// delta is two account-info writes — `SELF_CALLER` (the first call's caller write) and `B` (its
/// target) — and the second, self-targeted call adds nothing. Crucially this is not an under-count:
/// the frame account is still recorded exactly once.
#[test]
fn test_rex6_nested_pnu_false_self_transfer_counts_once() {
    let (data_value, kv_value) = nested_two_call_usage(MegaSpecId::REX6, 1);
    let (data_zero, kv_zero) = nested_two_call_usage(MegaSpecId::REX6, 0);
    assert_eq!(
        data_value.saturating_sub(data_zero),
        2 * ACCOUNT_INFO_WRITE_SIZE,
        "REX6 must record SELF_CALLER and B once each; the self-targeted second call adds nothing \
         (data_size)",
    );
    assert_eq!(
        kv_value.saturating_sub(kv_zero),
        2,
        "REX6 must record SELF_CALLER and B once each; the self-targeted second call adds nothing \
         (kv_updates)",
    );
}

/// REX5 baseline: the self-targeted second call still records `SELF_CALLER` a second time (frozen),
/// so the delta is three account-info writes.
#[test]
fn test_rex5_nested_pnu_false_self_transfer_double_counts() {
    let (data_value, kv_value) = nested_two_call_usage(MegaSpecId::REX5, 1);
    let (data_zero, kv_zero) = nested_two_call_usage(MegaSpecId::REX5, 0);
    assert_eq!(
        data_value.saturating_sub(data_zero),
        3 * ACCOUNT_INFO_WRITE_SIZE,
        "REX5 keeps the frozen duplicate SELF_CALLER target write (data_size)",
    );
    assert_eq!(
        kv_value.saturating_sub(kv_zero),
        3,
        "REX5 keeps the frozen duplicate SELF_CALLER target write (kv_updates)",
    );
}

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
