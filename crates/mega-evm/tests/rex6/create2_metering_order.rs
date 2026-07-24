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

use std::{cell::RefCell, convert::Infallible, rc::Rc};

use crate::common::{transact, transact_default, CALLER, CONTRACT, EMPTY_TARGET};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    constants::mini_rex::MAX_INITCODE_SIZE,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BucketHasher, BucketId, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{CREATE, CREATE2, STATICCALL, STOP},
    context::{tx::TxEnvBuilder, ContextTr},
    inspector::Inspector,
    interpreter::{
        interpreter_types::InterpreterTypes, CallInputs, CallOutcome, InstructionResult,
    },
};

/// Records the `InstructionResult` an inner call to [`EMPTY_TARGET`] halted with. Used to observe
/// the halt reason of a `CREATE2` executed inside a `STATICCALL` frame, which is otherwise not
/// visible in the outer transaction result.
#[derive(Clone, Default)]
struct InnerHaltCapture(Rc<RefCell<Option<InstructionResult>>>);

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for InnerHaltCapture {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == EMPTY_TARGET {
            *self.0.borrow_mut() = Some(outcome.result.result);
        }
    }
}

/// Routes every SALT lookup to [`STATIC_TEST_BUCKET_ID`] so a raised capacity scales the
/// contract-creation storage charge inside the static callee.
#[derive(Debug, Clone, Copy)]
struct SingleBucketHasher;

impl BucketHasher for SingleBucketHasher {
    fn bucket_id(_key: &[u8]) -> BucketId {
        STATIC_TEST_BUCKET_ID
    }
}

/// Bucket every SALT lookup is routed to under [`SingleBucketHasher`].
const STATIC_TEST_BUCKET_ID: BucketId = 7;

/// 2 × `MIN_BUCKET_SIZE` (256) → a 2× multiplier, so the REX-family contract-creation charge
/// `CONTRACT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)` is 32,000 gas instead of the 0 a
/// minimum-capacity bucket yields.
const DOUBLED_BUCKET_CAPACITY: u64 = 512;

/// Runs a tx to a contract that `STATICCALL`s [`EMPTY_TARGET`] (whose code is `inner_bytecode`)
/// with `forward_gas`, and returns the halt reason the static callee produced. A
/// `bucket_capacity` of `Some(DOUBLED_BUCKET_CAPACITY)` makes the contract-creation storage
/// charge nonzero (32,000 gas); `None` leaves every bucket at minimum capacity (charge = 0).
fn static_halt_reason_for_bytecode(
    spec: MegaSpecId,
    inner_bytecode: Bytes,
    forward_gas: u64,
    bucket_capacity: Option<u64>,
) -> Option<InstructionResult> {
    let outer_staticcall = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(EMPTY_TARGET) // to
        .push_number(forward_gas) // gas
        .append(STATICCALL)
        .append(STOP)
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, outer_staticcall)
        .account_code(EMPTY_TARGET, inner_bytecode);

    let mut external_envs = TestExternalEnvs::<Infallible, SingleBucketHasher>::new();
    if let Some(capacity) = bucket_capacity {
        external_envs = external_envs.with_bucket_capacity(STATIC_TEST_BUCKET_ID, capacity);
    }

    let capture = InnerHaltCapture::default();
    let mut context = MegaContext::new(&mut db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context).with_inspector(capture.clone());
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let reason = *capture.0.borrow();
    reason
}

/// `CREATE2(value=0, offset=0, size=initcode_len, salt=0)` then `STOP`, as the static callee.
fn static_create2_bytecode(initcode_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // salt
        .push_number(initcode_len) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(STOP)
        .build()
}

/// Runs a static `CREATE2` with the given initcode length, ample forwarded gas, and
/// minimum-capacity buckets, returning its halt reason.
fn static_create2_halt_reason(spec: MegaSpecId, initcode_len: u64) -> Option<InstructionResult> {
    static_halt_reason_for_bytecode(spec, static_create2_bytecode(initcode_len), 50_000_000, None)
}

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// 32 KiB initcode: the wrapper-side memory expansion gas is in the multi-KiB range, large enough
/// that a double-count or miss-count of the resize gas is visible, yet under the configured
/// initcode size limit (548,864 bytes — see `constants::mini_rex::MAX_INITCODE_SIZE`, well above
/// the vanilla EIP-3860 default) so the inner CREATE2 sets up a child frame rather than erroring.
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

