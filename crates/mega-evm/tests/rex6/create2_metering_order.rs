//! REX6 CREATE2 gas-metering order.
//!
//! REX6 folds the CREATE2 memory-expansion (`resize_memory!`) gas into the single compute-gas
//! recording taken after the inner opcode completes, instead of the pre-REX6 split recording (REX5
//! recorded it eagerly before the storage charge; pre-REX5 recorded it after the inner op).
//!
//! Coverage:
//! - **Happy path**: REX6 records the same total compute gas as REX5 (and identical `gas_used`),
//!   proving the single recording equals the REX5 eager-resize + inner sum with no double/miss
//!   count.
//! - **Missing salt**: a CREATE2 with the salt operand absent halts with `StackUnderflow` under
//!   both specs without leaking any `resize_gas` into the compute tracker.
//! - **Failure-path divergence**: under a compute-gas limit tight enough that the memory-expansion
//!   gas crosses it, REX5 halts at the eager resize record (before the inner opcode runs) while
//!   REX6 defers recording until after the inner opcode has executed — so REX6 records strictly
//!   more compute gas. This is the intended, spec-defined behavior change of the canonical order.

use crate::common::{transact, transact_default, CALLER, CONTRACT};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{CREATE2, STOP};

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// 32 KiB initcode: the wrapper-side memory expansion gas is in the multi-KiB range, large enough
/// that a double-count or miss-count of the resize gas is visible, yet under the EIP-3860 initcode
/// size limit (49,152 bytes) so the inner CREATE2 sets up a child frame rather than erroring.
const INITCODE_LEN: u64 = 32 * 1024;

/// `CREATE2(value=0, offset=0, size=INITCODE_LEN, salt=0)` then `STOP`. The zero-initialized
/// initcode region keccak-hashes to a `STOP`-only constructor that deploys empty code.
fn build_create2_contract() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // salt
        .push_number(INITCODE_LEN) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(STOP)
        .build()
}

/// The same four operand pushes as [`build_create2_contract`] followed by `STOP` (no CREATE2).
/// Running this measures the compute-gas usage accumulated *before* the CREATE2 opcode, used to
/// place a compute-gas limit precisely inside the CREATE2 memory-expansion window.
fn build_pushes_only_contract() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(INITCODE_LEN)
        .push_number(0u64)
        .push_number(0u64)
        .stop()
        .build()
}

fn db_with(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
}

#[test]
fn test_create2_happy_path_compute_gas_identical_rex5_rex6() {
    let r5 = transact_default(MegaSpecId::REX5, db_with(build_create2_contract()));
    let r6 = transact_default(MegaSpecId::REX6, db_with(build_create2_contract()));

    assert!(r5.is_success(), "REX5 CREATE2 happy path must succeed; got {:?}", r5.result);
    assert!(r6.is_success(), "REX6 CREATE2 happy path must succeed; got {:?}", r6.result);

    // The REX6 single recording (resize + inner, taken once after the body) equals the REX5 eager
    // resize record + inner record exactly — no double count, no miss count.
    assert_eq!(
        r5.compute_gas, r6.compute_gas,
        "CREATE2 happy-path compute_gas must be identical; REX5={} REX6={}",
        r5.compute_gas, r6.compute_gas
    );
    // The EVM-gas operations are byte-identical across the two orders.
    assert_eq!(
        r5.gas_used, r6.gas_used,
        "CREATE2 happy-path gas_used must be identical; REX5={} REX6={}",
        r5.gas_used, r6.gas_used
    );
}

#[test]
fn test_create2_missing_salt_consistent_rex5_rex6() {
    // Three CREATE2 operands, salt OMITTED: the wrapper's `stack.inspect::<3>()` fails, halting
    // with `StackUnderflow` before any memory expansion under both specs.
    let code = BytecodeBuilder::default()
        .push_number(INITCODE_LEN) // length (depth 2)
        .push_number(0u64) // offset (depth 1)
        .push_number(0u64) // value (depth 0)
        .append(CREATE2)
        .append(STOP)
        .build();

    let r5 = transact_default(MegaSpecId::REX5, db_with(code.clone()));
    let r6 = transact_default(MegaSpecId::REX6, db_with(code));

    let is_stack_underflow = |o: &crate::common::Outcome| {
        matches!(&o.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("StackUnderflow"))
    };
    assert!(is_stack_underflow(&r5), "REX5 must halt with StackUnderflow; got {:?}", r5.result);
    assert!(is_stack_underflow(&r6), "REX6 must halt with StackUnderflow; got {:?}", r6.result);

    // Neither spec records any resize gas on the salt-underflow halt path: both readings equal the
    // shared TX intrinsic baseline.
    assert_eq!(
        r5.compute_gas, r6.compute_gas,
        "compute_gas must be identical on the salt-underflow halt path; REX5={} REX6={}",
        r5.compute_gas, r6.compute_gas
    );
}

