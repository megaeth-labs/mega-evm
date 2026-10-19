//! The limits a keyless deployment is held to, and what a stop leaves behind.
//!
//! The creation is an ordinary frame below its `keylessDeploy` call, so it is held to everything
//! an ordinary creation is: its share of the data-size budget, the transaction's limits. A limit
//! the creation crosses, its budget or the transaction's, reverts the creation alone, and the call
//! answers `ExecutionReverted` with the stop's revert data, keeping what the creation's start
//! wrote. A limit the call itself crosses — its budget, crossed by the nonce record a failed
//! creation leaves it — reverts the call with the stop, which takes back everything the deployment
//! wrote, the signer's nonce included, and the sender pays for what ran and gets the rest of its
//! gas and its reservoir back.
//!
//! The creation's stop is pinned as a column of the matrix: every limit a creation can cross,
//! from a signer at nonce 0 and at nonce 1, below the execution cap and above it, with and without
//! an inspector that records every frame's end.
//!
//! The last tests pin the three places gas a mechanism grants a frame could leak back to a
//! caller or to the sender: an answer built without a frame, the settlement of a stopped
//! transaction, and the return of the frame's unspent gas.

use alloy_sol_types::SolError;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP, system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS, LimitCheck,
    LimitKind, MegaLimitExceeded, LOG_BASE_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{LOG0, PUSH0, REVERT, TIMESTAMP},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        InstructionResult,
    },
    primitives::KECCAK_EMPTY,
    Database, Inspector,
};

use super::{
    detention::{crossing_left, reads_then_burns},
    *,
};
use crate::common::context;

/// Init code that logs `len` bytes of data and deploys a one-byte runtime.
fn logging(len: u64) -> Bytes {
    let prefix = mega_evm::test_utils::BytecodeBuilder::default()
        .push_number(len)
        .push_number(0_u64)
        .append(LOG0)
        .build_vec();
    constructor(&prefix, &runtime(1))
}

/// Init code that fills a fresh slot and deploys a one-byte runtime.
fn filling_a_slot() -> Bytes {
    let prefix = mega_evm::test_utils::BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .build_vec();
    constructor(&prefix, &runtime(1))
}

/// The data size a transaction carrying `deployment`'s calldata counts before any frame: its
/// body.
fn body_bytes(deployment: &Deployment) -> u64 {
    reference(deployment.call_data(LARGE_OVERRIDE), GAS_LIMITS[0]).usage.data_size
}

/// Runs a `keylessDeploy` of `deployment` at `gas_limit` under `limits`.
fn limited(deployment: &Deployment, gas_limit: u64, limits: EvmTxRuntimeLimits) -> Outcome {
    run_with(system_db(), deployment.call_data(LARGE_OVERRIDE), gas_limit, limits)
}

type Outcome = MegaTransactionOutcome;

/// The stop a transaction reverted with.
fn stop(outcome: &Outcome) -> MegaLimitExceeded {
    let ExecutionResult::Revert { output, .. } = &outcome.result else {
        panic!("the transaction did not revert: {:?}", outcome.result);
    };
    MegaLimitExceeded::abi_decode(output).expect("the revert data is a limit stop")
}

/// The stop a deployment's creation reverted with, reported as its `ExecutionReverted` output.
fn creation_stop(outcome: &Outcome) -> MegaLimitExceeded {
    let KeylessDeployError::ExecutionReverted { output, .. } = failure(outcome) else {
        panic!("the creation did not revert: {:?}", outcome.result);
    };
    MegaLimitExceeded::abi_decode(&output).expect("the creation's revert data is a limit stop")
}

/// A transaction stopped inside its keyless deployment keeps nothing of it: the signer's nonce
/// is where it was, no code is deployed, no state or history gas beyond the body is spent, no
/// record is kept, and above the execution cap the whole reservoir but the body's history comes
/// back. The sender pays the overhead and what ran.
fn assert_stopped_whole(outcome: &Outcome, deployment: &Deployment, gas_limit: u64) {
    assert_nothing_kept(outcome, deployment, gas_limit, 0, "");
}