/// An oversized CREATE2 initcode must halt with `CreateInitCodeSizeLimit` under REX6
/// BEFORE any memory-expansion/copy/keccak256 prework runs, not after.
///
/// A 10 MiB initcode is chosen so the memory-expansion gas would be enormous (quadratic word
/// cost, ~210M gas — far past the 100M tx gas limit) if `resize_memory!` ran. Without the fix,
/// `resize_memory!` itself would exhaust the gas limit first and halt with `MemoryOOG` — the
/// canonical EIP-3860 check inside the inner opcode is never reached. With the fix, the halt
/// fires immediately with `CreateInitCodeSizeLimit`, before `resize_memory!` is ever called.
/// `gas_used` is NOT a useful signal here: any non-revert exceptional halt (both `MemoryOOG` and
/// `CreateInitCodeSizeLimit`) consumes the full remaining gas regardless of when it fires — this
/// matches the finding's own claim that committed gas/state is identical under REX6 either way;
/// only the halt-reason label (and the avoided node CPU work) differ.
#[test]
fn test_create2_oversized_initcode_halts_before_prework_rex6() {
    const HUGE_INITCODE_LEN: u64 = 10 * 1024 * 1024; // 10 MiB, far past the 548,864-byte cap

    let code = BytecodeBuilder::default()
        .push_number(0u64) // salt
        .push_number(HUGE_INITCODE_LEN) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(STOP)
        .build();

    let r6 = transact_default(MegaSpecId::REX6, db_with(code));

    assert!(
        matches!(&r6.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("CreateInitCodeSizeLimit")),
        "REX6 must halt with CreateInitCodeSizeLimit (not MemoryOOG) for a 10 MiB initcode — \
         the halt must fire before resize_memory! ever runs, got {:?}",
        r6.result,
    );
}

/// Pre-REX6 replay parity: the early size check is gated to REX6 only. REX5 (sealed) must keep
/// performing the resize/copy/keccak256 prework before any size check runs — no retroactive fix.
/// For the same 10 MiB initcode as the REX6 test above, REX5's `resize_memory!` itself exhausts the
/// gas limit and halts with `MemoryOOG`, never reaching the canonical EIP-3860 check at all —
/// contrasted with REX6's immediate `CreateInitCodeSizeLimit` above. This is the direct,
/// observable proof that pre-REX6 still performs the prework this finding is about.
#[test]
fn test_create2_oversized_initcode_pre_rex6_still_does_prework() {
    const HUGE_INITCODE_LEN: u64 = 10 * 1024 * 1024; // 10 MiB, far past the 548,864-byte cap

    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(HUGE_INITCODE_LEN)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE2)
        .append(STOP)
        .build();

    let r5 = transact_default(MegaSpecId::REX5, db_with(code));

    assert!(
        matches!(&r5.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("OutOfGas(Memory)")),
        "REX5 (sealed) must still attempt resize_memory! for an oversized initcode and halt \
         with an OutOfGas(Memory) halt (MemoryOOG), never reaching the canonical EIP-3860 \
         check; got {:?}",
        r5.result,
    );
}

/// Halt-reason parity for a "moderately" oversized initcode (bigger than the 548,864-byte cap,
/// but small enough that `resize_memory!`'s own gas cost stays well within the 100M tx gas
/// limit). Both specs reach `CreateInitCodeSizeLimit` — REX5 via the slower prework-then-inner-
/// check path (frozen, unpatched), REX6 via the immediate early check.
///
/// `gas_used` is deliberately NOT compared here: both specs consume the tx's full gas limit
/// regardless (any non-revert exceptional halt does), so equality there is a trivial artifact of
/// halt semantics, not a meaningful check. What genuinely differs — and is asserted below — is
/// the internal `compute_gas` tracker: REX5 still eagerly records the ~742,895-gas memory
/// expansion for this length before it ever reaches the inner opcode's check, while REX6's early
/// halt never reaches that recording at all. This divergence is pre-existing REX6 metering-order
/// behavior (`create_rex6`'s single post-body recording), not something this fix changes — it is
/// asserted here only to document that "same halt reason" does not imply "identical internal
/// accounting," and so a future change collapsing this gap doesn't silently look like a
/// regression.
#[test]
fn test_create2_moderately_oversized_initcode_same_reason_both_specs() {
    const MODERATELY_OVERSIZED_LEN: u64 = 600_000; // > 548,864 cap, resize cost is negligible

    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(MODERATELY_OVERSIZED_LEN)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE2)
        .append(STOP)
        .build();

    let r5 = transact_default(MegaSpecId::REX5, db_with(code.clone()));
    let r6 = transact_default(MegaSpecId::REX6, db_with(code));

    let is_create_initcode_size_limit = |o: &crate::common::Outcome| {
        matches!(&o.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("CreateInitCodeSizeLimit"))
    };
    assert!(
        is_create_initcode_size_limit(&r5),
        "REX5 must reach CreateInitCodeSizeLimit via the slower prework-then-inner-check path: {:?}",
        r5.result,
    );
    assert!(
        is_create_initcode_size_limit(&r6),
        "REX6 must reach CreateInitCodeSizeLimit via the immediate early check: {:?}",
        r6.result,
    );
    assert!(
        r5.compute_gas > r6.compute_gas + 500_000,
        "REX5 must still eagerly record the memory-expansion compute gas that REX6's early halt \
         skips; REX5={} REX6={}",
        r5.compute_gas,
        r6.compute_gas,
    );
}

