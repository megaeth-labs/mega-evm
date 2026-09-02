//! REX7 gas-clamp classification and the payload a clamp-induced exceed reports.
//!
//! The clamp is a lifecycle, not an amount: it is applied at a checkpoint, it binds the segment
//! that follows to one specific constraint, and it is consumed at the next checkpoint or at frame
//! exit. Whether the interpreter's true remaining happened to sit *above* the compute headroom or
//! exactly *on* it changes how much gets hidden — zero in the second case — but not whether the
//! clamp is in force. Both are the compute limit doing the stopping, and both must be reported as
//! such; only a frame whose own EVM gas runs out first is an ordinary out-of-gas.
//!
//! The payload has to match too. A frame-local binding reverts with
//! `MegaLimitExceeded(uint8 kind, uint64 limit)`, which the caller can decode and branch on, so its
//! `limit` must be the sub-frame budget that actually bound the clamp rather than the
//! transaction-level limit. A transaction-level binding halts with `ComputeGasLimitExceeded`, whose
//! `actual` must be the compute usage the transaction ends with — including the frame-exit
//! settlement that runs after the exceed is latched.

use crate::common::{
    base_db, plain_filler as common_plain_filler, transact, transact_default,
    transact_with_gas_limit, Outcome, CALLEE,
};
use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::BytecodeBuilder, EvmTxRuntimeLimits, LimitKind, MegaHaltReason, MegaLimitExceeded,
    MegaSpecId,
};
use revm::{
    bytecode::opcode::{
        CALL, DUP1, EXP, JUMPDEST, JUMPI, MSTORE, POP, RETURN, RETURNDATACOPY, RETURNDATASIZE,
        STOP, SUB, SWAP1,
    },
    context::result::ExecutionResult,
};

/// Per-spec runtime limits with the TX compute gas limit replaced.
fn compute_limit(limit: u64) -> impl Fn(MegaSpecId) -> EvmTxRuntimeLimits {
    move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit)
}

