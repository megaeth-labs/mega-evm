//! REX7: the double-exceed corner, swept one gas at a time across the knife edge.
//!
//! The corner is the single opcode whose cost outruns *both* the true EVM remaining and the compute
//! headroom. The adjudication is that the compute classification wins: the transaction reports the
//! resource limit and the sender keeps the remaining gas, instead of revm's out-of-gas burning the
//! frame. The reason it can be adjudicated at all is that the two are indistinguishable at the
//! frame boundary — an out-of-gas carries no opcode cost — so there is nothing to tell them apart
//! with.
//!
//! A single case at the corner cannot show that the rule is *stable*: pick the transaction gas
//! limit one gas differently and the crossing opcode may become affordable in true gas while still
//! crossing the compute headroom. These tests calibrate the exact gas limit at which the crossing
//! opcode becomes affordable and sweep ±3 gas around it, asserting that
//!
//! - REX7 reports the same classification on every point of the sweep, and rescues the same amount
//!   at every point — the receipt does not notice the edge at all;
//! - the window really does straddle the edge, which the REX6 arm shows by flipping from a burned
//!   out-of-gas to a resource stop partway through.
//!
//! The calibration runs a probe truncated just before the crossing opcode and reads two numbers off
//! it: the compute gas recorded there (which fixes where to put the compute limit) and the
//! receipt's `gas_used` (the EVM gas spent there, which fixes the transaction gas limit at which
//! the crossing opcode is exactly affordable). The two are not the same number — `MegaETH`'s
//! intrinsic transaction gas is larger than the intrinsic compute it records — so both have to be
//! measured.

use crate::common::{
    transact, transact_default, transact_with_gas_limit, Outcome, CALLER, CONTRACT, ONE_ETH,
};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaHaltReason, MegaSpecId,
};
use revm::bytecode::opcode::{CALL, MSTORE, POP, RETURN, STOP, TIMESTAMP};

/// Memory offset the crossing MSTORE writes to. Far enough out that the expansion dominates the
/// opcode's cost, close enough that the cost stays in the hundreds of gas.
const CROSSING_OFFSET: u64 = 0x2000;

/// How far either side of the knife edge to sweep.
const SWEEP: i64 = 3;

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

/// The run leading up to the crossing MSTORE: an optional volatile access, a plain segment, and the
/// MSTORE's two stack operands. Everything here is cheap and fully paid for in every sweep point.
fn approach(volatile: bool) -> BytecodeBuilder {
    let mut builder = BytecodeBuilder::default();
    if volatile {
        builder = builder.append(TIMESTAMP).append(POP);
    }
    plain_filler(builder, 20).push_number(0u64).push_number(CROSSING_OFFSET)
}

/// Runs `db` with nothing constraining it, for calibration.
fn unconstrained_db(db: MemoryDatabase) -> Outcome {
    let outcome = transact_default(MegaSpecId::REX7, db);
    assert!(outcome.is_success(), "the calibration run must succeed: {:?}", outcome.result);
    outcome
}

/// Runs `code` against the default database with nothing constraining it, for calibration.
fn unconstrained(code: Bytes) -> Outcome {
    unconstrained_db(base_db(code))
}

/// The compute gas `code` records when neither EVM gas nor any resource limit constrains it.
fn unconstrained_compute_gas(code: Bytes) -> u64 {
    unconstrained(code).compute_gas
}

/// The calibration a sweep runs against.
struct KnifeEdge {
    /// Compute gas recorded up to (not including) the crossing opcode.
    compute_before: u64,
    /// The crossing opcode's own cost.
    cost: u64,
    /// The transaction gas limit at which the crossing opcode is exactly affordable.
    gas_limit_at_edge: u64,
}

fn calibrate(volatile: bool) -> KnifeEdge {
    let before = unconstrained(approach(volatile).append(STOP).build());
    let after = approach(volatile).append(MSTORE).append(STOP).build();
    let cost = unconstrained_compute_gas(after) - before.compute_gas;
    assert!(
        cost > 100,
        "the crossing opcode must be expensive enough to sweep around; cost={cost}"
    );
    KnifeEdge {
        compute_before: before.compute_gas,
        cost,
        gas_limit_at_edge: before.gas_used + cost,
    }
}