/// Exact-boundary test: `initcode_len == max_initcode_size()` must NOT halt (the
/// comparator is strict `>`, matching revm's own canonical check), and
/// `max_initcode_size() + 1` MUST halt with `CreateInitCodeSizeLimit`. Pins the comparator
/// itself — a future `>` → `>=` regression would make the boundary length halt on REX6 while
/// still succeeding pre-REX6, a real spec divergence that the "moderately oversized" test above
/// (600,000 bytes, well clear of the boundary) cannot catch.
#[test]
fn test_create2_exact_boundary_initcode_length() {
    let at_limit = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(MAX_INITCODE_SIZE as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE2)
        .append(STOP)
        .build();
    let r6_at_limit = transact_default(MegaSpecId::REX6, db_with(at_limit));
    assert!(
        r6_at_limit.is_success(),
        "an initcode exactly at max_initcode_size must succeed (strict `>`, not `>=`): {:?}",
        r6_at_limit.result,
    );

    let over_limit = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(MAX_INITCODE_SIZE as u64 + 1)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE2)
        .append(STOP)
        .build();
    let r6_over_limit = transact_default(MegaSpecId::REX6, db_with(over_limit));
    assert!(
        matches!(&r6_over_limit.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("CreateInitCodeSizeLimit")),
        "one byte over max_initcode_size must halt with CreateInitCodeSizeLimit: {:?}",
        r6_over_limit.result,
    );
}

/// Operand-order test: an oversized `initcode_len` combined with an `initcode_offset`
/// that does not fit in a `usize` must still halt with `CreateInitCodeSizeLimit` on REX6 — the
/// size check runs before the offset is ever converted.
///
/// The wrapper converts `initcode_len` first, then runs the REX6 size check, then converts
/// `initcode_offset`. If the conversion order were the reverse (offset first, as it was before
/// this fix), the unrepresentable offset would halt with `InvalidOperandOOG`
/// (`OutOfGas(InvalidOperand)`) and mask the intended EIP-3860 halt. Pre-REX6 has no early size
/// check, so it correctly halts on the offset overflow — the same `InvalidOperandOOG` reason both
/// operands share, which is why reordering their conversion is inert on sealed specs.
#[test]
fn test_create2_oversized_len_unrepresentable_offset_halts_initcode_limit_rex6() {
    // `len` is oversized but representable as `usize`; `offset` is > `usize::MAX`.
    let code = BytecodeBuilder::default()
        .push_number(0u64) // salt
        .push_number(MAX_INITCODE_SIZE as u64 + 1) // size: oversized, but fits usize
        .push_u256(U256::from(u128::MAX)) // offset: > usize::MAX (unrepresentable)
        .push_number(0u64) // value
        .append(CREATE2)
        .append(STOP)
        .build();

    let r6 = transact_default(MegaSpecId::REX6, db_with(code.clone()));
    assert!(
        matches!(&r6.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("CreateInitCodeSizeLimit")),
        "REX6 must halt with CreateInitCodeSizeLimit even when the offset is unrepresentable — \
         the size check must run before the offset is converted; got {:?}",
        r6.result,
    );

    // Pre-REX6 (sealed) has no early size check, so the unrepresentable offset is what halts, with
    // OutOfGas(InvalidOperand). It must NOT surface CreateInitCodeSizeLimit — this pins that the
    // reorder did not change sealed-spec behavior.
    let r5 = transact_default(MegaSpecId::REX5, db_with(code));
    assert!(
        matches!(&r5.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("InvalidOperand")),
        "REX5 (sealed) must halt on the offset overflow (OutOfGas(InvalidOperand)), not the REX6 \
         size check; got {:?}",
        r5.result,
    );
}

