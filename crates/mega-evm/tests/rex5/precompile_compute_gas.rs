//! Tests for precompile error path compute gas.
//!
//! REX5 fixes compute gas accounting for failing precompile calls. When a precompile
//! fails with `PrecompileOOG` (e.g., ecrecover called with insufficient gas), the full
//! forwarded gas amount is permanently lost by the parent. REX5 records this lost gas
//! as compute gas via `gas.limit()`. Pre-REX5 specs record only `gas.spent()` which
//! is 0 on error paths because revm does not call `record_cost()` for failed precompiles.
//!
//! REX5 also refines the error-path accounting for Mega's fixed-cost KZG precompile
//! (`0x0a`). When the KZG wrapper's `gas_limit < GAS_COST` pre-check has passed and
//! upstream verification then returned a non-OOG error (invalid proof / mismatched
//! version / wrong input length), the recorded compute-gas is exactly `GAS_COST`
//! (= 100 000) instead of the full forwarded amount. The cap-induced OOG path
//! (`effective_gas_limit < GAS_COST`, wrapper itself OOGs before verification) is
//! unaffected and still records the capped budget.
//!
//! Pre-REX5 KZG verification-failure behaviour is intentionally preserved as the
//! original `spent() == 0` recording — replay determinism requires that stable specs
//! produce byte-identical receipts. The `test_rex4_kzg_*` test below is a
//! stable-spec replay-preservation guard.

use alloy_primitives::{address, Address, Bytes, U256};
use core::cell::RefCell;
use mega_evm::{
    kzg_point_evaluation,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    AdditionalLimit, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ResultAndState, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
};
use sha2::{Digest, Sha256};
use std::rc::Rc;

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const CONTRACT: Address = address!("0000000000000000000000000000000000600001");
const ECRECOVER: Address = address!("0000000000000000000000000000000000000001");
/// Blake2f precompile (EIP-152). Requires exactly 213 bytes of input.
/// Wrong-length input triggers `PrecompileError::Other` (not OOG).
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");
// KZG point evaluation precompile (EIP-4844, address `0x0a`) and its Mega-overridden
// fixed cost. These are local `const` aliases of the production constants in
// `mega_evm::kzg_point_evaluation`, kept here only because Rust's
// named-format-arg substitution (`{KZG_GAS_COST}` in `assert!` messages) requires
// the identifier to be in lexical scope. If the production constant is ever
// re-tuned (e.g., a future Mega spec changes the fixed cost), these aliases
// inherit the new value automatically; no manual sync required.
const KZG: Address = kzg_point_evaluation::ADDRESS;
const KZG_GAS_COST: u64 = kzg_point_evaluation::GAS_COST;

// ============================================================================
// HELPERS
// ============================================================================

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, u64) {
    transact_with_compute_limit(spec, db, tx, None)
}

/// Like `transact`, but optionally overrides the transaction's compute-gas budget
/// via a custom `AdditionalLimit` tracker. Used by KZG cap-induced tests that need
/// `tx_compute_gas_limit < KZG_GAS_COST` to exercise the wrapper's pre-check OOG.
fn transact_with_compute_limit(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
    tx_compute_gas_limit_override: Option<u64>,
) -> (ResultAndState<MegaHaltReason>, u64) {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    if let Some(limit) = tx_compute_gas_limit_override {
        let tx_limits = EvmTxRuntimeLimits {
            tx_compute_gas_limit: limit,
            ..EvmTxRuntimeLimits::from_spec(spec)
        };
        context.additional_limit = Rc::new(RefCell::new(AdditionalLimit::new(spec, tx_limits)));
    }
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    (r, compute_gas)
}

