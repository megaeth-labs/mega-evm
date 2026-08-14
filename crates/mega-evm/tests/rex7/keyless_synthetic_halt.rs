//! The `KeylessDeploy` interceptor's synthetic halts destroy the envelope they burn.
//!
//! An interceptor returns its result out of `frame_init`, before a child EVM frame exists, so the
//! frame-exit settlement that splits an ordinary exceptional halt never runs for it. Two of the
//! interceptor's halts keep the whole call envelope, and under REX7 the part they did not perform
//! belongs in the destroyed lane:
//!
//! - the call cannot pay the fixed dispatch overhead, so nothing at all was performed;
//! - the call paid the overhead but cannot pay the deploy signer's materialization storage gas, so
//!   the overhead is the only work performed.
//!
//! Every other synthetic result on this path is a `Return`, a `Revert`, or an out-of-gas whose
//! remaining gas is rescued for the sender. None of those destroys anything: the first two hand
//! the envelope back to the parent, and a rescue is a refund — booking it as destroyed as well
//! would report gas the sender got back, and would inflate the block's compute statistic by the
//! same amount. The rescue shape is pinned here next to the two destroying ones so the difference
//! stays visible.
//!
//! Every probe's expected destroyed amount is predicted from the frame's gas envelope, and that
//! envelope is measured through the `GasLimitTooLow` revert rather than read back out of the lane
//! under test.

use crate::common::{transact_tx, Outcome, ONE_ETH};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::{SolCall as _, SolError as _};
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    constants::{rex::NEW_ACCOUNT_STORAGE_GAS_BASE, rex2::KEYLESS_DEPLOY_OVERHEAD_GAS},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaSpecId, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
    MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::STOP,
    context::{result::ExecutionResult, tx::TxEnvBuilder},
};
use std::vec::Vec;

/// Relayer that sends the keyless-deploy transactions.
const RELAYER: Address = address!("0000000000000000000000000000000000340009");

/// The minimum bucket capacity, at which deploy-signer materialization is free and the
/// materialization arm is unreachable.
const MIN_BUCKET: u64 = MIN_BUCKET_SIZE as u64;

/// Twice the minimum capacity, which prices deploy-signer materialization at one base unit
/// (`NEW_ACCOUNT_STORAGE_GAS_BASE × (multiplier − 1)`).
const DOUBLE_BUCKET: u64 = 2 * MIN_BUCKET;

/// The materialization charge [`DOUBLE_BUCKET`] produces.
const MATERIALIZATION_GAS: u64 = NEW_ACCOUNT_STORAGE_GAS_BASE;

/// The inner keyless transaction's own gas limit. `gasLimitOverride` is passed well above it so
/// the pre-cap check clears; the post-cap re-check at step 4b is what the calibration below reads.
const INNER_TX_GAS_LIMIT: u64 = 200_000;

/// Builds a deterministic pre-EIP-155 keyless deployment transaction. Its init code never runs on
/// any path exercised here — every probe fails before the sandbox is built.
fn keyless_tx_bytes() -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: INNER_TX_GAS_LIMIT,
        to: TxKind::Create,
        value: U256::ZERO,
        input: BytecodeBuilder::default().append(STOP).build(),
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(buf)
}

/// Runs a top-level keyless-deploy call. Depth zero is the only depth the interceptor fires at, so
/// every probe here has to be a direct transaction.
fn run(
    spec: MegaSpecId,
    tx_gas_limit: u64,
    bucket_capacity: u64,
    tx_compute_gas_limit: Option<u64>,
) -> Outcome {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes(),
        gasLimitOverride: U256::from(1_000_000u64),
    }
    .abi_encode();

    let mut limits = EvmTxRuntimeLimits::from_spec(spec);
    if let Some(limit) = tx_compute_gas_limit {
        limits = limits.with_tx_compute_gas_limit(limit);
    }

    let db = MemoryDatabase::default().account_balance(RELAYER, U256::from(10 * ONE_ETH));
    let tx = TxEnvBuilder::default()
        .caller(RELAYER)
        .call(KEYLESS_DEPLOY_ADDRESS)
        .gas_limit(tx_gas_limit)
        .chain_id(Some(1))
        .data(Bytes::from(call_data))
        .build_fill();

    transact_tx(
        spec,
        db,
        limits,
        tx,
        &TestExternalEnvs::default().with_default_bucket_capacity(bucket_capacity),
    )
}

