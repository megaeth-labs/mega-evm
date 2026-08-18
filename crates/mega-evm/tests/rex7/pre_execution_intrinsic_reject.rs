//! The pre-execution intrinsic overrun: a transaction whose MegaETH-side intrinsic gas outgrows
//! the gas limit its sender supplied.
//!
//! `MINI_REX`..REX4 answer that with a synthetic top-level out-of-gas that burns the whole
//! envelope having executed nothing — the one halt shape that keeps a transaction's entire gas
//! envelope without ever creating a frame. REX5 moved the initial-gas check to the end of the
//! `MegaETH` storage-gas additions, which turns the same transaction into a validation error
//! before `pre_execution` debits the sender.
//!
//! That ordering is what keeps the REX7 destroyed lane complete without the synthetic halt having
//! to participate: no transaction on a spec that has the lane can reach the halt. These probes pin
//! both sides of the boundary, so a future change that re-opens the halt for REX7 turns red here
//! rather than silently reporting a transaction whose burnt envelope no lane accounts for.
//!
//! Every probe here is an ordinary transaction, and that is the whole of what they claim. The
//! second half of the reasoning — a validation reject produces no receipt, so there is nothing for
//! the lane to account for — holds only for ordinary transactions. A rejected deposit does produce
//! a receipt, rebuilt to report its whole gas limit, and the destroyed lane does have to account
//! for it; that shape lives in `deposit_receipt_rewrite`.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EVMError, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionError, MegaTransactionNew as _, MegaTransactionOutcome, SaltEnv,
    TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{context::TxEnv, Database as _};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
/// Value-transfer recipient that does not exist yet, so the transaction owes new-account storage
/// gas for materialising it.
const NEW_ACCOUNT: Address = address!("9000000000000000000000000000000000000009");

/// Covers the standard EVM intrinsic (21,000) and the REX flat intrinsic storage gas (39,000),
/// but not the dynamic new-account storage gas the hot bucket below scales up.
const INSUFFICIENT_GAS_LIMIT: u64 = 80_000;

/// Standard EVM intrinsic gas for this transaction — no calldata, no access list. This is the
/// whole of what `validate` records as compute gas before the first frame.
const INTRINSIC_COMPUTE_GAS: u64 = 21_000;

const CALLER_BALANCE: u64 = 10_000_000;

/// Runs the overrun transaction under `spec`: a value-transferring call to an empty account whose
/// SALT bucket is ten times the minimum, so the dynamic new-account storage gas alone is far
/// larger than the gas limit.
fn run_intrinsic_overrun(
    db: &mut MemoryDatabase,
    spec: MegaSpecId,
) -> Result<MegaTransactionOutcome, EVMError<Infallible, MegaTransactionError>> {
    let bucket_id = TestExternalEnvs::<Infallible>::bucket_id_for_account(NEW_ACCOUNT);
    let external_envs = TestExternalEnvs::<Infallible>::new()
        .with_bucket_capacity(bucket_id, MIN_BUCKET_SIZE as u64 * 10);

    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut tx = MegaTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(NEW_ACCOUNT),
        data: Bytes::new(),
        value: U256::from(1),
        gas_limit: INSUFFICIENT_GAS_LIMIT,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());

    MegaEvm::new(context).execute_transaction(tx)
}

fn funded_db() -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(CALLER_BALANCE));
    db
}

fn assert_sender_untouched(db: &mut MemoryDatabase) {
    let info = db.basic(CALLER).expect("db read should succeed").unwrap_or_default();
    assert_eq!(
        info.balance,
        U256::from(CALLER_BALANCE),
        "a validation reject must not debit the sender",
    );
    assert_eq!(info.nonce, 0, "a validation reject must not bump the sender's nonce");
}

/// REX7 (and REX6, its immediate predecessor) reject the overrun in validation. There is no
/// receipt, no burnt envelope, and therefore nothing for the destroyed lane to account for — the
/// synthetic halt that would keep the envelope is unreachable on both specs.
#[test]
fn test_intrinsic_overrun_is_a_validation_reject_from_rex6_on() {
    for spec in [MegaSpecId::REX6, MegaSpecId::REX7] {
        let mut db = funded_db();
        let err = match run_intrinsic_overrun(&mut db, spec) {
            Err(err) => err,
            Ok(outcome) => panic!(
                "{spec:?} must reject the intrinsic overrun before execution, got {:?}",
                outcome.result_and_state.result,
            ),
        };
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("CallGasCostMoreThanGasLimit"),
            "{spec:?}: expected CallGasCostMoreThanGasLimit, got {rendered}",
        );
        assert_sender_untouched(&mut db);
    }
}

/// REX4 keeps the frozen shape: validation accepts the transaction, the sender pays, and execution
/// answers with a synthetic out-of-gas that burns the whole envelope. The compute-gas total is
/// exactly the intrinsic `validate` recorded — the burnt remainder is attributed to nothing, which
/// is the frozen accounting the destroyed lane must not retroactively change.
#[test]
fn test_rex4_intrinsic_overrun_burns_the_envelope_with_no_destroyed_lane() {
    let mut db = funded_db();
    let outcome = run_intrinsic_overrun(&mut db, MegaSpecId::REX4)
        .expect("pre-REX5 specs must not reject the overrun as a validation error");

    assert!(outcome.result_and_state.result.is_halt(), "REX4 answers the overrun with a halt");
    assert_eq!(
        outcome.result_and_state.result.tx_gas_used(),
        INSUFFICIENT_GAS_LIMIT,
        "the halt burns the whole envelope",
    );
    assert_eq!(
        outcome.compute_gas_used, INTRINSIC_COMPUTE_GAS,
        "REX4 reports only the intrinsic compute gas validate recorded",
    );
    assert_eq!(outcome.compute_gas_destroyed, 0, "pre-REX7 specs have no destroyed lane");
}