/// Build a KZG-precompile payload from the four 48/32-byte fields.
/// Layout: `versioned_hash[32] || z[32] || y[32] || commitment[48] || proof[48]` = 192 bytes.
/// The `versioned_hash` is the SHA-256 of `commitment` with byte 0 forced to
/// `VERSIONED_HASH_VERSION_KZG = 0x01` unless `version_byte_override` is set
/// (used by `mismatched_version` tests).
fn build_kzg_input(version_byte_override: Option<u8>) -> Vec<u8> {
    let commitment = hex::decode("8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca25f26936857bc3a7c2539ea8ec3a952b7").unwrap();
    let z =
        hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000").unwrap();
    let y =
        hex::decode("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9").unwrap();
    let proof = hex::decode("a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc2160744faf0070725e00b60ad9a026a15b1a8c").unwrap();
    let mut versioned_hash = Sha256::digest(&commitment).to_vec();
    versioned_hash[0] = version_byte_override.unwrap_or(0x01);

    let mut input = Vec::with_capacity(192);
    input.extend_from_slice(&versioned_hash);
    input.extend_from_slice(&z);
    input.extend_from_slice(&y);
    input.extend_from_slice(&commitment);
    input.extend_from_slice(&proof);
    debug_assert_eq!(input.len(), 192);
    input
}

/// Valid KZG payload (verification succeeds upstream).
fn valid_kzg_input() -> Vec<u8> {
    build_kzg_input(None)
}

/// KZG payload with the proof's last byte flipped — structurally valid (192 bytes,
/// matching versioned hash) but upstream verification returns
/// `PrecompileError::BlobVerifyKzgProofFailed`.
fn invalid_proof_kzg_input() -> Vec<u8> {
    let mut input = valid_kzg_input();
    let last = input.len() - 1;
    input[last] ^= 0x01;
    input
}

/// KZG payload with `versioned_hash[0] = 0x02` so the version-byte check fails →
/// upstream returns `PrecompileError::BlobMismatchedVersion`.
fn mismatched_version_kzg_input() -> Vec<u8> {
    build_kzg_input(Some(0x02))
}

/// KZG payload truncated to 191 bytes — upstream returns
/// `PrecompileError::BlobInvalidInputLength` (requires exactly 192 bytes).
fn wrong_length_kzg_input() -> Vec<u8> {
    let mut input = valid_kzg_input();
    input.pop();
    input
}