/// The TX-level corner: the compute headroom at the MSTORE is half its cost, so the clamp always
/// stops it, while the transaction gas limit sweeps from one gas short of affording it to two gas
/// more than enough.
#[test]
fn test_tx_level_double_exceed_classification_is_stable_across_the_knife_edge() {
    let edge = calibrate(false);
    let code = approach(false).append(MSTORE).append(STOP).build();
    // Headroom strictly between zero and the opcode's cost: the clamp is outstanding at the MSTORE
    // on every sweep point, and the MSTORE never fits inside it.
    let limit = edge.compute_before + edge.cost / 2;
    let limits = |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit);

    let mut rex7_gas_used = Vec::new();
    let mut rex6_burned = Vec::new();
    for delta in -SWEEP..=SWEEP {
        let gas_limit = (edge.gas_limit_at_edge as i64 + delta) as u64;
        let label = format!("edge{delta:+}");
        let r6 = transact_with_gas_limit(
            MegaSpecId::REX6,
            base_db(code.clone()),
            limits(MegaSpecId::REX6),
            gas_limit,
        );
        let r7 = transact_with_gas_limit(
            MegaSpecId::REX7,
            base_db(code.clone()),
            limits(MegaSpecId::REX7),
            gas_limit,
        );

        assert!(!r7.is_success(), "{label}/REX7 must stop: {:?}", r7.result);
        assert!(
            matches!(r7.halt_reason(&label), MegaHaltReason::ComputeGasLimitExceeded { .. }),
            "{label}: REX7 must classify the corner as a compute exceed on every sweep point; got \
             {:?}",
            r7.halt_reason(&label),
        );
        // The crossing opcode never ran, so its cost is not in the usage: the recorded total sits
        // at or under the limit, and within one crossing-opcode cost of it — the headroom the
        // opcode could not pay for is what the frame leaves unspent.
        assert!(
            r7.compute_gas <= limit && r7.compute_gas + edge.cost > limit,
            "{label}: REX7 must stop at the clamp boundary; compute={} limit={limit} cost={}",
            r7.compute_gas,
            edge.cost,
        );
        assert!(
            r7.gas_used < gas_limit,
            "{label}: REX7 must rescue rather than burn; gas_used={} gas_limit={gas_limit}",
            r7.gas_used
        );
        rex7_gas_used.push(r7.gas_used);

        assert!(!r6.is_success(), "{label}/REX6 must stop: {:?}", r6.result);
        rex6_burned.push(r6.gas_used == gas_limit);
    }

    let first = rex7_gas_used[0];
    assert!(
        rex7_gas_used.iter().all(|&used| used == first),
        "the rescued amount must not notice the edge; gas_used across the sweep = {rex7_gas_used:?}",
    );
    // The sweep has to actually straddle the edge, or the stability claim is vacuous: per-opcode
    // accounting burns the frame below the edge and stops on the resource limit above it.
    assert!(
        rex6_burned.contains(&true) && rex6_burned.contains(&false),
        "the sweep must straddle the knife edge; REX6 burn pattern = {rex6_burned:?}",
    );
}

/// The same sweep with gas detention as the binding constraint: the classification that has to stay
/// stable is `VolatileDataAccessOutOfGas`, which the clamp reconstructs from what bound it rather
/// than from usage having crossed the detained limit.
#[test]
fn test_detained_double_exceed_classification_is_stable_across_the_knife_edge() {
    let edge = calibrate(true);
    let code = approach(true).append(MSTORE).append(STOP).build();
    // The cap is relative to usage at the access, which happens two opcodes in. Sizing it as
    // "everything between the access and the MSTORE, plus half the MSTORE" puts the detained
    // headroom at the MSTORE at half the opcode's cost, exactly as in the TX-level case.
    let at_access = unconstrained_compute_gas(
        BytecodeBuilder::default().append(TIMESTAMP).append(STOP).build(),
    );
    let cap = edge.compute_before - at_access + edge.cost / 2;
    let limits = move |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = cap;
        limits
    };

    let mut rex7_gas_used = Vec::new();
    let mut rex6_burned = Vec::new();
    for delta in -SWEEP..=SWEEP {
        let gas_limit = (edge.gas_limit_at_edge as i64 + delta) as u64;
        let label = format!("edge{delta:+}");
        let r6 = transact_with_gas_limit(
            MegaSpecId::REX6,
            base_db(code.clone()),
            limits(MegaSpecId::REX6),
            gas_limit,
        );
        let r7 = transact_with_gas_limit(
            MegaSpecId::REX7,
            base_db(code.clone()),
            limits(MegaSpecId::REX7),
            gas_limit,
        );

        assert!(!r7.is_success(), "{label}/REX7 must stop: {:?}", r7.result);
        assert!(
            matches!(r7.halt_reason(&label), MegaHaltReason::VolatileDataAccessOutOfGas { .. }),
            "{label}: a detained corner must keep the volatile attribution on every sweep point; \
             got {:?}",
            r7.halt_reason(&label),
        );
        assert!(
            r7.gas_used < gas_limit,
            "{label}: REX7 must rescue rather than burn; gas_used={} gas_limit={gas_limit}",
            r7.gas_used
        );
        rex7_gas_used.push(r7.gas_used);

        assert!(!r6.is_success(), "{label}/REX6 must stop: {:?}", r6.result);
        rex6_burned.push(r6.gas_used == gas_limit);
    }

    let first = rex7_gas_used[0];
    assert!(
        rex7_gas_used.iter().all(|&used| used == first),
        "the rescued amount must not notice the edge; gas_used across the sweep = {rex7_gas_used:?}",
    );
    assert!(
        rex6_burned.contains(&true) && rex6_burned.contains(&false),
        "the sweep must straddle the knife edge; REX6 burn pattern = {rex6_burned:?}",
    );
}

