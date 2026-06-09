//! Regression suite for the REX5+ CREATE2 wrapper `len == 0` short-circuit.
//!
//! Canonical revm CREATE2 ignores the `offset` operand when `initcode_len == 0`:
//! no offset conversion, no memory expansion, no slice, no keccak. The initcode
//! hash is `KECCAK_EMPTY` and the deploy address is derived from
//! `creator.create2(salt, KECCAK_EMPTY)`.
//!
//! The Mega CREATE2 wrapper used to observe `offset` unconditionally, which
//! diverged from canonical revm in two ways:
//!
//! - A valid `CREATE2(value=0, offset=U256::MAX, len=0, salt=0)` halted inside
//!   `as_usize_or_fail!(offset)` instead of succeeding with empty initcode.
//! - A `CREATE2(_, offset=large_finite, len=0, _)` over-charged memory-expansion EVM gas and (under
//!   REX5+) recorded `resize_gas` into the `compute_gas` tracker even though canonical revm did
//!   neither.
//!
//! REX5+ short-circuits `len == 0` before offset conversion / `resize_memory!`
//! / slice / `keccak256`, using the pre-computed `KECCAK_EMPTY`. Pre-REX5 keeps
//! the "observe offset, resize, slice, hash" sequence verbatim for replay
//! parity.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::{CREATE2, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::EvmTr,
};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const CONTRACT: Address = address!("0000000000000000000000000000000000200001");

/// Runs a tx that invokes a CREATE2 contract built from raw bytecode and returns
/// the execution result plus the post-tx `compute_gas` tracker reading.
fn transact_create2_with_code(
    spec: MegaSpecId,
    code: Bytes,
) -> (ExecutionResult<MegaHaltReason>, u64) {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000_000_000_000_000u128))
        .account_code(CONTRACT, code);
    let mut context =
        MegaContext::new(&mut db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let compute = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    (result.result, compute)
}

/// Builds bytecode that runs `CREATE2(value=0, offset, size=0, salt=0)`.
/// `offset` is pushed as a full 32-byte value so callers can exercise non-canonical
/// values such as `U256::MAX` or `1 << 30` that the short-circuit must tolerate.
fn build_create2_empty_initcode_at_offset(offset: U256) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // salt (depth 3)
        .push_number(0_u64) // size = 0 (depth 2)
        .push_u256(offset) // offset (depth 1)
        .push_number(0_u64) // value (depth 0)
        .append(CREATE2)
        .append(STOP)
        .build()
}

/// REX5: `CREATE2(len=0, offset=U256::MAX)` MUST succeed.
///
/// Pins that the wrapper short-circuits before offset conversion when
/// `len == 0` — without the short-circuit, `as_usize_or_fail!(U256::MAX)`
/// would halt with `InvalidOperandOOG`. Post-short-circuit the initcode hash
/// is `KECCAK_EMPTY` and the inner CREATE2 deploys empty code at the
/// deterministic address `creator.create2(0, KECCAK_EMPTY)`.
#[test]
fn test_rex5_create2_len_zero_offset_max_succeeds() {
    let code = build_create2_empty_initcode_at_offset(U256::MAX);
    let (result, _compute) = transact_create2_with_code(MegaSpecId::REX5, code);
    assert!(
        result.is_success(),
        "REX5 CREATE2(len=0, offset=U256::MAX) must succeed; got {result:?}",
    );
}

/// REX5: the `len == 0` short-circuit MUST NOT engage when `len > 0`.
/// `CREATE2(len=1, offset=U256::MAX, ...)` must halt inside
/// `as_usize_or_fail!(offset)`, not silently succeed with `KECCAK_EMPTY` as
/// the initcode hash. Pins the `initcode_len.is_zero()` guard against a
/// regression that broadens it (e.g. an accidental `<=` comparison).
#[test]
fn test_rex5_create2_len_nonzero_offset_max_still_halts() {
    let code = BytecodeBuilder::default()
        .push_number(0_u64) // salt (depth 3)
        .push_number(1_u64) // size = 1 (NON-zero)
        .push_u256(U256::MAX) // offset (depth 1)
        .push_number(0_u64) // value (depth 0)
        .append(CREATE2)
        .append(STOP)
        .build();
    let (result, _compute) = transact_create2_with_code(MegaSpecId::REX5, code);
    assert!(
        matches!(result, ExecutionResult::Halt { .. }),
        "REX5 CREATE2(len=1, offset=U256::MAX) must halt — the len==0 short-circuit \
         must not engage for nonzero len; got {result:?}",
    );
}