/// The transaction gas limit the envelope calibration runs at.
///
/// It has to sit in the window where the interceptor clears both the dispatch overhead and the
/// materialization charge — so the same value works at either bucket capacity — while leaving the
/// capped override below the inner transaction's own gas limit, which is what makes the post-cap
/// re-check revert instead of letting the sandbox start.
const CALIBRATION_GAS: u64 = 300_000;

/// The gas envelope the top-level frame opens with, measured through a channel the destroyed lane
/// cannot influence.
///
/// A keyless-deploy call that clears the dispatch overhead but cannot cover the inner
/// transaction's own gas limit reverts with `GasLimitTooLow`, and the `providedGasLimit` it
/// carries is what the outer frame had left at that point. Adding back the charges taken before it
/// recovers the envelope the frame started with — so the probes below predict their expected
/// destroyed amount instead of reading it back out of the lane under test.
fn frame_envelope(bucket_capacity: u64, charges_before: u64) -> u64 {
    let probe = run(MegaSpecId::REX7, CALIBRATION_GAS, bucket_capacity, None);
    let ExecutionResult::Revert { output, .. } = &probe.result else {
        panic!("the calibration probe must revert, got {:?}", probe.result);
    };
    let decoded = IKeylessDeploy::GasLimitTooLow::abi_decode(output)
        .expect("the calibration probe must revert with GasLimitTooLow");
    assert_eq!(
        decoded.txGasLimit, INNER_TX_GAS_LIMIT,
        "the revert must be the post-cap re-check against the inner transaction's gas limit",
    );
    decoded.providedGasLimit + charges_before
}

/// The gas the transaction pays before the top-level frame opens.
fn withheld_intrinsic() -> u64 {
    let envelope_at_min = frame_envelope(MIN_BUCKET, KEYLESS_DEPLOY_OVERHEAD_GAS);
    // Cross-check: raising the bucket capacity must change nothing except the materialization
    // charge. This validates the charge's size and, at the same time, that the pre-frame intrinsic
    // is not itself bucket-scaled — which is what lets one calibration serve both probes.
    let envelope_at_double =
        frame_envelope(DOUBLE_BUCKET, KEYLESS_DEPLOY_OVERHEAD_GAS + MATERIALIZATION_GAS);
    assert_eq!(
        envelope_at_min, envelope_at_double,
        "the materialization charge must be the only thing the bucket capacity changes",
    );
    CALIBRATION_GAS - envelope_at_min
}

/// Asserts that REX7 changed nothing a consumer of the transaction can see, which is what lets the
/// destroyed lane be a pure addition to the reported total.
fn assert_receipt_parity(rex6: &Outcome, rex7: &Outcome, label: &str) {
    assert_eq!(
        std::format!("{:?}", rex6.result),
        std::format!("{:?}", rex7.result),
        "{label}: the execution result must be identical across the two specs",
    );
    assert_eq!(rex6.gas_used, rex7.gas_used, "{label}: receipt gas_used must be identical");
    assert_eq!(rex6.destroyed, 0, "{label}: REX6 has no destroyed lane");
    assert_eq!(
        rex7.enforced(),
        rex6.compute_gas,
        "{label}: REX7 must enforce exactly what REX6 recorded — the destroyed lane is an \
         addition to the reported total, never a change to the enforced one",
    );
}

/// The call cannot pay the fixed dispatch overhead. Nothing was recorded as compute and nothing is
/// rescued, so the whole envelope the frame opened with is destroyed.
#[test]
fn test_underfunded_dispatch_destroys_the_whole_envelope() {
    // An envelope below the dispatch overhead, and not a round number, so an amount that happened
    // to coincide with some other quantity would not pass.
    const ENVELOPE: u64 = 33_333;
    const { assert!(ENVELOPE < KEYLESS_DEPLOY_OVERHEAD_GAS) };
    let tx_gas_limit = withheld_intrinsic() + ENVELOPE;

    let rex6 = run(MegaSpecId::REX6, tx_gas_limit, MIN_BUCKET, None);
    let rex7 = run(MegaSpecId::REX7, tx_gas_limit, MIN_BUCKET, None);

    assert!(!rex7.is_success(), "the underfunded dispatch must halt: {:?}", rex7.result);
    assert_eq!(
        rex7.gas_used, tx_gas_limit,
        "the whole transaction envelope is burnt — nothing is rescued on this path",
    );
    assert_receipt_parity(&rex6, &rex7, "underfunded dispatch");

    assert_eq!(
        rex7.destroyed, ENVELOPE,
        "the destroyed part is the whole envelope the frame opened with, because the call failed \
         before performing anything",
    );
}

