//! REX7: where a latched non-compute limit exceed surfaces.
//!
//! Only the compute dimension is checked on the hot path. The other three — data size, KV updates,
//! state growth — are recorded at their own mutation sites, and each site latches the exceed into
//! `has_exceeded_limit` itself. The latch becomes a stop at the next position that consults it,
//! which under per-opcode accounting is the next metered opcode and under checkpoint accounting is
//! the next checkpoint.
//!
//! Every non-compute mutation site reachable from bytecode is *itself* a checkpoint (SSTORE, the
//! LOG family, SELFDESTRUCT, the CALL / CREATE family), and each of those settles its own body
//! after the mutation has run. The two surfacing rules therefore land on the same opcode, and these
//! tests pin that they do — by construction, not by coincidence:
//!
//! - the recorded compute gas is exactly what a run truncated at the mutation site records, so the
//!   plain segment *after* the site never executed;
//! - the checkpoint downstream of that segment never ran, which its absent storage write shows;
//! - the reported dimension, limit and usage are identical to per-opcode accounting.
//!
//! The oracle-hint case at the bottom covers the one non-compute mutation site that is not an
//! opcode: it records inside `frame_init`, one step past the CALL checkpoint that settled the
//! caller's segment.

use crate::common::{
    assert_outcomes_identical, transact, transact_default, transact_tx, Outcome, CALLER, CONTRACT,
    DEFAULT_TX_GAS_LIMIT, ONE_ETH,
};
use alloy_primitives::{Bytes, B256, U256};
use alloy_sol_types::{SolCall as _, SolError as _};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IOracle, LimitKind, MegaHaltReason, MegaLimitExceeded, MegaSpecId,
    TestExternalEnvs, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2,
};
use revm::{
    bytecode::opcode::{CALL, LOG1, POP, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
};

/// Slot written by the checkpoint downstream of the latching site; its absence proves the stop
/// landed at the site and not after it.
const DOWNSTREAM_SLOT: u64 = 0x31;
/// Slot written by the latching SSTORE itself.
const LATCHING_SLOT: u64 = 0x30;

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// `pairs` PUSH1/POP pairs — plain opcodes that record nothing of their own.
fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// The plain segment placed between the latching site and the checkpoint downstream of it. Long
/// enough that including it in the recorded compute gas would be unmistakable.
const GAP_PAIRS: usize = 40;

/// The dimension a failed transaction blamed, read out of whichever failure shape it produced.
///
/// A TX-level exceed halts and carries the dimension in the halt reason; a frame-local exceed is
/// absorbed into a revert carrying `MegaLimitExceeded(uint8 kind, uint64 limit)`. Both are in
/// scope here, since which one a given limit produces is not what these tests are about.
fn blamed_dimension(label: &str, outcome: &Outcome) -> LimitKind {
    match &outcome.result {
        ExecutionResult::Halt { reason, .. } => match reason {
            MegaHaltReason::DataLimitExceeded { .. } => LimitKind::DataSize,
            MegaHaltReason::KVUpdateLimitExceeded { .. } => LimitKind::KVUpdate,
            MegaHaltReason::ComputeGasLimitExceeded { .. } => LimitKind::ComputeGas,
            MegaHaltReason::StateGrowthLimitExceeded { .. } => LimitKind::StateGrowth,
            other => panic!("{label}: not a limit halt: {other:?}"),
        },
        ExecutionResult::Revert { output, .. } => {
            let decoded = MegaLimitExceeded::abi_decode(output)
                .unwrap_or_else(|e| panic!("{label}: revert data is not MegaLimitExceeded: {e}"));
            LimitKind::from_u8(decoded.kind)
                .unwrap_or_else(|| panic!("{label}: unknown limit kind {}", decoded.kind))
        }
        other => panic!("{label}: expected a limit failure, got {other:?}"),
    }
}

/// Runs `code` under both specs with `limits` and returns `(REX6, REX7)`.
fn run_both(
    code: &Bytes,
    limits: &impl Fn(MegaSpecId) -> EvmTxRuntimeLimits,
) -> (Outcome, Outcome) {
    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code.clone()), limits(MegaSpecId::REX7));
    (r6, r7)
}

