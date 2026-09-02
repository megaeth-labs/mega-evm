//! A failed OP deposit's receipt is rewritten to report the whole gas limit, after every `MegaETH`
//! settlement has already run.
//!
//! An OP deposit is not allowed to fail, so when one does, its receipt is rebuilt at the outermost
//! error boundary: the journal is rolled back to nothing but the nonce bump and the mint, and the
//! reported gas becomes the transaction's whole `gas_limit`. Two shapes arrive there, and they
//! start from opposite accounting positions:
//!
//! - a validation reject, which never reached a settlement at all, so its lanes hold only the
//!   intrinsic compute gas `validate` recorded before returning the error;
//! - an execution halt, which settled correctly against the envelope it burnt and is then raised
//!   back to `gas_limit`, re-taking whatever the resource-limit rescue had handed back.
//!
//! Both end at the same place: a receipt burning an envelope for which nothing was executed. The
//! boundary books the difference as destroyed compute gas, so the reported total covers the
//! receipt while the enforced total — what the per-tx limits and the block's admission counter
//! read — stays exactly the work the transaction performed.
//!
//! What is deliberately not affected: pre-REX7 specs, which have no destroyed lane and whose lane
//! values here must stay what they always were; and the keyless-deploy sandbox, whose own rejected
//! transactions never settle a derivation, because the law is stated over an outer transaction's
//! final envelope.

use crate::common::{transact_mega_tx, transact_tx, Outcome, ONE_ETH};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::{SolCall as _, SolError as _};
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    constants::rex::TX_INTRINSIC_STORAGE_GAS,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
    MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
};
use revm::{
    bytecode::opcode::{INVALID, JUMP, JUMPDEST},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    inspector::NoOpInspector,
};
use std::vec::Vec;

/// Sender of the deposit transactions.
const DEPOSIT_CALLER: Address = address!("0000000000000000000000000000000000350000");
/// Callee of the deposit transactions.
const TARGET: Address = address!("0000000000000000000000000000000000350001");
/// Relayer that sends the keyless-deploy transaction.
const RELAYER: Address = address!("0000000000000000000000000000000000350002");

/// Standard EVM intrinsic gas for a plain call with no calldata and no access list — the whole of
/// what `validate` records as compute gas before the first frame opens.
const BASE_INTRINSIC_GAS: u64 = 21_000;

/// What a plain deposit call must supply before a frame can open: the standard EVM intrinsic plus
/// `MegaETH`'s flat intrinsic storage gas, which is charged to the envelope but is not compute.
const INTRINSIC_REQUIREMENT: u64 = BASE_INTRINSIC_GAS + TX_INTRINSIC_STORAGE_GAS;

/// A source hash no `MegaETH` component produces, so the deposit is an ordinary user deposit
/// rather than a system-originated one.
fn user_source_hash() -> B256 {
    B256::repeat_byte(0x42)
}

/// A deposit transaction calling [`TARGET`], with the given source hash and gas limit.
fn deposit_tx(source_hash: B256, gas_limit: u64) -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(DEPOSIT_CALLER)
            .call(TARGET)
            .gas_limit(gas_limit)
            .gas_price(0)
            .build_fill(),
    );
    tx.deposit.source_hash = source_hash;
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// The same call as [`deposit_tx`], as an ordinary (non-deposit) transaction — the control that
/// shows what the receipt would have reported without the rewrite.
fn plain_tx(gas_limit: u64) -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(DEPOSIT_CALLER)
            .call(TARGET)
            .gas_limit(gas_limit)
            .gas_price(0)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// A funded sender, so the deposit never owes caller-materialization storage gas, plus whatever
/// code [`TARGET`] needs for the shape under test.
fn db(target_code: Option<Bytes>) -> MemoryDatabase {
    let db = MemoryDatabase::default().account_balance(DEPOSIT_CALLER, U256::from(ONE_ETH));
    match target_code {
        Some(code) => db.account_code(TARGET, code),
        None => db,
    }
}

fn run(spec: MegaSpecId, target_code: Option<Bytes>, tx: MegaTransaction) -> Outcome {
    transact_mega_tx(
        spec,
        db(target_code),
        EvmTxRuntimeLimits::from_spec(spec),
        tx,
        &TestExternalEnvs::default(),
    )
}

fn run_with_compute_limit(
    spec: MegaSpecId,
    target_code: Option<Bytes>,
    tx: MegaTransaction,
    tx_compute_gas_limit: u64,
) -> Outcome {
    transact_mega_tx(
        spec,
        db(target_code),
        EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(tx_compute_gas_limit),
        tx,
        &TestExternalEnvs::default(),
    )
}