#[test]
fn test_create2_compute_limit_divergence_rex5_rex6() {
    // Measure the compute-gas usage accumulated before the CREATE2 opcode (TX intrinsic + the four
    // operand pushes), then place the limit just above it — inside the memory-expansion window.
    let usage_before_create2 =
        transact_default(MegaSpecId::REX6, db_with(build_pushes_only_contract())).compute_gas;
    let compute_limit = usage_before_create2 + 100;

    let limits =
        |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(compute_limit);

    let r5 =
        transact(MegaSpecId::REX5, db_with(build_create2_contract()), limits(MegaSpecId::REX5));
    let r6 =
        transact(MegaSpecId::REX6, db_with(build_create2_contract()), limits(MegaSpecId::REX6));

    // Both transactions exceed the compute-gas limit and fail.
    assert!(!r5.is_success(), "REX5 must fail under the tight compute limit; got {:?}", r5.result);
    assert!(!r6.is_success(), "REX6 must fail under the tight compute limit; got {:?}", r6.result);

    // REX5 halts at the eager resize record, before the inner CREATE2 executes: its reading covers
    // only the memory-expansion gas on top of the pre-CREATE2 usage.
    assert!(
        r5.compute_gas < usage_before_create2 + 15_000,
        "REX5 should halt at the eager resize record (memory-expansion only); \
         usage_before={usage_before_create2}, got {}",
        r5.compute_gas
    );
    // REX6 defers recording until after the inner CREATE2 has executed, so it additionally records
    // the inner opcode's compute gas (CREATE2 base cost 32,000 + initcode word cost) before the
    // limit check fires.
    assert!(
        r6.compute_gas > usage_before_create2 + 30_000,
        "REX6 should record the inner CREATE2 compute gas before the limit fires; \
         usage_before={usage_before_create2}, got {}",
        r6.compute_gas
    );
    assert!(
        r6.compute_gas > r5.compute_gas,
        "REX6 records strictly more compute gas than REX5 under the tight limit; \
         REX5={} REX6={}",
        r5.compute_gas,
        r6.compute_gas
    );
}

#[test]
fn test_create2_late_record_skipped_on_compute_limit_halt_pre_rex5() {
    // Pre-REX5 path through `storage_gas_ext::create` records the CREATE2 memory-expansion gas in
    // a trailing `if resize_gas > 0 { compute_gas!(...) }` block AFTER the post-body compute-gas
    // recording. If the compute-gas limit trips at the post-body recording, the late-record block
    // MUST be skipped — otherwise it adds `resize_gas` to the tracker after the OOG halt was
    // already set, diverging from the pre-PR behavior on a frozen spec.
    //
    // Pin to REX4 (last pre-REX5 spec) so `compute_created_address` leaves `resize_gas > 0` and
    // the late-record block has real work to skip.
    let spec = MegaSpecId::REX4;

    // Happy path: with a generous limit, the late-record runs successfully, so the final
    // tracker equals `previous + body_gas + resize_gas`.
    let happy = transact_default(spec, db_with(build_create2_contract())).compute_gas;

    // Measure the pre-CREATE2 compute-gas usage so we can position the limit precisely.
    let usage_before_create2 =
        transact_default(spec, db_with(build_pushes_only_contract())).compute_gas;

    // Position the limit just past the pre-CREATE2 usage so the limit trips inside
    // `record_storage_compute_gas!` immediately after the inner CREATE2 body completes.
    let compute_limit = usage_before_create2 + 100;
    let limits = EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(compute_limit);
    let tight = transact(spec, db_with(build_create2_contract()), limits);

    assert!(!tight.is_success(), "tight compute limit must halt; got {:?}", tight.result);

    // With the fix, `record_storage_compute_gas!` halts and returns, so the late-record block
    // does not run — `tight.compute_gas` is strictly less than `happy` by approximately the
    // memory-expansion gas of the 32 KiB initcode. Without the fix, the late-record block adds
    // `resize_gas` to the tracker after the halt, making `tight.compute_gas == happy`.
    assert!(
        tight.compute_gas < happy,
        "pre-REX5 CREATE2 late-record block must be skipped on post-body halt; \
         happy={} tight={}",
        happy,
        tight.compute_gas
    );
}