/// The shared body of the three per-dimension cases.
///
/// `full` runs the latching site, a plain gap, and a downstream SSTORE. `truncated` is the same
/// program cut off immediately after the latching site. Asserting the failing run's compute gas
/// against the truncated run's is what pins the stop to the site: the gap contributes nothing.
fn assert_stops_at_the_latching_site(
    label: &str,
    full: Bytes,
    truncated: Bytes,
    expected: LimitKind,
    limits: impl Fn(MegaSpecId) -> EvmTxRuntimeLimits,
) -> (Outcome, Outcome) {
    let at_site = transact_default(MegaSpecId::REX7, base_db(truncated)).compute_gas;
    let (r6, r7) = run_both(&full, &limits);

    for (spec, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(
            !r.is_success(),
            "{label}/{spec}: the tight limit must stop the tx: {:?}",
            r.result
        );
        assert_eq!(
            blamed_dimension(&format!("{label}/{spec}"), r),
            expected,
            "{label}/{spec}: the wrong dimension was blamed",
        );
        assert!(
            r.storage_value(CONTRACT, U256::from(DOWNSTREAM_SLOT)).is_zero(),
            "{label}/{spec}: the checkpoint downstream of the gap must never run",
        );
    }
    assert_eq!(
        r7.compute_gas, at_site,
        "{label}: REX7 must stop at the latching site — the plain gap after it must contribute no \
         compute gas; stopped at {} vs {at_site} recorded up to the site",
        r7.compute_gas
    );
    assert_outcomes_identical(label, &r6, &r7);
    (r6, r7)
}

/// Data size: `on_log` records the log's topics and payload, then latches. LOG1 is a checkpoint, so
/// its own trailing settlement surfaces the latch on the spot.
#[test]
fn test_data_size_latch_stops_at_the_log_that_recorded_it() {
    let log = |builder: BytecodeBuilder| {
        builder
            .mstore(0, [0x11u8; 32])
            .push_number(0xabcu64) // topic0
            .push_number(32u64) // len
            .push_number(0u64) // offset
            .append(LOG1)
    };
    let truncated = log(plain_filler(BytecodeBuilder::default(), 10)).append(STOP).build();
    let full = plain_filler(log(plain_filler(BytecodeBuilder::default(), 10)), GAP_PAIRS)
        .sstore(U256::from(DOWNSTREAM_SLOT), U256::from(1))
        .append(STOP)
        .build();

    // One byte under what the log needs, so the log's own recording is what overflows.
    let before = transact_default(MegaSpecId::REX7, base_db(truncated.clone()));
    let intrinsic = transact_default(
        MegaSpecId::REX7,
        base_db(BytecodeBuilder::default().append(STOP).build()),
    )
    .data_size;
    assert!(
        intrinsic < before.data_size,
        "the log must be what pushes data size past the intrinsic footprint; {intrinsic} vs {}",
        before.data_size
    );
    let limit = before.data_size - 1;

    assert_stops_at_the_latching_site(
        "data size / LOG1",
        full,
        truncated,
        LimitKind::DataSize,
        move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(limit),
    );
}

/// KV updates: `on_sstore` records the storage write, then latches. SSTORE is a checkpoint, so the
/// stop lands on it.
#[test]
fn test_kv_update_latch_stops_at_the_sstore_that_recorded_it() {
    let truncated = plain_filler(BytecodeBuilder::default(), 10)
        .sstore(U256::from(LATCHING_SLOT), U256::from(0x77))
        .append(STOP)
        .build();
    let full = plain_filler(
        plain_filler(BytecodeBuilder::default(), 10)
            .sstore(U256::from(LATCHING_SLOT), U256::from(0x77)),
        GAP_PAIRS,
    )
    .sstore(U256::from(DOWNSTREAM_SLOT), U256::from(1))
    .append(STOP)
    .build();

    let before = transact_default(MegaSpecId::REX7, base_db(truncated.clone()));
    let intrinsic = transact_default(
        MegaSpecId::REX7,
        base_db(BytecodeBuilder::default().append(STOP).build()),
    )
    .kv_updates;
    assert!(
        intrinsic < before.kv_updates,
        "the store must be what pushes KV updates past the intrinsic footprint; {intrinsic} vs {}",
        before.kv_updates
    );
    let limit = before.kv_updates - 1;

    let (_, r7) = assert_stops_at_the_latching_site(
        "KV updates / SSTORE",
        full,
        truncated,
        LimitKind::KVUpdate,
        move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_kv_updates_limit(limit),
    );
    // The store's KV usage is frame-discardable, so popping the stopped frame takes it back out
    // again — the post-transaction reading is the intrinsic footprint, under the limit that the
    // store transiently crossed.
    assert!(
        r7.kv_updates <= limit,
        "the stopped frame's KV usage must be discarded; usage={} limit={limit}",
        r7.kv_updates
    );
}

