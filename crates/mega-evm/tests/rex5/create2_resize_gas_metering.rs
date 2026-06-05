//! Regression suite for the REX5+ CREATE2 wrapper `resize_gas` recording rule.
//!
//! REX5+ validates the salt operand before `resize_memory!` / copy / keccak,
//! and records the memory-expansion gas (`resize_gas`) into the `compute_gas`
//! tracker immediately after `resize_memory!`. Pre-REX5 keeps the original
//! "resize first, salt last, record after inner CREATE2 returns" order
//! verbatim for replay parity.
//!
//! Coverage:
//! - Happy-path CREATE2 with measurable memory expansion succeeds under both specs.
//! - Cross-spec parity: the post-tx `compute_gas` tracker agrees within a small tolerance, pinning
//!   that REX5 records `resize_gas` exactly once (no double-count from the trailing late-record
//!   block, no miss-count from the early-record path being skipped).
//! - Stack-underflow consistency: a CREATE2 dispatched with only three operands halts with
//!   `StackUnderflow` under both specs and leaks no `resize_gas` into the tracker.
//!
//! Failure-path verification (storage-gas OOG / inner CREATE2 halt under REX5
//! retaining `resize_gas`, pre-REX5 skipping via the trailing block) is
//! delicate to engineer with stable gas tuning and is left to a follow-up.

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

/// Builds bytecode that runs `CREATE2(value=0, offset=0, size=initcode_len, salt=0)`.
/// The initcode region is zero-initialized; the canonical CREATE2 keccak256-hashes
/// the bytes and runs the resulting `STOP`-only constructor, deploying empty code.
/// Memory expansion gas inside the wrapper is a function of `initcode_len`.
fn build_create2_contract(initcode_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // salt
        .push_number(initcode_len) // size
        .push_number(0_u64) // offset
        .push_number(0_u64) // value
        .append(CREATE2)
        .append(STOP)
        .build()
}