/// The overhead is paid, the deploy signer's materialization storage gas is not. The overhead is
/// the only work performed, so what the call still held is what gets destroyed.
#[test]
fn test_underfunded_signer_materialization_destroys_what_the_overhead_left() {
    // Enough envelope for the dispatch overhead, then a remainder too small for the
    // materialization charge — so the charge is the thing that cannot fit.
    const REMAINDER: u64 = 9_999;
    const { assert!(REMAINDER < MATERIALIZATION_GAS) };
    let tx_gas_limit = withheld_intrinsic() + KEYLESS_DEPLOY_OVERHEAD_GAS + REMAINDER;

    let rex6 = run(MegaSpecId::REX6, tx_gas_limit, DOUBLE_BUCKET, None);
    let rex7 = run(MegaSpecId::REX7, tx_gas_limit, DOUBLE_BUCKET, None);

    assert!(!rex7.is_success(), "the underfunded materialization must halt: {:?}", rex7.result);
    assert_eq!(
        rex7.gas_used, tx_gas_limit,
        "the whole transaction envelope is burnt — this path does not rescue either",
    );
    assert_receipt_parity(&rex6, &rex7, "underfunded materialization");

    assert_eq!(
        rex7.destroyed, REMAINDER,
        "the destroyed part is what the call still held after paying the dispatch overhead, not \
         the whole envelope — the overhead was performed and stays enforcing",
    );
    assert_eq!(
        rex7.enforced() - withheld_compute(),
        KEYLESS_DEPLOY_OVERHEAD_GAS,
        "and the enforcing lane carries that overhead on top of the intrinsic, which is what \
         separates this probe from the underfunded-dispatch one",
    );
}

/// The compute gas an underfunded-dispatch run records: the transaction intrinsic and nothing
/// else, because the call fails before the overhead is charged.
fn withheld_compute() -> u64 {
    run(MegaSpecId::REX7, withheld_intrinsic() + 1_000, MIN_BUCKET, None).enforced()
}

/// The transaction-level compute exceed crossed while recording the dispatch overhead rescues the
/// call's remaining gas for the sender. A rescue is a refund, so nothing on that path is
/// destroyed — booking the rescued remainder as destroyed as well would report gas that was
/// handed back.
#[test]
fn test_rescued_overhead_exceed_is_not_also_destroyed() {
    // A compute limit under the overhead makes the overhead's own recording cross it.
    const TX_COMPUTE_LIMIT: u64 = 50_000;
    const TX_GAS_LIMIT: u64 = 30_000_000;

    let rex6 = run(MegaSpecId::REX6, TX_GAS_LIMIT, MIN_BUCKET, Some(TX_COMPUTE_LIMIT));
    let rex7 = run(MegaSpecId::REX7, TX_GAS_LIMIT, MIN_BUCKET, Some(TX_COMPUTE_LIMIT));

    assert!(!rex7.is_success(), "the compute exceed must halt: {:?}", rex7.result);
    assert_eq!(
        rex7.destroyed, 0,
        "a rescued halt destroys nothing: the remaining gas goes back to the sender, so counting \
         it as destroyed would report the same gas twice",
    );
    assert_receipt_parity(&rex6, &rex7, "rescued overhead exceed");

    // The rescue is real, so the receipt lands far below the envelope — the contrast that makes
    // the zero above meaningful rather than vacuous.
    assert!(
        rex7.gas_used < TX_GAS_LIMIT / 2,
        "the rescue must actually refund most of the envelope; gas_used={}",
        rex7.gas_used,
    );
}