/// Build a wrapper-contract bytecode that stores `input` at memory offset 0, then
/// CALLs `precompile` with `(gas, value=0, argsOffset=0, argsSize=input.len(),
/// retOffset=input.len(), retSize=0)`, pops the CALL result, and STOPs.
///
/// Modelled on the existing ecrecover / blake2f tests so the outer tx always
/// succeeds (the contract swallows the precompile failure via POP).
fn kzg_wrapper_bytecode(input: &[u8], forwarded_gas: u64) -> Bytes {
    let args_size = input.len() as u64;
    BytecodeBuilder::default()
        .mstore(0, input)
        .push_number(0_u64) // retSize
        .push_number(args_size) // retOffset (place return data after the input)
        .push_number(args_size) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(KZG)
        .push_number(forwarded_gas) // gas
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

// ============================================================================
// TESTS
// ============================================================================

/// A precompile CALL that fails with `PrecompileOOG` should record the full forwarded gas
/// as compute gas in REX5, but 0 in REX4.
///
/// ecrecover (address 0x01) costs 3000 gas. Forwarding only 2999 gas triggers
/// `PrecompileOOG`. The parent permanently loses the 2999 gas (`is_ok_or_revert()` is
/// false -> no refund), but pre-REX5 specs record 0 compute gas because `spent()` = 0.
/// REX5 fixes this by recording `limit()` (= forwarded gas) on error paths.
#[test]
fn test_rex5_precompile_oog_records_forwarded_gas_as_compute_gas() {
    // ecrecover requires 3000 gas. Forward only 2999 to trigger PrecompileOOG.
    let forwarded_gas: u64 = 2_999;

    // Contract: store 128 bytes of input in memory (ecrecover expects 128 bytes),
    // then CALL ecrecover with insufficient gas.
    let code = BytecodeBuilder::default()
        // Write 128 bytes of zeros at memory offset 0 (valid-length but invalid-content
        // input for ecrecover, but irrelevant because it will OOG before processing).
        .mstore(0, [0u8; 32])
        .mstore(32, [0u8; 32])
        .mstore(64, [0u8; 32])
        .mstore(96, [0u8; 32])
        // CALL ecrecover with 128-byte input but only 2999 gas
        .push_number(32_u64) // retSize
        .push_number(128_u64) // retOffset
        .push_number(128_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ECRECOVER)
        .push_number(forwarded_gas) // gas (< 3000, triggers OOG)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result_rex5, compute_gas_rex5) = transact(MegaSpecId::REX5, &mut db, tx.clone());
    assert!(
        result_rex5.result.is_success(),
        "outer tx should succeed (precompile failure is caught by CALL)"
    );

    let (result_rex4, compute_gas_rex4) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(
        result_rex4.result.is_success(),
        "outer tx should succeed (precompile failure is caught by CALL)"
    );

    // REX5 should record more compute gas because the failing precompile's forwarded gas
    // is now counted. REX4 records 0 for the failing precompile path (spent() = 0).
    assert!(
        compute_gas_rex5 > compute_gas_rex4,
        "REX5 compute gas ({compute_gas_rex5}) should be higher than REX4 ({compute_gas_rex4}) \
         due to PrecompileOOG compute gas accounting"
    );

    // The difference should be approximately the forwarded gas amount.
    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert!(
        diff >= forwarded_gas,
        "compute gas difference ({diff}) should be at least the forwarded gas ({forwarded_gas})"
    );
}

/// A precompile CALL that fails with `PrecompileError` (not OOG) should also record
/// the full forwarded gas as compute gas in REX5.
///
/// Blake2f (0x09) requires exactly 213 bytes of input. Calling it with wrong-length
/// input triggers `PrecompileError::Other`, which is distinct from `PrecompileOOG`.
/// Both error types hit the same `!is_ok_or_revert()` branch, but this test guards
/// against future regressions that might split the handling.
#[test]
fn test_rex5_precompile_error_records_forwarded_gas_as_compute_gas() {
    let forwarded_gas: u64 = 50_000;

    // Contract: CALL blake2f with 1 byte of input (wrong length → PrecompileError).
    let code = BytecodeBuilder::default()
        // Store 1 byte at memory[0]
        .mstore(0, [0xffu8; 1])
        // CALL blake2f with 1-byte input
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(1_u64) // argsSize = 1 (invalid: blake2f requires 213)
        .push_number(31_u64) // argsOffset (last byte of the 32-byte word)
        .push_number(0_u64) // value
        .push_address(BLAKE2F)
        .push_number(forwarded_gas)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result_rex5, compute_gas_rex5) = transact(MegaSpecId::REX5, &mut db, tx.clone());
    assert!(result_rex5.result.is_success(), "outer tx should succeed");

    let (result_rex4, compute_gas_rex4) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result_rex4.result.is_success(), "outer tx should succeed");

    // Same pattern as OOG test: REX5 records forwarded gas, REX4 records 0.
    assert!(
        compute_gas_rex5 > compute_gas_rex4,
        "REX5 compute gas ({compute_gas_rex5}) should be higher than REX4 ({compute_gas_rex4}) \
         due to PrecompileError compute gas accounting"
    );

    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert!(
        diff >= forwarded_gas,
        "compute gas difference ({diff}) should be at least the forwarded gas ({forwarded_gas})"
    );
}

// ============================================================================
// KZG fixed-cost compute-gas accounting (REX5)
// ============================================================================
//
// The three `test_rex5_kzg_*_records_exact_gas_cost` tests pin the invariant
// that on REX5, when the KZG wrapper reaches past its `gas_limit < GAS_COST`
// pre-check and upstream verification then fails (invalid proof / mismatched
// version / wrong length), the recorded compute-gas equals exactly
// `KZG_GAS_COST`. The cap-induced OOG test pins that the
// wrapper-pre-check-OOG case does NOT take the fixed-cost arm.
//
// The `test_rex4_kzg_*` test is a stable-spec replay-preservation guard: REX4
// records `0` on the same path. Changing that under REX4 would break
// byte-for-byte replay of historical blocks.

