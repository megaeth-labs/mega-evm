//! REX7 gas-clamp classification and the payload a clamp-induced exceed reports.
//!
//! The clamp is a lifecycle, not an amount: it is applied at a checkpoint, it binds the segment
//! that follows to one specific constraint, and it is consumed at the next checkpoint or at frame
//! exit. Whether the interpreter's true remaining happened to sit *above* the compute headroom or
//! exactly *on* it changes how much gets hidden — zero in the second case — but not whether the
//! clamp is in force. Both are the compute limit doing the stopping, and both must be reported as
//! such; only a frame whose own EVM gas runs out first is an ordinary out-of-gas.

use crate::common::{transact_default, transact_with_gas_limit, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaHaltReason, MegaSpecId,
};
use revm::bytecode::opcode::{MSTORE, POP, STOP};

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// Per-spec runtime limits with the TX compute gas limit replaced.
fn compute_limit(limit: u64) -> impl Fn(MegaSpecId) -> EvmTxRuntimeLimits {
    move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit)
}

/// `pairs` PUSH1/POP pairs — plain opcodes that record nothing of their own, five gas each.
fn plain_filler(pairs: usize) -> Vec<u8> {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder.build_vec()
}

// ---------------------------------------------------------------------------------------------
// The equal-value clamp: hidden == 0 and the clamp still binds.
// ---------------------------------------------------------------------------------------------

/// Memory offset the calibrated `MSTORE` writes at — 32 KiB, so its expansion cost is thousands of
/// gas and the knife edge is not sensitive to a one-gas miscount anywhere else.
const MSTORE_OFFSET: u64 = 0x8000;

/// The two shapes the knife-edge calibration needs: everything up to the `MSTORE`'s operands, and
/// the same thing with the `MSTORE` itself.
fn knife_edge_shapes() -> (Bytes, Bytes) {
    let operands = |mut code: Vec<u8>| {
        let mut builder = BytecodeBuilder::default();
        builder = builder.push_number(0u64).push_number(MSTORE_OFFSET);
        code.extend_from_slice(&builder.build_vec());
        code
    };
    let mut before = operands(plain_filler(20));
    before.push(STOP);
    let mut full = operands(plain_filler(20));
    full.push(MSTORE);
    full.push(STOP);
    (Bytes::from(before), Bytes::from(full))
}

/// The calibrated knife edge: the exact transaction gas limit and compute gas limit that leave the
/// crossing `MSTORE` one gas short on *both* budgets at once.
struct KnifeEdge {
    code: Bytes,
    gas_limit: u64,
    compute_limit: u64,
    /// Compute gas the transaction has recorded when the `MSTORE` is reached.
    compute_before: u64,
}

fn calibrate_knife_edge() -> KnifeEdge {
    let (before_code, full_code) = knife_edge_shapes();
    let before = transact_default(MegaSpecId::REX7, base_db(before_code));
    let full = transact_default(MegaSpecId::REX7, base_db(full_code.clone()));
    assert!(before.is_success(), "calibration run must succeed: {:?}", before.result);
    assert!(full.is_success(), "calibration run must succeed: {:?}", full.result);

    let mstore_cost = full.compute_gas - before.compute_gas;
    assert!(mstore_cost > 1, "the MSTORE must have a real expansion cost, got {mstore_cost}");
    KnifeEdge {
        code: full_code,
        // One gas short of the MSTORE on the EVM's own counter...
        gas_limit: before.gas_used + mstore_cost - 1,
        // ...and one gas short of it on the compute headroom, so the two coincide exactly and the
        // clamp hides nothing at all.
        compute_limit: before.compute_gas + mstore_cost - 1,
        compute_before: before.compute_gas,
    }
}

/// An exact-value clamp — true remaining equal to the compute headroom, nothing hidden — is still
/// the compute limit doing the stopping, and must be reported as a compute exceed rather than as
/// an ordinary EVM out-of-gas.
///
/// This is the double-exceed preference at its knife edge: the crossing opcode exhausts both
/// budgets at the same gas, and the compute classification is the one that keeps the sender's
/// remaining gas refundable.
#[test]
fn test_exact_value_clamp_is_still_a_compute_exceed() {
    let edge = calibrate_knife_edge();
    let r7 = transact_with_gas_limit(
        MegaSpecId::REX7,
        base_db(edge.code.clone()),
        compute_limit(edge.compute_limit)(MegaSpecId::REX7),
        edge.gas_limit,
    );

    match r7.halt_reason("REX7") {
        MegaHaltReason::ComputeGasLimitExceeded { limit, actual } => {
            assert_eq!(*limit, edge.compute_limit, "the reported limit is the TX compute limit");
            assert!(
                *actual <= edge.compute_limit,
                "the crossing opcode never ran, so usage cannot be past the limit; got {actual}",
            );
        }
        other => panic!(
            "an equal-value clamp must classify as a compute exceed, not an ordinary \
             out-of-gas; got {other:?}",
        ),
    }
}

/// The neighbouring points on either side of the knife edge classify the way the equal point does
/// or the way an ordinary out-of-gas does, and nothing in between.
///
/// One gas more of transaction gas puts the true remaining strictly above the headroom, so the
/// clamp hides one gas — the case that already worked. One gas more of compute limit puts the
/// headroom strictly above the true remaining, so the frame's own gas is what runs out and the
/// halt is an ordinary out-of-gas with no compute attribution.
#[test]
fn test_knife_edge_neighbours_classify_by_which_budget_binds() {
    let edge = calibrate_knife_edge();

    let hidden_one = transact_with_gas_limit(
        MegaSpecId::REX7,
        base_db(edge.code.clone()),
        compute_limit(edge.compute_limit)(MegaSpecId::REX7),
        edge.gas_limit + 1,
    );
    assert!(
        matches!(
            hidden_one.halt_reason("hidden=1"),
            MegaHaltReason::ComputeGasLimitExceeded { .. }
        ),
        "one gas above the edge the clamp hides one gas and binds; got {:?}",
        hidden_one.result
    );

    let gas_bound = transact_with_gas_limit(
        MegaSpecId::REX7,
        base_db(edge.code.clone()),
        compute_limit(edge.compute_limit + 1)(MegaSpecId::REX7),
        edge.gas_limit,
    );
    assert!(
        !matches!(
            gas_bound.halt_reason("gas-bound"),
            MegaHaltReason::ComputeGasLimitExceeded { .. }
        ),
        "one gas of headroom above the true remaining makes this the EVM's own out-of-gas; \
         got {:?}",
        gas_bound.result
    );
    assert!(
        gas_bound.compute_gas > edge.compute_before,
        "the EVM out-of-gas burns the frame's remainder, which settles as compute",
    );
}