/// Asserts the shape every rewritten receipt has: a `FailedDeposit` halt reporting the whole gas
/// limit.
fn assert_failed_deposit(outcome: &Outcome, gas_limit: u64, label: &str) {
    let rendered = std::format!("{:?}", outcome.halt_reason(label));
    assert!(
        rendered.contains("FailedDeposit"),
        "{label}: a failed deposit must be reported as FailedDeposit, got {rendered}",
    );
    assert_eq!(
        outcome.gas_used, gas_limit,
        "{label}: a failed deposit's receipt reports the whole gas limit",
    );
}

/// Runtime code that loops forever, so the transaction stops on a budget rather than on its own.
fn spin_forever() -> Bytes {
    BytecodeBuilder::default().append(JUMPDEST).push_number(0u8).append(JUMP).build()
}

/// A deposit one gas short of its own intrinsic requirement never reaches execution: `validate`
/// rejects it after recording the standard EVM intrinsic as compute and before booking the
/// `MegaETH` share as non-compute. The receipt still reports the whole gas limit, so everything
/// past the intrinsic is an envelope that nothing was executed for.
#[test]
fn test_underfunded_deposit_reject_settles_the_rewritten_envelope() {
    let gas_limit = INTRINSIC_REQUIREMENT - 1;
    let outcome = run(MegaSpecId::REX7, None, deposit_tx(user_source_hash(), gas_limit));

    assert_failed_deposit(&outcome, gas_limit, "underfunded deposit");
    assert_eq!(
        outcome.enforced(),
        BASE_INTRINSIC_GAS,
        "only the intrinsic validate recorded may enforce — the transaction executed nothing",
    );
    assert_eq!(
        outcome.destroyed,
        gas_limit - BASE_INTRINSIC_GAS,
        "the rest of the rewritten envelope is destroyed",
    );
    assert_eq!(
        outcome.compute_gas, gas_limit,
        "the reported total must cover the receipt: the reject books no MegaETH storage gas, so \
         the whole envelope is compute",
    );
    assert_eq!(
        outcome.non_compute_gas(),
        0,
        "the reject returns before the MegaETH share of intrinsic gas is booked",
    );
    assert_eq!(
        outcome.booked_destroyed(),
        outcome.destroyed,
        "the per-site booking and the derived total must agree",
    );
}

/// A deposit that halts inside execution has already settled correctly against the envelope it
/// burnt, and that envelope is the whole gas limit — an exceptional halt keeps everything. The
/// rewrite reports the same number, so this shape needs no correction and must not receive one.
#[test]
fn test_deposit_runtime_halt_keeps_its_settlement() {
    const GAS_LIMIT: u64 = 200_000;
    let code = BytecodeBuilder::default().append(INVALID).build();
    let outcome = run(MegaSpecId::REX7, Some(code), deposit_tx(user_source_hash(), GAS_LIMIT));

    assert_failed_deposit(&outcome, GAS_LIMIT, "halting deposit");
    assert_eq!(
        outcome.enforced(),
        BASE_INTRINSIC_GAS,
        "INVALID performs no work, so the intrinsic is the whole of what enforces",
    );
    assert_eq!(
        outcome.destroyed,
        GAS_LIMIT - INTRINSIC_REQUIREMENT,
        "the frame's whole budget is destroyed",
    );
    assert_eq!(
        outcome.non_compute_gas(),
        i128::from(TX_INTRINSIC_STORAGE_GAS),
        "the MegaETH share of intrinsic gas is booked as non-compute",
    );
    assert_eq!(
        outcome.compute_gas,
        GAS_LIMIT - TX_INTRINSIC_STORAGE_GAS,
        "the reported total plus the storage gas must cover the receipt",
    );
}

/// A deposit stopped by a per-transaction resource limit is the shape where the rewrite actually
/// takes gas back. The limit halt rescues the frame's remaining gas for the sender, which shrinks
/// the envelope settlement reads; the rewrite then raises the receipt back to the gas limit. The
/// rescued amount is exactly what the boundary has to destroy, which is asserted against an
/// identical non-deposit transaction rather than against a constant.
#[test]
fn test_deposit_resource_limit_halt_destroys_what_the_rescue_returned() {
    const GAS_LIMIT: u64 = 5_000_000;
    const COMPUTE_LIMIT: u64 = 100_000;

    let plain = run_with_compute_limit(
        MegaSpecId::REX7,
        Some(spin_forever()),
        plain_tx(GAS_LIMIT),
        COMPUTE_LIMIT,
    );
    let rescued = GAS_LIMIT - plain.total_gas_spent;
    assert!(
        rescued > 0,
        "the control must actually rescue gas, otherwise the shape proves nothing; \
         spent={} limit={GAS_LIMIT}",
        plain.total_gas_spent,
    );

    let deposit = run_with_compute_limit(
        MegaSpecId::REX7,
        Some(spin_forever()),
        deposit_tx(user_source_hash(), GAS_LIMIT),
        COMPUTE_LIMIT,
    );

    assert_failed_deposit(&deposit, GAS_LIMIT, "resource-limited deposit");
    assert_eq!(
        deposit.enforced(),
        plain.enforced(),
        "the rewrite must not change what enforces — the same work was performed either way",
    );
    assert_eq!(
        deposit.enforced(),
        COMPUTE_LIMIT,
        "the transaction ran until the compute limit bound it",
    );
    assert_eq!(
        deposit.destroyed,
        plain.destroyed + rescued,
        "the rewrite destroys exactly the gas the rescue had returned to the sender",
    );
    assert_eq!(
        deposit.compute_gas,
        plain.compute_gas + rescued,
        "the reported total grows by the same amount, so it covers the rewritten receipt",
    );
    assert_eq!(
        deposit.non_compute_gas(),
        plain.non_compute_gas(),
        "the storage-gas lane is untouched by the rewrite",
    );
}