/// Static-call precedence: a `CREATE2` inside a `STATICCALL` with oversized initcode must report
/// the static-context rejection (`StateChangeDuringStaticCall`), NOT `CreateInitCodeSizeLimit`.
/// REX6 halts with `StateChangeDuringStaticCall` at the top of the REX6 create handler, before
/// any operand read, the size check, or any resize/copy/keccak256 prework runs. REX5 (sealed) has
/// no such hoist, so it reaches the same halt reason only after running that prework and falling
/// through to the inner opcode's own static guard. A "moderately" oversized length (over the cap,
/// but cheap to resize) is used so REX5's prework-then-inner-guard path stays affordable rather
/// than running out of gas on the resize.
#[test]
fn test_create2_static_oversized_initcode_reports_static_rejection() {
    const MODERATELY_OVERSIZED_LEN: u64 = 600_000; // > 548,864 cap, resize cost negligible vs. gas

    let is_static_reject = |r: Option<InstructionResult>| {
        matches!(r, Some(InstructionResult::StateChangeDuringStaticCall))
    };

    let r6 = static_create2_halt_reason(MegaSpecId::REX6, MODERATELY_OVERSIZED_LEN);
    let r5 = static_create2_halt_reason(MegaSpecId::REX5, MODERATELY_OVERSIZED_LEN);

    assert!(
        is_static_reject(r6),
        "REX6 static oversized CREATE2 must halt with StateChangeDuringStaticCall, not \
         CreateInitCodeSizeLimit; got {r6:?}",
    );
    assert!(
        is_static_reject(r5),
        "REX5 (sealed) static oversized CREATE2 halts with StateChangeDuringStaticCall; got {r5:?}",
    );
    assert_eq!(r6, r5, "REX6 must match sealed REX5 on the static-CREATE2 halt reason");
}

/// Static-call precedence at an unaffordable initcode length: a static `CREATE2` whose initcode
/// length would make `resize_memory!` prohibitively expensive must still halt with
/// `StateChangeDuringStaticCall` on REX6 — the hoisted guard fires before any operand conversion
/// or memory work is attempted, so the halt reason does not depend on whether the resize could
/// have been afforded. REX5 (sealed) has no such hoist: it attempts the resize, cannot afford it
/// out of the gas forwarded to the inner frame, and halts with a memory out-of-gas reason instead
/// — mirroring the non-static divergence pinned above, now shown to hold inside a static frame.
#[test]
fn test_create2_static_hugely_oversized_initcode_halt_reason() {
    const HUGE_INITCODE_LEN: u64 = 10 * 1024 * 1024; // 10 MiB, resize cost far exceeds forwarded gas

    let r6 = static_create2_halt_reason(MegaSpecId::REX6, HUGE_INITCODE_LEN);
    assert_eq!(
        r6,
        Some(InstructionResult::StateChangeDuringStaticCall),
        "REX6 static CREATE2 with a hugely oversized initcode must halt with \
         StateChangeDuringStaticCall before resize_memory! is ever attempted; got {r6:?}",
    );

    let r5 = static_create2_halt_reason(MegaSpecId::REX5, HUGE_INITCODE_LEN);
    assert_eq!(
        r5,
        Some(InstructionResult::MemoryOOG),
        "REX5 (sealed) static CREATE2 must still attempt resize_memory! for a hugely oversized \
         initcode and halt with a memory out-of-gas reason, never reaching the static-call \
         rejection; got {r5:?}",
    );
}

/// Zero-length boundary: a static `CREATE2` with `initcode_len == 0` reports the static rejection
/// on both specs. REX6 halts at the top of the create handler, before the zero-length shortcut
/// runs; REX5 (sealed) takes the shortcut, derives the address, attempts the contract-creation
/// storage charge, and only then reaches the inner opcode's static guard — same reason, different
/// path. The low-gas variant below distinguishes the two paths observably.
#[test]
fn test_create2_static_zero_length_initcode_reports_static_rejection() {
    let is_static_reject = |r: Option<InstructionResult>| {
        matches!(r, Some(InstructionResult::StateChangeDuringStaticCall))
    };

    let r6 = static_create2_halt_reason(MegaSpecId::REX6, 0);
    let r5 = static_create2_halt_reason(MegaSpecId::REX5, 0);

    assert!(
        is_static_reject(r6),
        "REX6 static zero-length CREATE2 must halt with StateChangeDuringStaticCall via the \
         inner opcode's static guard; got {r6:?}",
    );
    assert!(
        is_static_reject(r5),
        "REX5 (sealed) static zero-length CREATE2 halts with StateChangeDuringStaticCall; \
         got {r5:?}",
    );
    assert_eq!(r6, r5, "REX6 must match sealed REX5 on the static zero-length CREATE2 halt reason");
}