/// Differential helper: run the same KZG-via-wrapper bytecode under REX4 and REX5
/// and return both `(compute_gas_rex4, compute_gas_rex5)`. Both outer txs are
/// expected to succeed (the contract swallows the precompile failure).
fn run_kzg_wrapper_under_both_specs(input: Vec<u8>, forwarded_gas: u64) -> (u64, u64) {
    let code = kzg_wrapper_bytecode(&input, forwarded_gas);
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result_rex5, compute_gas_rex5) = transact(MegaSpecId::REX5, &mut db, tx.clone());
    assert!(
        result_rex5.result.is_success(),
        "REX5 outer tx should succeed (KZG failure caught by CALL)"
    );

    let (result_rex4, compute_gas_rex4) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(
        result_rex4.result.is_success(),
        "REX4 outer tx should succeed (KZG failure caught by CALL)"
    );

    (compute_gas_rex4, compute_gas_rex5)
}

/// Verification failure with `BlobVerifyKzgProofFailed`: structurally valid
/// 192-byte input with the proof's last byte flipped. Wrapper's pre-check
/// passes, upstream verification rejects the proof. REX5 records exactly
/// `KZG_GAS_COST`; REX4 records 0.
#[test]
fn test_rex5_kzg_invalid_proof_records_exact_gas_cost() {
    let forwarded_gas: u64 = 500_000;
    let (compute_gas_rex4, compute_gas_rex5) =
        run_kzg_wrapper_under_both_specs(invalid_proof_kzg_input(), forwarded_gas);

    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert_eq!(
        diff, KZG_GAS_COST,
        "REX5 must record exactly KZG_GAS_COST ({KZG_GAS_COST}) more compute gas than REX4 \
         on a failed-proof KZG call (REX5={compute_gas_rex5}, REX4={compute_gas_rex4}). \
         A diff > KZG_GAS_COST means the fixed-cost arm fell through to `limit()`; a \
         diff < KZG_GAS_COST means the fixed-cost arm did not fire."
    );
}

/// Verification failure with `BlobMismatchedVersion`: versioned hash's first
/// byte forced to 0x02 so it disagrees with `VERSIONED_HASH_VERSION_KZG = 0x01`.
/// Wrapper's pre-check passes; upstream rejects on the version comparison
/// before the cryptographic verify runs.
#[test]
fn test_rex5_kzg_mismatched_version_records_exact_gas_cost() {
    let forwarded_gas: u64 = 500_000;
    let (compute_gas_rex4, compute_gas_rex5) =
        run_kzg_wrapper_under_both_specs(mismatched_version_kzg_input(), forwarded_gas);

    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert_eq!(
        diff, KZG_GAS_COST,
        "REX5 must record exactly KZG_GAS_COST ({KZG_GAS_COST}) more compute gas than REX4 \
         on a mismatched-version KZG call (REX5={compute_gas_rex5}, REX4={compute_gas_rex4})."
    );
}

/// Verification failure with `BlobInvalidInputLength`: 191-byte payload (one
/// byte short). Wrapper's pre-check passes; upstream rejects on the
/// `input.len() != 192` check.
#[test]
fn test_rex5_kzg_invalid_input_length_records_exact_gas_cost() {
    let forwarded_gas: u64 = 500_000;
    let (compute_gas_rex4, compute_gas_rex5) =
        run_kzg_wrapper_under_both_specs(wrong_length_kzg_input(), forwarded_gas);

    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert_eq!(
        diff, KZG_GAS_COST,
        "REX5 must record exactly KZG_GAS_COST ({KZG_GAS_COST}) more compute gas than REX4 \
         on a wrong-length KZG call (REX5={compute_gas_rex5}, REX4={compute_gas_rex4})."
    );
}