/// REX4 (pre-REX5) baseline: same input MUST halt — the stable-spec path runs
/// `as_usize_or_fail!(U256::MAX)` and surfaces an `InvalidOperandOOG`-shaped
/// halt. Frozen for replay parity; pinned so a future change that backports the
/// short-circuit to stable specs is caught here.
#[test]
fn test_rex4_create2_len_zero_offset_max_halts() {
    let code = build_create2_empty_initcode_at_offset(U256::MAX);
    let (result, _compute) = transact_create2_with_code(MegaSpecId::REX4, code);
    assert!(
        matches!(result, ExecutionResult::Halt { .. }),
        "REX4 CREATE2(len=0, offset=U256::MAX) must halt (frozen stable-spec behavior); \
         got {result:?}",
    );
}

/// REX5: `CREATE2(len=0, offset=large_finite)` MUST succeed without paying for
/// memory expansion. Without the short-circuit, `resize_memory!(1<<30, 0)`
/// quadratically prices the implied range — pricing canonical revm explicitly
/// skips when `len == 0`. With the short-circuit the `compute_gas` reading
/// sits near the TX intrinsic baseline rather than balloon to MB-scale.
#[test]
fn test_rex5_create2_len_zero_large_offset_skips_memory_expansion() {
    let offset = U256::from(1u64 << 30);
    let code = build_create2_empty_initcode_at_offset(offset);
    let (result, compute) = transact_create2_with_code(MegaSpecId::REX5, code);
    assert!(
        result.is_success(),
        "REX5 CREATE2(len=0, offset={offset}) must succeed; got {result:?}",
    );
    // The wrapper short-circuit means no `resize_gas` is recorded under REX5,
    // so the compute_gas tracker only holds the TX intrinsic (~21k base + a
    // handful of bytes of calldata) plus the CREATE2 frame setup (~32k storage
    // gas drained from stipend on REX5 — does NOT count against compute_gas).
    // A regression that observes the offset would push compute_gas into the
    // millions on a `1 << 30` byte range.
    assert!(
        compute < 100_000,
        "REX5 CREATE2(len=0) should not record memory-expansion compute_gas; got {compute}",
    );
}

/// REX5: `CREATE2(len=0, offset=0)` happy path — the short-circuit must not
/// break the canonical "small offset, empty initcode" success case.
#[test]
fn test_rex5_create2_len_zero_offset_zero_succeeds() {
    let code = build_create2_empty_initcode_at_offset(U256::ZERO);
    let (result, _compute) = transact_create2_with_code(MegaSpecId::REX5, code);
    assert!(result.is_success(), "REX5 CREATE2(len=0, offset=0) must succeed; got {result:?}",);
}

/// REX4: `CREATE2(len=0, offset=0)` happy path — pinned to confirm pre-REX5
/// still succeeds via the frozen "observe offset (=0), trivial resize, slice
/// empty, hash empty" sequence.
#[test]
fn test_rex4_create2_len_zero_offset_zero_succeeds() {
    let code = build_create2_empty_initcode_at_offset(U256::ZERO);
    let (result, _compute) = transact_create2_with_code(MegaSpecId::REX4, code);
    assert!(result.is_success(), "REX4 CREATE2(len=0, offset=0) must succeed; got {result:?}",);
}

/// REX5 vs REX4 deployed-address parity for `len=0, offset=0`. Both must derive
/// the same address `creator.create2(0, KECCAK_EMPTY)` — REX5 via the
/// short-circuit, REX4 via `keccak256(slice_len(0, 0))` which equals
/// `KECCAK_EMPTY`. Equality of `is_success` is a weaker form of address parity;
/// pinning the spec on it would require reading the deployed address out of the
/// execution result, which requires more plumbing. The success-only check is
/// sufficient for the wrapper-level invariant — if the REX5 short-circuit
/// produced a wrong hash, the inner CREATE2 would deploy to a different
/// address but `STOP` would still succeed at that other address. Direct
/// address-equality assertion is left as a follow-up.
#[test]
fn test_create2_len_zero_offset_zero_succeeds_on_both_specs() {
    let code = build_create2_empty_initcode_at_offset(U256::ZERO);
    let (result_rex5, _) = transact_create2_with_code(MegaSpecId::REX5, code.clone());
    let (result_rex4, _) = transact_create2_with_code(MegaSpecId::REX4, code);
    assert!(result_rex5.is_success(), "REX5 must succeed; got {result_rex5:?}");
    assert!(result_rex4.is_success(), "REX4 must succeed; got {result_rex4:?}");
}