fn plain_filler(pairs: usize) -> Vec<u8> {
    common_plain_filler(BytecodeBuilder::default(), pairs).build_vec()
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
            assert_eq!(
                *actual, r7.compute_gas,
                "the reported usage must be the transaction's final compute usage",
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

/// The two shapes the spend-all knife edge needs: everything up to the `EXP`'s operands, and
/// the same thing with the `EXP` itself. A full-width exponent is charged through the plain
/// `gas!` macro, so a shortage is `InstructionResult::OutOfGas` — the variant
/// `Interpreter::halt` spends all remaining gas for.
fn spend_all_knife_edge_shapes() -> (Bytes, Bytes) {
    let operands = |mut code: Vec<u8>| {
        let mut builder = BytecodeBuilder::default();
        builder = builder.push_u256(U256::MAX).push_number(2u64);
        code.extend_from_slice(&builder.build_vec());
        code
    };
    let mut before = operands(plain_filler(20));
    before.push(STOP);
    let mut full = operands(plain_filler(20));
    full.push(EXP);
    full.push(STOP);
    (Bytes::from(before), Bytes::from(full))
}

fn calibrate_spend_all_knife_edge() -> KnifeEdge {
    let (before_code, full_code) = spend_all_knife_edge_shapes();
    let before = transact_default(MegaSpecId::REX7, base_db(before_code));
    let full = transact_default(MegaSpecId::REX7, base_db(full_code.clone()));
    assert!(before.is_success(), "calibration run must succeed: {:?}", before.result);
    assert!(full.is_success(), "calibration run must succeed: {:?}", full.result);
    // The receipt carries compute gas plus MegaETH storage gas; only the compute half is what
    // the compute limit bounds. Both calibration shapes must carry the same storage gas, or the
    // two budgets cannot be lined up from these readings.
    assert_eq!(
        before.gas_used - before.compute_gas,
        full.gas_used - full.compute_gas,
        "the two calibration shapes must carry the same storage gas",
    );

    let crossing_cost = full.compute_gas - before.compute_gas;
    assert!(crossing_cost > 1, "the EXP must have a real cost, got {crossing_cost}");
    KnifeEdge {
        code: full_code,
        // One gas short of the EXP on the EVM's own counter...
        gas_limit: before.gas_used + crossing_cost - 1,
        // ...and one gas short of it on the compute headroom, so the two coincide exactly.
        compute_limit: before.compute_gas + crossing_cost - 1,
        compute_before: before.compute_gas,
    }
}

/// An equal-value clamp on a spend-all out-of-gas charges the sender the same as REX6 at the
/// same point. The halt reason moves to a compute exceed; the receipt `gas_used` does not.
///
/// The existing equal-value case above is a `MemoryOOG` (`MSTORE`), which does not spend all
/// and is already pinned for classification. This is the `OutOfGas` neighbour, where revm
/// zeroes the counter before the clamp restore, so rescue has nothing to hand back.
#[test]
fn test_equal_value_clamp_on_a_spend_all_out_of_gas_matches_rex6_gas_used() {
    let edge = calibrate_spend_all_knife_edge();

    let r7 = transact_with_gas_limit(
        MegaSpecId::REX7,
        base_db(edge.code.clone()),
        compute_limit(edge.compute_limit)(MegaSpecId::REX7),
        edge.gas_limit,
    );
    let r6 = transact_with_gas_limit(
        MegaSpecId::REX6,
        base_db(edge.code.clone()),
        compute_limit(edge.compute_limit)(MegaSpecId::REX6),
        edge.gas_limit,
    );

    assert_eq!(
        r7.gas_used, edge.gas_limit,
        "an equal-value clamp on a spend-all out-of-gas returns nothing to the sender",
    );
    assert_eq!(
        r7.gas_used, r6.gas_used,
        "REX6 and REX7 charge the sender identically at the equal-value spend-all edge",
    );
    assert!(
        matches!(r7.halt_reason("REX7"), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "the equal-value clamp still classifies as a compute exceed; got {:?}",
        r7.result,
    );
    assert!(
        !matches!(r6.halt_reason("REX6"), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "REX6 reports the EVM's own out-of-gas, not a compute exceed; got {:?}",
        r6.result,
    );
}

// ---------------------------------------------------------------------------------------------
// The payload a clamp-induced exceed reports.
// ---------------------------------------------------------------------------------------------

/// A countdown loop of cheap plain opcodes, prefixed verbatim.
fn countdown_loop_code(prefix: &[u8], iterations: u16) -> Bytes {
    let mut code = prefix.to_vec();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = u8::try_from(code.len()).expect("loop target must fit in a PUSH1");
    code.push(JUMPDEST);
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(SWAP1);
    code.push(SUB);
    code.push(DUP1);
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(JUMPI);
    code.push(STOP);
    Bytes::from(code)
}

/// A caller that CALLs [`CALLEE`] and returns the sub-frame's return data verbatim, so the
/// `MegaLimitExceeded` payload the sub-frame reverted with is observable from the receipt.
fn call_and_return_revert_data(gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(gas)
        .append(CALL)
        .append(POP)
        .append(RETURNDATASIZE)
        .push_number(0u64) // dataOffset
        .push_number(0u64) // destOffset
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .push_number(0u64) // offset
        .append(RETURN)
        .build()
}

/// Decodes the `MegaLimitExceeded` payload a successful transaction returned.
fn decode_limit_exceeded(label: &str, outcome: &Outcome) -> MegaLimitExceeded {
    let output = match &outcome.result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        other => panic!("{label}: expected success carrying the sub-frame payload, got {other:?}"),
    };
    MegaLimitExceeded::abi_decode(&output)
        .unwrap_or_else(|e| panic!("{label}: return data is not MegaLimitExceeded: {e}"))
}

/// A frame-local clamp exceed must report the sub-frame budget that bound it, byte for byte the
/// same payload per-opcode enforcement produces.
///
/// The revert data is visible to the calling contract, which can decode `limit` and branch on it,
/// so a transaction-level value here is not a diagnostic difference — it is a different observable
/// return value for the same execution.
#[test]
fn test_frame_local_clamp_exceed_reports_the_sub_frame_budget() {
    // Enough iterations to outrun the sub-frame's 98/100 share of a one-million compute budget.
    let callee = countdown_loop_code(&[], 40_000);
    let code = call_and_return_revert_data(50_000_000);
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let limits = compute_limit(1_000_000);

    let r6 = transact(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7));

    let d6 = decode_limit_exceeded("REX6", &r6);
    let d7 = decode_limit_exceeded("REX7", &r7);

    assert_eq!(d6.kind, LimitKind::ComputeGas.as_u8(), "REX6 must blame compute gas");
    assert_eq!(d7.kind, d6.kind, "REX7 must blame the same dimension");
    assert_eq!(
        d7.limit, d6.limit,
        "the ABI-visible limit must be the sub-frame budget on both specs; REX6={} REX7={}",
        d6.limit, d7.limit
    );
    assert!(
        d7.limit < 1_000_000,
        "the sub-frame budget is a fraction of the TX limit, not the TX limit itself; got {}",
        d7.limit
    );
}

/// A transaction-level clamp exceed must report the usage the transaction actually ends with.
///
/// The exceed is latched at the frame's final result, but the frame-exit settlement that closes
/// the partial plain segment runs after that. A halt reason frozen at latch time reports a usage
/// the transaction never had.
#[test]
fn test_tx_level_clamp_halt_reports_the_final_usage() {
    let mut code = plain_filler(200);
    code.push(STOP);
    let code = Bytes::from(code);

    // The unconstrained run tells us both ends of the plain segment; putting the limit in the
    // middle of it guarantees the halt lands with a partial segment still unsettled.
    let free = transact_default(MegaSpecId::REX7, base_db(code.clone()));
    assert!(free.is_success(), "the unconstrained run must succeed: {:?}", free.result);
    let intrinsic = transact_default(MegaSpecId::REX7, base_db(Bytes::from(vec![STOP])));
    let midpoint = (intrinsic.compute_gas + free.compute_gas) / 2;

    let r7 = transact(MegaSpecId::REX7, base_db(code), compute_limit(midpoint)(MegaSpecId::REX7));

    let (limit, actual) = match r7.halt_reason("REX7") {
        MegaHaltReason::ComputeGasLimitExceeded { limit, actual } => (*limit, *actual),
        other => panic!("expected a compute-gas halt, got {other:?}"),
    };
    assert_eq!(limit, midpoint, "the reported limit is the configured TX compute limit");
    assert_eq!(
        r7.compute_gas, midpoint,
        "clamp enforcement stops the crossing opcode, so usage lands exactly on the limit",
    );
    assert_eq!(
        actual, r7.compute_gas,
        "the reported usage must be the tracker's final reading, not a pre-settlement snapshot; \
         reported={actual} tracker={}",
        r7.compute_gas
    );
    assert_eq!(
        r7.gas_used,
        r7.compute_gas + (intrinsic.gas_used - intrinsic.compute_gas),
        "the receipt must charge exactly the compute the transaction was allowed plus the \
         intrinsic storage gas",
    );
}