/// Cap-induced OOG: `tx_compute_gas_limit = KZG_GAS_COST - 1` so the REX5 cap
/// drives `effective_gas_limit < KZG_GAS_COST`. The wrapper's own
/// `gas_limit < GAS_COST` pre-check fires before verification runs; the
/// recorded compute-gas matches the cap, **not** `KZG_GAS_COST` — the
/// fixed-cost arm must not fire on this path.
#[test]
fn test_rex5_kzg_cap_induced_oog_still_records_capped_limit() {
    // `forwarded_gas` is deliberately set well above KZG_GAS_COST. The wrapper
    // contract pushes this onto the stack as the CALL opcode's `gas` argument,
    // but it does NOT bound the precompile here — `tx_compute_gas_limit` does,
    // via the REX5 cap inside `PrecompilesMap::run`. Setting `forwarded_gas`
    // small enough to OOG on its own would exercise a different code path.
    let forwarded_gas: u64 = 500_000;
    let tx_compute_gas_limit = KZG_GAS_COST - 1;
    let code = kzg_wrapper_bytecode(&valid_kzg_input(), forwarded_gas);

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    // The outer tx may halt (TX-level compute limit exceeded) — `transact_raw`
    // returns `ResultAndState` either way. We only care about the compute-gas
    // tracker's final value.
    let (_, compute_gas) =
        transact_with_compute_limit(MegaSpecId::REX5, &mut db, tx, Some(tx_compute_gas_limit));

    // If the fixed-cost arm had fired, the precompile alone would contribute
    // KZG_GAS_COST and total compute_gas would be >= KZG_GAS_COST. With the cap
    // engaging at GAS_COST - 1 the precompile contribution is bounded by the cap.
    assert!(
        compute_gas < KZG_GAS_COST,
        "cap-induced OOG must NOT take the fixed-cost arm (compute_gas={compute_gas}, \
         KZG_GAS_COST={KZG_GAS_COST}). If it had fired, the precompile contribution \
         alone would equal KZG_GAS_COST."
    );
    // Total compute_gas is bounded by the TX-level cap (clamped at limit on overflow).
    assert!(
        compute_gas <= tx_compute_gas_limit,
        "total compute_gas ({compute_gas}) must not exceed tx_compute_gas_limit \
         ({tx_compute_gas_limit})"
    );
}

/// Stable-spec replay-preservation guard. REX4 keeps the KZG
/// verification-failure precompile contribution at 0 compute-gas; any change
/// to this would shift `tx_compute_gas_used` on historical REX4 blocks and
/// break byte-for-byte replay.
///
/// Assertion strategy: this test cannot assert `compute_gas == 0` because the
/// wrapper contract's MSTORE / PUSH / CALL opcodes themselves consume some
/// compute-gas (~23k observed). The bound pinned here is "the precompile
/// contributes strictly less than `KZG_GAS_COST` on REX4". The differential
/// test `test_rex5_kzg_invalid_proof_records_exact_gas_cost` separately
/// asserts `REX5 - REX4 == KZG_GAS_COST`, which together with this bound pins
/// the per-spec contribution exactly.
#[test]
fn test_rex4_kzg_invalid_proof_keeps_zero_compute_gas() {
    let forwarded_gas: u64 = 500_000;
    let code = kzg_wrapper_bytecode(&invalid_proof_kzg_input(), forwarded_gas);

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result, compute_gas) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result.result.is_success(), "outer tx should succeed (KZG failure caught by CALL)");
    assert!(
        compute_gas < KZG_GAS_COST,
        "REX4 precompile contribution must remain 0. Total compute_gas \
         ({compute_gas}) >= KZG_GAS_COST ({KZG_GAS_COST}) indicates the precompile \
         alone contributed KZG_GAS_COST — a replay-determinism regression where the \
         REX5 fixed-cost arm silently activated on REX4."
    );
}