/// State growth: the same SSTORE records a net-new storage slot. With only the state-growth limit
/// tightened, that is the dimension `check_limit` reports.
#[test]
fn test_state_growth_latch_stops_at_the_sstore_that_recorded_it() {
    let truncated = plain_filler(BytecodeBuilder::default(), 10)
        .sstore(U256::from(LATCHING_SLOT), U256::from(0x77))
        .append(STOP)
        .build();
    let full = plain_filler(
        plain_filler(BytecodeBuilder::default(), 10)
            .sstore(U256::from(LATCHING_SLOT), U256::from(0x77)),
        GAP_PAIRS,
    )
    .sstore(U256::from(DOWNSTREAM_SLOT), U256::from(1))
    .append(STOP)
    .build();

    let before = transact_default(MegaSpecId::REX7, base_db(truncated.clone()));
    assert!(before.state_growth > 0, "the store must create a net-new slot");
    let limit = before.state_growth - 1;

    assert_stops_at_the_latching_site(
        "state growth / SSTORE",
        full,
        truncated,
        LimitKind::StateGrowth,
        move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_state_growth_limit(limit),
    );
}

/// The one non-compute mutation site that is not an opcode: the oracle-hint interceptor meters the
/// payload inside `frame_init`, one step past the CALL checkpoint.
///
/// On overflow the interceptor deliberately synthesizes nothing and returns `None`, leaving
/// `before_frame_init` to produce the canonical TX-level halt. Under checkpoint accounting the
/// caller's segment was already settled by the CALL's own checkpoint, which runs before
/// `frame_init` — so the halt reports the same usage REX6 reports, and the caller's plain segment
/// ahead of the CALL is fully accounted for despite the frame never starting.
#[test]
fn test_oracle_hint_data_size_latch_halts_at_the_frame_boundary() {
    let payload = vec![0u8; 256];
    let calldata =
        IOracle::sendHintCall { topic: B256::repeat_byte(0x5a), data: payload.into() }.abi_encode();
    let len = calldata.len() as u64;
    let mut builder = plain_filler(BytecodeBuilder::default(), 20);
    builder = builder.mstore(0, &calldata);
    let code = builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(len) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64) // gas
        .append(CALL)
        .append(POP)
        .sstore(U256::from(DOWNSTREAM_SLOT), U256::from(1))
        .append(STOP)
        .build();
    let build_db =
        || base_db(code.clone()).account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);

    // Enough for the calldata footprint but not for the hint payload the interceptor meters on top.
    let unconstrained = transact_default(MegaSpecId::REX7, build_db());
    assert!(
        unconstrained.is_success(),
        "the unconstrained run must succeed: {:?}",
        unconstrained.result
    );
    let limit = unconstrained.data_size - len;
    let limits = move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(limit);

    let envs = TestExternalEnvs::new();
    let tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .build_fill()
    };
    let r6 = transact_tx(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6), tx(), &envs);
    let hints_after_rex6 = envs.recorded_hints().len();
    let r7 = transact_tx(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7), tx(), &envs);
    let hints_after_rex7 = envs.recorded_hints().len();

    for (label, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(!r.is_success(), "{label}: the tight data-size limit must halt: {:?}", r.result);
        assert_eq!(
            blamed_dimension(label, r),
            LimitKind::DataSize,
            "{label}: the halt must blame data size",
        );
        assert!(
            r.storage_value(CONTRACT, U256::from(DOWNSTREAM_SLOT)).is_zero(),
            "{label}: the store after the CALL must never run",
        );
    }
    assert_eq!(
        (hints_after_rex6, hints_after_rex7),
        (0, 0),
        "an over-budget hint must never reach the oracle backend under either spec",
    );
    assert_outcomes_identical("oracle hint data-size overflow", &r6, &r7);
}