/// Runs a tx that invokes the CREATE2 contract and returns the execution result and
/// the post-tx `compute_gas` tracker reading.
fn transact_create2(spec: MegaSpecId, initcode_len: u64) -> (ExecutionResult<MegaHaltReason>, u64) {
    let code = build_create2_contract(initcode_len);
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

/// 32 KiB initcode: memory expansion gas inside the wrapper is in the ~5 K range,
/// large enough that a double-count or miss-count of `resize_gas` would be
/// visible against the cross-spec parity tolerance.
const INITCODE_LEN: u64 = 32 * 1024;

/// REX5 happy path: CREATE2 with non-trivial memory expansion succeeds. The
/// `compute_gas` tracker registers usage that includes the `resize_gas` recorded
/// in the early-record path.
#[test]
fn test_rex5_create2_with_non_trivial_resize_succeeds() {
    let (result, compute) = transact_create2(MegaSpecId::REX5, INITCODE_LEN);
    assert!(result.is_success(), "REX5 CREATE2 happy path must succeed; got {result:?}");
    assert!(
        compute > 0,
        "REX5 compute_gas tracker must register usage including resize_gas; got {compute}",
    );
}

/// REX4 baseline: same scenario under the pre-REX5 spec succeeds via the trailing
/// late-record path. Pinned to guarantee replay parity is preserved.
#[test]
fn test_rex4_create2_with_non_trivial_resize_succeeds() {
    let (result, compute) = transact_create2(MegaSpecId::REX4, INITCODE_LEN);
    assert!(result.is_success(), "REX4 CREATE2 happy path must succeed; got {result:?}");
    assert!(
        compute > 0,
        "REX4 compute_gas tracker must register usage including resize_gas; got {compute}",
    );
}

/// Cross-spec parity: REX5's early-record path and pre-REX5's late-record path must
/// register the same `resize_gas` total on a successful CREATE2. A multi-KiB delta
/// would indicate either a double-count under REX5 (early + trailing both recording
/// the same `resize_gas`) or a miss-count (early-record path skipped without the
/// trailing block taking over).
#[test]
fn test_create2_compute_gas_parity_across_specs() {
    let (result_rex5, compute_rex5) = transact_create2(MegaSpecId::REX5, INITCODE_LEN);
    let (result_rex4, compute_rex4) = transact_create2(MegaSpecId::REX4, INITCODE_LEN);
    assert!(result_rex5.is_success(), "REX5 must succeed; got {result_rex5:?}");
    assert!(result_rex4.is_success(), "REX4 must succeed; got {result_rex4:?}");

    let diff = compute_rex5.abs_diff(compute_rex4);
    assert!(
        diff < 1_000,
        "REX5 ({compute_rex5}) vs REX4 ({compute_rex4}) compute_gas must agree within 1K gas — diff = {diff}",
    );
}

/// Runs a CREATE2 contract built from raw bytecode (used to construct stack-underflow
/// scenarios that `build_create2_contract` cannot express). Mirrors `transact_create2`.
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

/// CREATE2 dispatched with only three stack operands (value, offset, length; salt
/// absent) must halt with `StackUnderflow` under both specs, and neither spec may
/// leak `resize_gas` into the `compute_gas` tracker on the halt path:
///
/// - REX5: the wrapper inspects salt at position 3 BEFORE `resize_memory!`, so the early-record
///   block is unreachable when salt is missing.
/// - REX4: the wrapper inspects salt AFTER `resize_memory!`, but the trailing `if resize_gas > 0 {
///   ... }` block is skipped on the `StackUnderflow` halt.
///
/// Equality of the post-tx `compute_gas` reading across REX4 and REX5 pins this
/// invariant — a regression that records `resize_gas` on either halt path would
/// produce a spec-asymmetric reading.
#[test]
fn test_create2_missing_salt_halts_consistently_across_specs() {
    // Three CREATE2 args, salt OMITTED. Stack order from top (depth 0) at the
    // CREATE2 instruction:
    //   depth 0: value
    //   depth 1: offset
    //   depth 2: length
    //   depth 3: <empty> → wrapper's `stack.inspect::<3>()` fails → StackUnderflow.
    let code = BytecodeBuilder::default()
        .push_number(INITCODE_LEN) // length (depth 2)
        .push_number(0_u64) // offset (depth 1)
        .push_number(0_u64) // value (depth 0)
        .append(CREATE2)
        .append(STOP)
        .build();

    let (result_rex5, compute_rex5) = transact_create2_with_code(MegaSpecId::REX5, code.clone());
    let (result_rex4, compute_rex4) = transact_create2_with_code(MegaSpecId::REX4, code);

    // Both specs must halt with StackUnderflow. (Compare via Debug to avoid pinning the
    // exact OpHaltReason → EthHaltReason wrapper path.)
    let is_stack_underflow = |r: &ExecutionResult<MegaHaltReason>| -> bool {
        matches!(r, ExecutionResult::Halt { reason, .. } if format!("{reason:?}").contains("StackUnderflow"))
    };
    assert!(
        is_stack_underflow(&result_rex5),
        "REX5 must halt with StackUnderflow; got {result_rex5:?}",
    );
    assert!(
        is_stack_underflow(&result_rex4),
        "REX4 must halt with StackUnderflow; got {result_rex4:?}",
    );

    // Neither spec leaks `resize_gas` into the compute_gas tracker on the
    // stack-underflow path. The expected baseline for both specs is the TX
    // intrinsic recorded by `validate()` (`record_compute_gas(initial_gas)`) and
    // nothing more:
    //
    // - REX5: salt inspect at position 3 halts BEFORE `resize_memory!`, so the early-record block
    //   (`if is_rex5_enabled && resize_gas > 0 { ... }`) is never reached.
    // - REX4: `resize_memory!` runs and burns EVM gas, but the late-record block at the wrapper's
    //   tail is skipped when the frame halts on the position-3 salt inspect.
    //
    // Equality between the two specs pins this consistency: any regression that
    // records `resize_gas` on a halted CREATE2 (whether by moving the REX5 early
    // record ahead of the salt check, or by running the REX4 late-record despite
    // the halt) would produce a spec-asymmetric compute_gas reading exceeding the
    // shared intrinsic baseline.
    assert_eq!(
        compute_rex5, compute_rex4,
        "compute_gas must be identical across REX4 ({compute_rex4}) and REX5 ({compute_rex5}) \
         when CREATE2 halts on salt underflow — any divergence indicates resize_gas leaked into \
         the tracker on the halt path",
    );
    // Sanity check that what's recorded is just the TX intrinsic (≈21k base + ~9 calldata
    // bytes for the CREATE2 contract code). A multi-KiB reading would indicate that the
    // wrapper recorded `resize_gas` despite the halt.
    assert!(
        compute_rex5 < 25_000,
        "compute_gas reading should be near the TX intrinsic (~21k) on the halt path, \
         not include the {INITCODE_LEN}-byte resize_gas; got {compute_rex5}",
    );
}