/// Inspection is a separate entry into the handler, and the boundary settlement has to be on both.
/// The same resource-limited deposit is run with a no-op inspector attached and must report
/// exactly what the uninspected run reports — an inspector changes what is observed, never what is
/// accounted.
#[test]
fn test_inspected_deposit_failure_settles_the_same_way() {
    const GAS_LIMIT: u64 = 5_000_000;
    const COMPUTE_LIMIT: u64 = 100_000;

    let uninspected = run_with_compute_limit(
        MegaSpecId::REX7,
        Some(spin_forever()),
        deposit_tx(user_source_hash(), GAS_LIMIT),
        COMPUTE_LIMIT,
    );

    let mut database = db(Some(spin_forever()));
    let mut context = MegaContext::new(&mut database, MegaSpecId::REX7).with_tx_runtime_limits(
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(COMPUTE_LIMIT),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let executed = evm
        .execute_transaction(deposit_tx(user_source_hash(), GAS_LIMIT))
        .expect("tx should not surface EVMError");

    assert_eq!(
        (
            executed.result_and_state.result.tx_gas_used(),
            executed.compute_gas_used,
            executed.compute_gas_enforced,
            executed.compute_gas_destroyed,
        ),
        (
            uninspected.gas_used,
            uninspected.compute_gas,
            uninspected.enforced(),
            uninspected.destroyed,
        ),
        "the inspected run must account for the rewritten envelope exactly as the plain run does",
    );
    assert_eq!(
        executed.compute_gas_destroyed,
        GAS_LIMIT - TX_INTRINSIC_STORAGE_GAS - COMPUTE_LIMIT,
        "the destroyed total is the envelope less the storage gas and the work performed",
    );
}

/// A system-originated deposit is exempt from `MegaETH`'s per-transaction resource limits, which
/// is a statement about enforcement, not about recording. Its lanes must still account for the
/// rewritten envelope exactly as a user deposit's do.
#[test]
fn test_exempt_deposit_reject_still_accounts_for_the_envelope() {
    let gas_limit = INTRINSIC_REQUIREMENT - 1;
    let exempt =
        run(MegaSpecId::REX7, None, deposit_tx(MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, gas_limit));
    let user = run(MegaSpecId::REX7, None, deposit_tx(user_source_hash(), gas_limit));

    assert_failed_deposit(&exempt, gas_limit, "exempt deposit");
    assert_eq!(
        exempt.enforced(),
        BASE_INTRINSIC_GAS,
        "the exempt deposit enforces only the intrinsic validate recorded",
    );
    assert_eq!(
        exempt.destroyed,
        gas_limit - BASE_INTRINSIC_GAS,
        "the rest of its rewritten envelope is destroyed, exemption or not",
    );
    assert_eq!(
        (exempt.compute_gas, exempt.enforced(), exempt.destroyed, exempt.non_compute_gas()),
        (user.compute_gas, user.enforced(), user.destroyed, user.non_compute_gas()),
        "an exemption suppresses limit enforcement, not accounting",
    );
}

/// The inner keyless transaction's own gas limit, set below what `MegaETH`'s intrinsic
/// requirement for a create transaction comes to, so the sandbox transaction is rejected in
/// validation rather than running.
const SANDBOX_REJECT_INNER_GAS_LIMIT: u64 = INTRINSIC_REQUIREMENT;

/// Builds a deterministic pre-EIP-155 keyless deployment transaction whose gas limit cannot cover
/// its own `MegaETH` intrinsic requirement.
fn underfunded_keyless_tx_bytes() -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: SANDBOX_REJECT_INNER_GAS_LIMIT,
        to: TxKind::Create,
        value: U256::ZERO,
        input: BytecodeBuilder::default().append(INVALID).build(),
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

/// A keyless-deploy sandbox transaction that fails validation is rewritten into a failed deposit
/// too — inside the sandbox, where no settlement of its own belongs. Its usage is discarded and
/// the interceptor hands the whole reservation back, so the outer transaction sees only the
/// upfront charges. Pinned across both specs: the boundary settlement must not reach in here and
/// change what the outer transaction reports.
#[test]
fn test_keyless_sandbox_reject_leaves_the_outer_transaction_alone() {
    const OUTER_GAS_LIMIT: u64 = 1_000_000;
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: underfunded_keyless_tx_bytes(),
        gasLimitOverride: U256::from(SANDBOX_REJECT_INNER_GAS_LIMIT),
    }
    .abi_encode();

    let outcomes: Vec<Outcome> = [MegaSpecId::REX6, MegaSpecId::REX7]
        .into_iter()
        .map(|spec| {
            let tx = TxEnvBuilder::default()
                .caller(RELAYER)
                .call(KEYLESS_DEPLOY_ADDRESS)
                .gas_limit(OUTER_GAS_LIMIT)
                .chain_id(Some(1))
                .data(Bytes::from(call_data.clone()))
                .build_fill();
            transact_tx(
                spec,
                MemoryDatabase::default().account_balance(RELAYER, U256::from(10 * ONE_ETH)),
                EvmTxRuntimeLimits::from_spec(spec),
                tx,
                &TestExternalEnvs::default(),
            )
        })
        .collect();
    let (rex6, rex7) = (&outcomes[0], &outcomes[1]);

    let ExecutionResult::Revert { output, .. } = &rex7.result else {
        panic!("a sandbox reject must revert the outer call, got {:?}", rex7.result);
    };
    IKeylessDeploy::InvalidTransaction::abi_decode(output).expect(
        "the sandbox transaction must be rejected in validation — that is the shape whose inner \
         deposit is rewritten inside the sandbox",
    );
    assert_eq!(
        rex7.gas_used, rex6.gas_used,
        "a sandbox reject must cost the outer transaction the same on both specs",
    );
    assert_eq!(
        rex7.compute_gas, rex6.compute_gas,
        "the sandbox's own rejected envelope must not reach the outer transaction's lanes",
    );
    assert_eq!(
        rex7.destroyed, 0,
        "nothing the outer transaction did was destroyed: the reservation came back in full",
    );
}