/// [`assert_stopped_whole`], for a signer whose nonce was `signer_nonce`: the transaction leaves
/// it there, whether it touched the signer's account or not.
fn assert_nothing_kept(
    outcome: &Outcome,
    deployment: &Deployment,
    gas_limit: u64,
    signer_nonce: u64,
    case: &str,
) {
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    let nonce = outcome.state.get(&deployment.signer).map_or(signer_nonce, |a| a.info.nonce);
    assert_eq!(nonce, signer_nonce, "{case}: the signer's nonce is taken back");
    assert!(
        code_hash(outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY),
        "{case}: no code is deployed"
    );
    let [_, regular, state, history_gas, history_bytes] = beyond(outcome, &reference);
    assert_eq!([state, history_gas, history_bytes], [0; 3], "{case} at {gas_limit}");
    assert!(regular >= KEYLESS_DEPLOY_OVERHEAD_GAS, "{case}: the overhead stays spent");
    assert_eq!(outcome.usage.write_records, 0, "{case}: no record is kept");
    if gas_limit > TX_GAS_LIMIT_CAP {
        assert_eq!(
            outcome.result.gas().reservoir_remaining(),
            reference.result.gas().reservoir_remaining(),
            "{case}: the reservoir comes back",
        );
    }
}

/// A log that crosses the creation's own data-size budget reverts the creation alone: the call
/// reports `ExecutionReverted` with the stop, the transaction is not stopped, and the signer's
/// nonce is spent.
#[test]
fn test_a_deployment_crossing_its_data_size_budget_reverts_alone() {
    let deployment = Deployment::new(logging(2_000));
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(1_000),
        );
        // The call's own lane gets the frame cap, and its creation 98% of that.
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 0, limit: 980 });
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(nonce(&outcome, deployment.signer), 1);
        assert_eq!(outcome.usage.write_records, 1, "the signer's nonce record");
    }
}

/// The records the creation's start makes are held before the creation is built: a transaction
/// limit that holds one record but not two stops the creation before its init code runs. The
/// start still bumps the signer's nonce, whose record the call keeps within the limit, and the
/// call answers the stop.
#[test]
fn test_the_creations_start_crossing_the_limit_stops_the_creation() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let limit = body_bytes(&deployment) + 40;
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
        );
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 0, limit });
        assert_eq!(outcome.limit_exceeded, None, "at {gas_limit}");
        assert_eq!(nonce(&outcome, deployment.signer), 1, "at {gas_limit}");
        assert_eq!(
            outcome.usage.data_size, limit,
            "at {gas_limit}: the signer's nonce record, which meets the limit"
        );
        assert!(
            code_hash(&outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY),
            "at {gas_limit}: no code is deployed"
        );
    }
}

/// A failed creation leaves its creator's nonce record on the call's lane, which no check held it
/// to: a call whose budget cannot hold that record reverts alone with its own stop, and takes the
/// nonce back with it. The outcome names the call's budget, whose crossing produced the result of
/// the transaction's own frame.
#[test]
fn test_the_signers_record_can_put_the_call_over_its_budget() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let outcome = limited(
            &deployment,
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(39),
        );
        assert_eq!(stop(&outcome), MegaLimitExceeded { kind: 0, limit: 39 });
        assert_eq!(
            outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: 39,
                used: WRITE_RECORD_SIZE,
                frame_local: true,
            }),
            "a budget, not the transaction's limit"
        );
        assert_stopped_whole(&outcome, &deployment, gas_limit);
    }
}

/// A failed creation by a signer at nonce 1 leaves no record on the call: its nonce bump is taken
/// back, and the record with it. The same budget then holds the call, which answers with the
/// creation's own stop, and the signer stays at 1.
#[test]
fn test_a_failure_from_nonce_one_leaves_the_call_within_its_budget() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let outcome = run_with(
            system_db().account_nonce(deployment.signer, 1),
            deployment.call_data(LARGE_OVERRIDE),
            gas_limit,
            EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(39),
        );
        // The call's lane gets the frame cap, and its creation 98% of that.
        assert_eq!(creation_stop(&outcome), MegaLimitExceeded { kind: 0, limit: 38 });
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(nonce(&outcome, deployment.signer), 1, "at {gas_limit}");
        assert_eq!(outcome.usage.write_records, 0, "at {gas_limit}");
    }
}