/// The corner one frame down, where the clamp is bound frame-locally rather than TX-level.
///
/// A nested frame's compute budget is always strictly tighter than the TX-level remaining (98/100
/// of its parent's), so the clamp inside a sub-frame is always bound frame-locally — and a
/// frame-local exceed is absorbed into a revert rather than halting the transaction. With the
/// compute limit set so the child's headroom runs out inside the crossing opcode, that absorption
/// has to hold on every sweep point, including where the child's *true* forwarded gas flips from
/// too little to enough.
///
/// The control arm — the same sweep with nothing constraining compute — is what shows the window
/// straddles a real edge: there the sub-frame's outcome does flip.
#[test]
fn test_frame_local_double_exceed_classification_is_stable_across_the_knife_edge() {
    let callee_code = approach(false).append(MSTORE).append(STOP).build();
    let callee_before = approach(false).append(STOP).build();
    // Calibrating the callee as a standalone transaction gives its work up to the MSTORE once the
    // intrinsic part — which a sub-frame does not pay again — is taken back out.
    let callee_intrinsic =
        unconstrained_compute_gas(BytecodeBuilder::default().append(STOP).build());
    let callee_before_compute = unconstrained_compute_gas(callee_before);
    let before_in_frame = callee_before_compute - callee_intrinsic;
    let cost = unconstrained_compute_gas(callee_code.clone()) - callee_before_compute;
    let forwarded_at_edge = before_in_frame + cost;

    let caller = |forwarded: u64| {
        BytecodeBuilder::default()
            .push_number(0u64) // retSize
            .push_number(0u64) // retOffset
            .push_number(0u64) // argsSize
            .push_number(0u64) // argsOffset
            .push_number(0u64) // value
            .push_address(crate::common::CALLEE)
            .push_number(forwarded)
            .append(CALL)
            .push_number(0u64)
            .append(MSTORE)
            .push_number(32u64)
            .push_number(0u64)
            .append(RETURN)
            .build()
    };
    let build_db = |code: &Bytes| {
        base_db(code.clone()).account_code(crate::common::CALLEE, callee_code.clone())
    };
    let call_succeeded =
        |r: &Outcome| r.result.output().map(|o| U256::from_be_slice(o)) == Some(U256::from(1u64));

    // A compute limit half a crossing-opcode short of what the whole transaction needs when the
    // sub-frame completes: the child's own budget is what runs out, and it runs out inside the
    // MSTORE.
    let generous = caller(forwarded_at_edge + SWEEP as u64);
    let whole_tx = unconstrained_db(build_db(&generous));
    assert!(call_succeeded(&whole_tx), "the calibration run's sub-frame must complete");
    let limit = whole_tx.compute_gas - cost / 2;
    let limits = |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit);

    let mut control_outcomes = Vec::new();
    for delta in -SWEEP..=SWEEP {
        let forwarded = (forwarded_at_edge as i64 + delta) as u64;
        let label = format!("edge{delta:+}");
        let code = caller(forwarded);

        // Treatment: the child's compute headroom is what the crossing opcode cannot pay for.
        let r7 = transact(MegaSpecId::REX7, build_db(&code), limits(MegaSpecId::REX7));
        assert!(
            r7.is_success(),
            "{label}: a frame-local exceed must be absorbed into a revert, not halt the \
             transaction: {:?}",
            r7.result
        );
        assert!(
            !call_succeeded(&r7),
            "{label}: the sub-frame must report failure on every sweep point",
        );

        // Control: nothing constrains compute, so only the forwarded gas decides.
        let c6 = transact_default(MegaSpecId::REX6, build_db(&code));
        let c7 = transact_default(MegaSpecId::REX7, build_db(&code));
        for (spec, r) in [("REX6", &c6), ("REX7", &c7)] {
            assert!(
                r.is_success(),
                "{label}/{spec}: a sub-frame running out of gas must not stop the transaction: \
                 {:?}",
                r.result
            );
        }
        assert_eq!(
            call_succeeded(&c6),
            call_succeeded(&c7),
            "{label}: both models must agree on whether the sub-frame survived",
        );
        control_outcomes.push(call_succeeded(&c7));
    }
    // The window straddles the point where the forwarded gas starts covering the crossing opcode,
    // so the stability asserted above is a statement about a real edge.
    assert!(
        control_outcomes.contains(&true) && control_outcomes.contains(&false),
        "the sweep must straddle the sub-frame's knife edge; control outcomes = \
         {control_outcomes:?}",
    );
}