/// Pre-REX7 specs have no destroyed lane, and the boundary settlement must leave them alone. Both
/// rewritten shapes are run under REX6 and pinned to the accounting they have always produced:
/// the receipt reports the whole gas limit, nothing is destroyed, and the reported compute total
/// stays exactly what REX7 now enforces.
#[test]
fn test_rex6_deposit_failures_keep_their_frozen_accounting() {
    let underfunded_gas_limit = INTRINSIC_REQUIREMENT - 1;
    let rex6_reject =
        run(MegaSpecId::REX6, None, deposit_tx(user_source_hash(), underfunded_gas_limit));
    let rex7_reject =
        run(MegaSpecId::REX7, None, deposit_tx(user_source_hash(), underfunded_gas_limit));

    assert_failed_deposit(&rex6_reject, underfunded_gas_limit, "REX6 underfunded deposit");
    assert_eq!(rex6_reject.destroyed, 0, "REX6 has no destroyed lane");
    assert_eq!(
        rex6_reject.compute_gas, BASE_INTRINSIC_GAS,
        "REX6 reports only the intrinsic validate recorded before the reject",
    );
    assert_eq!(
        rex7_reject.enforced(),
        rex6_reject.compute_gas,
        "REX7 must enforce exactly what REX6 recorded — the destroyed lane is an addition to the \
         reported total, never a change to the enforced one",
    );

    const HALT_GAS_LIMIT: u64 = 200_000;
    let code = BytecodeBuilder::default().append(INVALID).build();
    let rex6_halt =
        run(MegaSpecId::REX6, Some(code.clone()), deposit_tx(user_source_hash(), HALT_GAS_LIMIT));
    let rex7_halt =
        run(MegaSpecId::REX7, Some(code), deposit_tx(user_source_hash(), HALT_GAS_LIMIT));

    assert_failed_deposit(&rex6_halt, HALT_GAS_LIMIT, "REX6 halting deposit");
    assert_eq!(rex6_halt.destroyed, 0, "REX6 has no destroyed lane");
    assert_eq!(
        rex7_halt.enforced(),
        rex6_halt.compute_gas,
        "REX7 must enforce exactly what REX6 recorded for the halting shape too",
    );
    assert_eq!(
        rex6_halt.gas_used, rex7_halt.gas_used,
        "the receipt is op-revm's, identical on both specs",
    );
}