/// A limit that holds the whole deployment lets it through.
#[test]
fn test_a_deployment_within_every_limit_deploys() {
    let deployment = Deployment::new(filling_a_slot());
    // The creation gets 98% of what the call has left, rounded down: room for its three records
    // and its byte of code.
    let limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(body_bytes(&deployment) + 3 * 40 + 1 + 10);
    for gas_limit in GAS_LIMITS {
        let outcome = limited(&deployment, gas_limit, limits);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address, "at {gas_limit}");
    }
}

/// An answer built without a frame carries the reservoir the call inherited: a call refused after
/// it paid for the signer's account — the forward capped below the signed gas limit, which only a
/// signed gas limit near the execution cap reaches above it — gives the charge back to the
/// reservoir and the sender gets all of it but the body's history.
#[test]
fn test_a_refusal_after_the_charges_gives_the_reservoir_back() {
    let deployment =
        Deployment::signed(0, TX_GAS_LIMIT_CAP - 50_000, U256::ZERO, deploying(&runtime(1)));
    let gas_limit = GAS_LIMITS[1];
    let outcome = limited(&deployment, gas_limit, EvmTxRuntimeLimits::no_limits());
    assert!(matches!(refusal(&outcome), KeylessDeployError::GasLimitTooLow { .. }));
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    let [total, regular, state, history_gas, _] = beyond(&outcome, &reference);
    assert_eq!(
        [total, regular, state, history_gas],
        [KEYLESS_DEPLOY_OVERHEAD_GAS, KEYLESS_DEPLOY_OVERHEAD_GAS, 0, 0]
    );
    assert_eq!(
        outcome.result.gas().reservoir_remaining(),
        reference.result.gas().reservoir_remaining(),
    );
}

/// And a call refused after the charges of the creation's start — the forward the `CREATE`
/// opcode's regular gas takes below the signed gas limit, which above the execution cap only a
/// signed gas limit just under what the call has left reaches — gives their state and history gas
/// back to the reservoir, and keeps their regular gas, as it keeps the overhead.
#[test]
fn test_a_refusal_after_the_creations_charges_gives_the_reservoir_back() {
    let gas_limit = GAS_LIMITS[1];
    let init_code = deploying(&runtime(1));
    // Above the cap the call has what the execution cap leaves once the body's regular gas — all
    // the reference spends — and the overhead are paid. The signed gas limit is one less, and is
    // part of the calldata the body is priced by, so the two are settled together.
    let mut signed = TX_GAS_LIMIT_CAP - 200_000;
    let (deployment, left) = (0..10)
        .find_map(|_| {
            let deployment = Deployment::signed(0, signed, U256::ZERO, init_code.clone());
            let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
            let left = TX_GAS_LIMIT_CAP - reference.gas.regular - KEYLESS_DEPLOY_OVERHEAD_GAS;
            let settled = left - 1 == signed;
            signed = left - 1;
            settled.then_some((deployment, left))
        })
        .expect("the signed gas limit settles");
    let outcome = limited(&deployment, gas_limit, EvmTxRuntimeLimits::no_limits());
    let opcode = create_regular(init_code.len());
    assert_eq!(
        refusal(&outcome),
        KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: signed,
            provided_gas_limit: left - opcode
        },
    );
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    let [total, regular, state, history_gas, _] = beyond(&outcome, &reference);
    let kept = KEYLESS_DEPLOY_OVERHEAD_GAS + opcode;
    assert_eq!([total, regular, state, history_gas], [kept, kept, 0, 0]);
    assert_eq!(
        outcome.result.gas().reservoir_remaining(),
        reference.result.gas().reservoir_remaining(),
    );
}