/// Observable split on the zero-length path: with the contract-creation storage charge made
/// nonzero (2× bucket capacity → 32,000 gas) and forwarded gas below it, REX5 (sealed) attempts
/// the charge on its way to the inner opcode and halts out-of-gas — while REX6 halts
/// `StateChangeDuringStaticCall` at the top of the create handler, before the address is derived
/// or the charge is attempted. Pins that the static rejection covers the zero-length shortcut
/// path.
#[test]
fn test_create2_static_zero_length_low_gas_halt_reason() {
    let r6 = static_halt_reason_for_bytecode(
        MegaSpecId::REX6,
        static_create2_bytecode(0),
        10_000,
        Some(DOUBLED_BUCKET_CAPACITY),
    );
    assert_eq!(
        r6,
        Some(InstructionResult::StateChangeDuringStaticCall),
        "REX6 static zero-length CREATE2 must halt before the contract-creation storage charge; \
         got {r6:?}",
    );

    let r5 = static_halt_reason_for_bytecode(
        MegaSpecId::REX5,
        static_create2_bytecode(0),
        10_000,
        Some(DOUBLED_BUCKET_CAPACITY),
    );
    assert_eq!(
        r5,
        Some(InstructionResult::OutOfGas),
        "REX5 (sealed) static zero-length CREATE2 must still attempt the contract-creation \
         storage charge first and halt out-of-gas when it cannot afford it; got {r5:?}",
    );
}

/// The static rejection covers plain `CREATE`, not just `CREATE2`: same low-gas setup as the
/// zero-length test but with a `CREATE(value=0, offset=0, size=0)` callee. REX6 halts
/// `StateChangeDuringStaticCall` before the contract-creation storage charge; REX5 (sealed)
/// derives the address from the creator nonce, attempts the charge, and halts out-of-gas.
#[test]
fn test_create_static_low_gas_halt_reason() {
    let create_bytecode = BytecodeBuilder::default()
        .push_number(0u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(STOP)
        .build();

    let r6 = static_halt_reason_for_bytecode(
        MegaSpecId::REX6,
        create_bytecode.clone(),
        10_000,
        Some(DOUBLED_BUCKET_CAPACITY),
    );
    assert_eq!(
        r6,
        Some(InstructionResult::StateChangeDuringStaticCall),
        "REX6 static CREATE must halt before the contract-creation storage charge; got {r6:?}",
    );

    let r5 = static_halt_reason_for_bytecode(
        MegaSpecId::REX5,
        create_bytecode,
        10_000,
        Some(DOUBLED_BUCKET_CAPACITY),
    );
    assert_eq!(
        r5,
        Some(InstructionResult::OutOfGas),
        "REX5 (sealed) static CREATE must still attempt the contract-creation storage charge \
         first and halt out-of-gas when it cannot afford it; got {r5:?}",
    );
}

/// The static rejection precedes operand inspection: a bare `CREATE2` on an empty stack halts
/// `StackUnderflow` on REX5 (sealed — operand inspection runs first) but
/// `StateChangeDuringStaticCall` on REX6, matching canonical revm, whose
/// `require_non_staticcall!` runs before the operand pops.
#[test]
fn test_create2_static_missing_operands_halt_reason() {
    let bare_create2 = BytecodeBuilder::default().append(CREATE2).append(STOP).build();

    let r6 =
        static_halt_reason_for_bytecode(MegaSpecId::REX6, bare_create2.clone(), 50_000_000, None);
    assert_eq!(
        r6,
        Some(InstructionResult::StateChangeDuringStaticCall),
        "REX6 static CREATE2 with missing operands must halt with the static rejection, not \
         StackUnderflow; got {r6:?}",
    );

    let r5 = static_halt_reason_for_bytecode(MegaSpecId::REX5, bare_create2, 50_000_000, None);
    assert_eq!(
        r5,
        Some(InstructionResult::StackUnderflow),
        "REX5 (sealed) static CREATE2 with missing operands halts with StackUnderflow; got {r5:?}",
    );
}

// Not covered: static + `initcode_len` unrepresentable in `usize`. `static_create2_halt_reason`
// takes a `u64` length, which always fits in `usize` on the 64-bit targets the tests run on, so
// the case cannot be expressed without a raw-`U256` variant of the shared helper.