/// The call gets back what its creation did not spend, the same way whether the creation
/// succeeds or reverts: the transaction spends the overhead, the `CREATE` opcode's regular gas,
/// what the call kept of the creation's start, and the creation's `gasUsed`, and nothing else,
/// below and above the execution cap.
#[test]
fn test_the_unspent_forward_comes_back_on_success_and_revert_alike() {
    let succeeding = Deployment::new(deploying(&runtime(1)));
    let reverting = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    let new_account = entry(GasId::new_account_state_gas());
    for gas_limit in GAS_LIMITS {
        let success = limited(&succeeding, gas_limit, EvmTxRuntimeLimits::no_limits());
        let [total, ..] =
            beyond(&success, &reference(succeeding.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS +
                create_regular(deploying(&runtime(1)).len()) +
                new_account +
                entry(GasId::create_state_gas()) +
                2 * record() +
                returned(&success).gasUsed,
            "success at {gas_limit}",
        );

        let revert = limited(&reverting, gas_limit, EvmTxRuntimeLimits::no_limits());
        let [total, ..] =
            beyond(&revert, &reference(reverting.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            total,
            KEYLESS_DEPLOY_OVERHEAD_GAS +
                create_regular(3) +
                new_account +
                record() +
                returned(&revert).gasUsed,
            "revert at {gas_limit}",
        );
    }
}

/* ---------- the stop, as a column of the matrix ---------- */

/// A limit a deployment's creation crosses.
#[derive(Clone, Copy, Debug)]
enum Crossing {
    /// The constructor's `LOG0` of 2,000 bytes crosses the data-size limit.
    DataSize,
    /// The constructor reads the block's timestamp, then computes past the cap.
    Compute,
}

/// The cap on a read of the block environment the compute crossing runs under.
const COMPUTE_CAP: u64 = 1_000;

impl Crossing {
    const ALL: [Self; 2] = [Self::DataSize, Self::Compute];

    /// The init code whose run crosses the limit.
    fn init_code(self) -> Bytes {
        match self {
            Self::DataSize => logging(2_000),
            Self::Compute => reads_then_burns(TIMESTAMP, 1_000),
        }
    }

    /// The limits under which a deployment of `deployment`, by a signer at either nonce, crosses
    /// the limit, with the stop its creation returns: its kind, its limit, and what it counts as
    /// used where the constructor crosses.
    fn limits(self, deployment: &Deployment) -> (EvmTxRuntimeLimits, Crossed) {
        let limits = EvmTxRuntimeLimits::no_limits();
        match self {
            Self::DataSize => {
                // The body, the two records of the creation's start and 100 bytes more; the
                // constructor's log crosses with all its bytes.
                let start = body_bytes(deployment) + 2 * WRITE_RECORD_SIZE;
                let limit = start + 100;
                let used = start + LOG_BASE_SIZE + 2_000;
                (
                    limits.with_tx_data_size_limit(limit),
                    Crossed::new(LimitKind::DataSize, limit, used),
                )
            }
            Self::Compute => {
                // Read at the call's charges and the read's own two gas. The stop lands on the
                // first charge of the constructor's loop the cap leaves no room for — after the
                // read's `POP` and the push of its counter, 26 gas a round: `JUMPDEST`, `PUSH1`,
                // `SWAP1`, `SUB`, `DUP1`, the push of its start and `JUMPI` — and counts as used
                // the compute before it.
                let charges = KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(self.init_code().len());
                let limit = charges + 2 + COMPUTE_CAP;
                let left = crossing_left(COMPUTE_CAP, &[2, 3], &[1, 3, 3, 3, 3, 3, 10]);
                let limits = limits.with_block_env_access_compute_gas_limit(COMPUTE_CAP);
                (limits, Crossed::new(LimitKind::ComputeGas, limit, limit - left))
            }
        }
    }
}

/// What a crossing reports: the kind of the stop, its limit, and what it counts as used.
#[derive(Clone, Copy, Debug)]
struct Crossed {
    kind: LimitKind,
    limit: u64,
    used: u64,
}

impl Crossed {
    const fn new(kind: LimitKind, limit: u64, used: u64) -> Self {
        Self { kind, limit, used }
    }
}

/// Records the result of every frame it sees end, and rewrites nothing.
#[derive(Default)]
struct Recorder {
    /// The frames that ended, deepest first: whether each was a creation, and its result.
    ended: Vec<(bool, InstructionResult)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Recorder {
    fn call_end(&mut self, _: &mut MegaContext<DB>, _: &CallInputs, outcome: &mut CallOutcome) {
        self.ended.push((false, outcome.result.result));
    }

    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.ended.push((true, outcome.result.result));
    }
}

/// Runs a `keylessDeploy` of `deployment` over `db` at `gas_limit` under `limits`, without an
/// inspector and under a [`Recorder`]: both outcomes, and the frames the recorder saw end.
fn plain_and_recorded(
    db: MemoryDatabase,
    deployment: &Deployment,
    gas_limit: u64,
    limits: EvmTxRuntimeLimits,
) -> (Outcome, Outcome, Vec<(bool, InstructionResult)>) {
    let plain = run_with(db.clone(), deployment.call_data(LARGE_OVERRIDE), gas_limit, limits);
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .with_inspector(Recorder::default());
    let recorded = evm.execute_transaction(tx).expect("the transaction is valid");
    let ended = evm.inspector().ended.clone();
    (plain, recorded, ended)
}

/// The creation's stop, as a column of the matrix: each limit a deployment's creation can cross —
/// data size and gas detention's compute limit — crossed from a signer at nonce 0 and at nonce 1,
/// below the execution cap and above it, without an inspector and under one that records every
/// frame's end.
///
/// The creation reverts with the stop, which names the limit the constructor's crossing brings the
/// transaction past: so the crossing is the constructor's, not the creation's start or the call's
/// charges. The call answers it as `ExecutionReverted` with the stop's revert data, and keeps what
/// the creation's start wrote and nothing else of the deployment: from nonce 0 the signer's nonce,
/// spent, and its record; from nonce 1 nothing, the bump taken back. No code is deployed. A
/// compute stop bills what it counts as used past the body — the call's charges, the read, and the
/// constructor's compute up to the charge that crosses the cap — and the call charges nothing on
/// its resume. The transaction reports no stop: the result of its own frame is the call's answer.
/// The recorder sees the creation end with the stop, then the call return, and changes nothing.
#[test]
fn test_every_limit_stops_a_deployment_from_either_nonce_at_either_tier() {
    for crossing in Crossing::ALL {
        let deployment = Deployment::new(crossing.init_code());
        for signer_nonce in [0, 1] {
            let (limits, expected) = crossing.limits(&deployment);
            let Crossed { kind, limit, used } = expected;
            let db = match signer_nonce {
                0 => system_db(),
                nonce => system_db().account_nonce(deployment.signer, nonce),
            };
            for gas_limit in GAS_LIMITS {
                let row = format!("{crossing:?}, signer at {signer_nonce}, gas limit {gas_limit}");
                let (plain, recorded, ended) =
                    plain_and_recorded(db.clone(), &deployment, gas_limit, limits);
                for (column, outcome) in [("plain", &plain), ("recorded", &recorded)] {
                    let cell = format!("{row}, {column}");
                    assert_eq!(
                        creation_stop(outcome),
                        MegaLimitExceeded { kind: kind.as_u8(), limit },
                        "{cell}"
                    );
                    assert_eq!(outcome.limit_exceeded, None, "{cell}: the call caught the stop");
                    assert_eq!(nonce(outcome, deployment.signer), 1, "{cell}: the signer's nonce");
                    let records = u64::from(signer_nonce == 0);
                    assert_eq!(outcome.usage.write_records, records, "{cell}: the nonce's record");
                    assert!(
                        code_hash(outcome, deployment.address).is_none_or(|h| h == KECCAK_EMPTY),
                        "{cell}: no code is deployed"
                    );
                    if kind == LimitKind::ComputeGas {
                        let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
                        let [_, regular, ..] = beyond(outcome, &reference);
                        assert_eq!(regular, used, "{cell}: computed up to the charge that crossed");
                    } else {
                        assert!(outcome.usage.data_size <= limit, "{cell}: within the limit");
                    }
                }
                assert_eq!(
                    ended,
                    [(true, InstructionResult::Revert), (false, InstructionResult::Return)],
                    "{row}: the creation ended with the stop, then the call returned its answer",
                );
                assert_eq!(recorded.result_and_state, plain.result_and_state, "{row}");
                assert_eq!(recorded.gas, plain.gas, "{row}");
                assert_eq!(recorded.usage, plain.usage, "{row}");
            }
        }
    }
}

/// A transaction the protocol sends is held to no limit, keyless deployments included: the
/// deployment that a transaction limit stops for a user deploys for a system call.
#[test]
fn test_a_deployment_is_counted_but_not_stopped_under_the_exemption() {
    let deployment = Deployment::new(logging(2_000));
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(1);
    let mut evm = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits));
    let outcome = alloy_evm::Evm::transact_system_call(
        &mut evm,
        mega_evm::system::MEGA_SYSTEM_ADDRESS,
        KEYLESS_DEPLOY_ADDRESS,
        deployment.call_data(LARGE_OVERRIDE),
    )
    .expect("a system call runs");
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("the system call did not succeed: {:?}", outcome.result);
    };
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data()).unwrap();
    assert_eq!(ret.deployedAddress, deployment.address);
}
