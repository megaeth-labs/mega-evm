//! A keyless deployment under gas detention.
//!
//! The `keylessDeploy` call is the transaction's own frame and runs no code: it charges its own
//! work — the overhead, then the `CREATE` opcode's regular gas — on gas held to what the compute
//! limit leaves it, and its creation is an ordinary frame below it. So a deployment is detained
//! as any transaction is: a read of volatile data anywhere in it caps what it may still compute,
//! its charges count as compute, a crossing stops the transaction with the deployment taken back
//! whole, and the control contracts and the Oracle answer its constructor as they answer any
//! frame.
//!
//! Every case runs below the execution cap and above it ([`GAS_LIMITS`]).

use alloy_primitives::TxKind;
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    alloy_op_evm::OpTx,
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS},
    decode_volatile_data_access_disabled,
    system::{
        keyless::KEYLESS_DEPLOY_OVERHEAD_GAS, IMegaAccessControl, IMegaLimitControl, IOracle,
        ACCESS_CONTROL_ADDRESS, LIMIT_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder},
    ExternalEnvTypes, LimitCheck, LimitKind, MegaLimitExceeded, MegaTransaction,
    VolatileDataAccess,
};
use revm::{
    bytecode::opcode::{
        BALANCE, CALL, COINBASE, DUP1, GAS, JUMPDEST, JUMPI, MLOAD, POP, PUSH0, SSTORE, STATICCALL,
        STOP, SUB, SWAP1, TIMESTAMP,
    },
    context::{BlockEnv, TxEnv},
    context_interface::cfg::GasId,
    interpreter::{interpreter::EthInterpreter, CallInputs, CallOutcome, Interpreter},
    primitives::KECCAK_EMPTY,
    Inspector,
};

use super::*;
use crate::common::{call_tx, context, CALLER as RELAYER};

/// The block beneficiary of the cases where nobody in the transaction is the beneficiary.
const BENEFICIARY: Address = address!("0x0000000000000000000000000000000000beef02");

/// The cap a read of the block environment sets, by default.
const CAP: u64 = BLOCK_ENV_ACCESS_COMPUTE_GAS;

/// What a transaction did, and what gas detention made of it.
struct Run {
    outcome: MegaTransactionOutcome,
    limit: Option<u64>,
    accessed: VolatileDataAccess,
}

/// The default runtime limits, with the block-environment cap at `cap`.
fn capped(cap: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap)
}

/// A Satin context over `db`, in a block whose beneficiary is `beneficiary`, under `limits`.
fn context_in(
    db: MemoryDatabase,
    beneficiary: Address,
    limits: EvmTxRuntimeLimits,
) -> MegaContext<MemoryDatabase> {
    context(db).with_block(BlockEnv { beneficiary, ..block() }).with_tx_runtime_limits(limits)
}

/// The `keylessDeploy` transaction `data` from the relayer at `gas_limit`.
fn keyless_tx(data: Bytes, gas_limit: u64) -> MegaTransaction {
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    tx
}

/// Runs `tx` in `ctx`, through an inspector when `inspected`.
fn run_tx<E: ExternalEnvTypes>(
    ctx: MegaContext<MemoryDatabase, E>,
    tx: MegaTransaction,
    inspected: bool,
) -> Run {
    let mut evm = MegaEvm::new(ctx);
    if inspected {
        let mut evm = evm.with_inspector(revm::inspector::NoOpInspector);
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        let detention = evm.ctx().detention();
        return Run { outcome, limit: detention.compute_limit(), accessed: detention.accessed() };
    }
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let detention = evm.ctx().detention();
    Run { outcome, limit: detention.compute_limit(), accessed: detention.accessed() }
}

/// Runs the `keylessDeploy` transaction `data` from the relayer over `db` at `gas_limit`, in a
/// block whose beneficiary is `beneficiary`, under the default limits with the block-environment
/// cap at `cap`.
fn run_in(db: MemoryDatabase, beneficiary: Address, data: Bytes, gas_limit: u64, cap: u64) -> Run {
    run_tx(context_in(db, beneficiary, capped(cap)), keyless_tx(data, gas_limit), false)
}

/// Runs a `keylessDeploy` of `deployment` at `gas_limit` under `limits`, plain and inspected, and
/// asserts the two alike: the inspected path settles the deployment as the plain one does.
fn run_both(deployment: &Deployment, gas_limit: u64, limits: EvmTxRuntimeLimits) -> Run {
    let run = |inspected| {
        let ctx = context_in(db_for(deployment, U256::ZERO), BENEFICIARY, limits);
        run_tx(ctx, keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit), inspected)
    };
    let (plain, inspected) = (run(false), run(true));
    assert_eq!(inspected.outcome.result, plain.outcome.result, "at {gas_limit}");
    assert_eq!(inspected.outcome.gas, plain.outcome.gas, "at {gas_limit}");
    assert_eq!((inspected.limit, inspected.accessed), (plain.limit, plain.accessed));
    plain
}

/// Runs `init_code` as a creation transaction from the relayer at `gas_limit` under `limits`: the
/// same constructor run by the transaction's own frame, with no `keylessDeploy` call above it.
fn run_as_creation(init_code: Bytes, gas_limit: u64, limits: EvmTxRuntimeLimits) -> Run {
    let tx = OpTx(op_transaction(TxEnv {
        caller: RELAYER,
        kind: TxKind::Create,
        data: init_code,
        gas_limit,
        ..Default::default()
    }));
    run_tx(context_in(system_db(), BENEFICIARY, limits), tx, false)
}

/// Asserts `run` of `deployment` at `gas_limit` was stopped by gas detention, having computed
/// exactly its limit past its intrinsic gas, and kept nothing of the deployment.
fn assert_stopped(run: &Run, deployment: &Deployment, gas_limit: u64) {
    let limit = run.limit.expect("a read set a limit");
    let ExecutionResult::Revert { output, .. } = &run.outcome.result else {
        panic!("expected the detention stop, got {:?}", run.outcome.result);
    };
    assert_eq!(
        MegaLimitExceeded::abi_decode(output).expect("the revert data is a limit stop"),
        MegaLimitExceeded { kind: LimitKind::ComputeGas.as_u8(), limit },
    );
    assert_eq!(
        run.outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit,
            used: limit,
            frame_local: false,
        }),
    );
    let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
    assert_eq!(
        run.outcome.gas.regular,
        reference.gas.regular + limit,
        "the transaction computed up to its limit and not one unit past it, at {gas_limit}",
    );
    assert_eq!(run.outcome.gas.state, 0, "a stop keeps no state");
    assert_eq!(nonce(&run.outcome, deployment.signer), 0, "the signer's nonce is taken back");
    assert!(code_hash(&run.outcome, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
}

/// Appends `rounds` rounds of a counting loop, twenty-six gas a round, to `code`, which starts
/// the init code.
fn burn(code: BytecodeBuilder, rounds: u16) -> BytecodeBuilder {
    let code = code.push_number(rounds);
    let dest = u16::try_from(code.len()).expect("a short prefix");
    code.append(JUMPDEST)
        .push_number(1_u8)
        .append_many([SWAP1, SUB, DUP1])
        .push_number(dest)
        .append_many([JUMPI, POP])
}

/// Init code that runs `first` — a read, or a same-priced opcode that reads nothing — then
/// `rounds` rounds of [`burn`], and deploys a one-byte runtime.
pub(super) fn reads_then_burns(first: u8, rounds: u16) -> Bytes {
    let prefix = burn(BytecodeBuilder::default().append_many([first, POP]), rounds);
    constructor(&prefix.build_vec(), &runtime(1))
}

/// Appends a `STATICCALL` of `to` with `data`, all gas forwarded, keeping 32 bytes of what it
/// returns at `0x100`, and dropping its status.
fn static_calls(code: BytecodeBuilder, to: Address, data: &[u8]) -> BytecodeBuilder {
    code.mstore(0x200, data)
        .push_number(32_u8)
        .push_number(0x100_u16)
        .push_number(u8::try_from(data.len()).expect("short calldata"))
        .push_number(0x200_u16)
        .push_address(to)
        .append_many([GAS, STATICCALL, POP])
}

/// The error a deployment whose creation reverted reports, when it is a refused read of volatile
/// data: the kind it names.
fn refused_read(outcome: &MegaTransactionOutcome) -> VolatileDataAccess {
    let KeylessDeployError::ExecutionReverted { output, .. } = failure(outcome) else {
        panic!("the creation did not revert: {:?}", outcome.result);
    };
    decode_volatile_data_access_disabled(&output).expect("a refused read")
}

/* ---------- the call's own work ---------- */

/// A signer that is the block beneficiary: the call reads the beneficiary's account for its
/// rules, through the journal, and the creation runs for that account, so the deployment is
/// detained from the read, at the call's compute then — the overhead — as a sender that is the
/// beneficiary is from its start. Under the spec's cap it deploys as without the read. Under a cap
/// its `CREATE` opcode's regular gas crosses, it stops at that charge, before the creation starts.
/// A signer that is not the beneficiary is not detained.
#[test]
fn test_a_signer_that_is_the_beneficiary_detains_its_deployment() {
    let init_code = deploying(&runtime(1));
    let deployment = Deployment::new(init_code.clone());
    let data = deployment.call_data(LARGE_OVERRIDE);
    for gas_limit in GAS_LIMITS {
        let plain =
            run_in(db_for(&deployment, U256::ZERO), BENEFICIARY, data.clone(), gas_limit, 1);
        assert_eq!(returned(&plain.outcome).deployedAddress, deployment.address);
        assert_eq!((plain.limit, plain.accessed), (None, VolatileDataAccess::empty()));

        let signer = deployment.signer;
        let detained =
            run_in(db_for(&deployment, U256::ZERO), signer, data.clone(), gas_limit, CAP);
        assert_eq!(detained.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);
        assert_eq!(detained.limit, Some(KEYLESS_DEPLOY_OVERHEAD_GAS + CAP), "read at the overhead");
        assert_eq!(detained.outcome.result, plain.outcome.result, "at {gas_limit}");
        assert_eq!(detained.outcome.gas, plain.outcome.gas, "at {gas_limit}");

        let cap = create_regular(init_code.len()) - 1;
        let stopped = run_in(db_for(&deployment, U256::ZERO), signer, data.clone(), gas_limit, cap);
        assert_eq!(stopped.limit, Some(KEYLESS_DEPLOY_OVERHEAD_GAS + cap));
        assert_stopped(&stopped, &deployment, gas_limit);
    }
}

/* ---------- the creation ---------- */

/// A constructor that reads volatile data is held to the cap as any frame is. The limit is the
/// transaction's compute at the read plus the cap, and that compute holds the call's own work:
/// the limit is the same constructor's as a creation transaction's, plus the overhead and the
/// `CREATE` opcode's regular gas. Within the cap the deployment is the same as its twin's, whose
/// constructor reads nothing in the read's place (`PUSH0`, the same two gas), on every ledger.
/// Past it, the constructor stops at the limit: the call reverts with the stop, and the deployment
/// is taken back whole. Plain and inspected, below and above the execution cap.
#[test]
fn test_a_constructor_that_reads_volatile_data_is_held_to_the_cap() {
    let init_code = reads_then_burns(TIMESTAMP, 1_000);
    let (reads, twin) =
        (Deployment::new(init_code.clone()), Deployment::new(reads_then_burns(PUSH0, 1_000)));
    let charges = KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(init_code.len());
    for gas_limit in GAS_LIMITS {
        let deployed = run_both(&reads, gas_limit, capped(CAP));
        let undetained = run_both(&twin, gas_limit, capped(CAP));
        assert_eq!(returned(&deployed.outcome).deployedAddress, reads.address);
        assert_eq!(deployed.outcome.gas, undetained.outcome.gas, "as without the read");
        assert_eq!(deployed.accessed, VolatileDataAccess::TIMESTAMP);
        let as_creation = run_as_creation(init_code.clone(), gas_limit, capped(CAP));
        assert_eq!(deployed.limit, Some(as_creation.limit.unwrap() + charges), "at {gas_limit}");
        assert_eq!(deployed.limit, Some(charges + 2 + CAP), "the read's own two gas");

        let stopped = run_both(&reads, gas_limit, capped(1_000));
        assert_eq!(stopped.limit, Some(charges + 2 + 1_000));
        assert_stopped(&stopped, &reads, gas_limit);
        let twin_deploys = run_both(&twin, gas_limit, capped(1_000));
        assert_eq!(returned(&twin_deploys.outcome).deployedAddress, twin.address);
    }
}

/// A sender that is the beneficiary is detained from its start, before the call charges anything:
/// the call's work and the constructor's together are held to the cap. A cap that pays both
/// deploys as without the read; one gas less, and the constructor's last charge crosses.
#[test]
fn test_a_sender_that_is_the_beneficiary_holds_the_whole_deployment_to_the_cap() {
    let init_code = reads_then_burns(PUSH0, 100);
    let deployment = Deployment::new(init_code.clone());
    let data = deployment.call_data(LARGE_OVERRIDE);
    for gas_limit in GAS_LIMITS {
        let plain =
            run_in(db_for(&deployment, U256::ZERO), BENEFICIARY, data.clone(), gas_limit, 1);
        let intrinsic = reference(data.clone(), gas_limit).gas.regular;
        let compute = plain.outcome.gas.regular - intrinsic;
        assert!(compute > KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(init_code.len()));

        let fits =
            run_in(db_for(&deployment, U256::ZERO), RELAYER, data.clone(), gas_limit, compute);
        assert_eq!(fits.limit, Some(compute), "detained from its start");
        assert_eq!(fits.outcome.result, plain.outcome.result, "at {gas_limit}");
        assert_eq!(fits.outcome.gas, plain.outcome.gas, "at {gas_limit}");
        let short =
            run_in(db_for(&deployment, U256::ZERO), RELAYER, data.clone(), gas_limit, compute - 1);
        assert_stopped(&short, &deployment, gas_limit);
    }
}

/// A sender that is the beneficiary, under a cap below the overhead: the call is held to the cap
/// before it charges anything, so the overhead is the charge that crosses, and the transaction
/// stops at the cap having computed exactly the cap. A call held only once its charges were paid
/// would pay the overhead and the `CREATE` opcode's regular gas in full, past the cap, and stop
/// at its creation's first charge instead. Plain and inspected.
#[test]
fn test_a_sender_that_is_the_beneficiary_is_held_before_the_overhead() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let data = deployment.call_data(LARGE_OVERRIDE);
    let cap = KEYLESS_DEPLOY_OVERHEAD_GAS - 1;
    for gas_limit in GAS_LIMITS {
        for inspected in [false, true] {
            let ctx = context_in(db_for(&deployment, U256::ZERO), RELAYER, capped(cap));
            let run = run_tx(ctx, keyless_tx(data.clone(), gas_limit), inspected);
            assert_eq!(run.limit, Some(cap), "detained from its start, at no compute");
            assert_stopped(&run, &deployment, gas_limit);
        }
    }
}

/* ---------- the control contracts ---------- */

/// A deployment in a subtree where volatile-data access is switched off keeps the refusal. With
/// the switch off from the transaction's own frame — the keyless call — or from the creation, the
/// constructor's `TIMESTAMP` is refused: the creation reverts with
/// `VolatileDataAccessDisabled`, which the call reports as `ExecutionReverted`, having read and
/// capped nothing, and the signer's nonce is spent as by any failed deployment. Off only from
/// below the creation, the constructor reads.
#[test]
fn test_a_deployment_in_a_subtree_with_volatile_access_off_is_refused_its_reads() {
    let deployment = Deployment::new(reads_then_burns(TIMESTAMP, 1));
    for gas_limit in GAS_LIMITS {
        for from in [0, 1, 2] {
            let ctx = context_in(db_for(&deployment, U256::ZERO), BENEFICIARY, capped(CAP))
                .with_volatile_access_disabled_from(from);
            let tx = keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit);
            let run = run_tx(ctx, tx, false);
            if from == 2 {
                assert_eq!(returned(&run.outcome).deployedAddress, deployment.address);
                assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP);
                continue;
            }
            assert_eq!(
                refused_read(&run.outcome),
                VolatileDataAccess::TIMESTAMP,
                "off from {from}"
            );
            assert_eq!((run.limit, run.accessed), (None, VolatileDataAccess::empty()));
            assert_eq!(nonce(&run.outcome, deployment.signer), 1);
        }
    }
}

/// A constructor that switches volatile-data access off for itself is refused its own read after
/// it, as any frame is.
#[test]
fn test_a_constructor_that_switches_volatile_access_off_is_refused_its_own_read() {
    let disable = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let prefix = BytecodeBuilder::default()
        .mstore(0x200, disable)
        .append_many([PUSH0, PUSH0])
        .push_number(4_u8)
        .push_number(0x200_u16)
        .append(PUSH0)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append_many([GAS, CALL, POP, TIMESTAMP, POP]);
    let deployment = Deployment::new(constructor(&prefix.build_vec(), &runtime(1)));
    for gas_limit in GAS_LIMITS {
        let run = run_both(&deployment, gas_limit, capped(CAP));
        assert_eq!(refused_read(&run.outcome), VolatileDataAccess::TIMESTAMP);
        assert_eq!((run.limit, run.accessed), (None, VolatileDataAccess::empty()));
    }
}

/// Records every answer `remainingComputeGas()` gives, next to the spendable regular gas its
/// caller resumes with: what a regular charge of the caller could draw at its next instruction.
#[derive(Debug, Default)]
struct AnswerAndResume {
    pending: Option<u64>,
    pairs: Vec<(u64, u64)>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for AnswerAndResume {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if let Some(answer) = self.pending.take() {
            self.pairs.push((answer, interp.gas.tracker().spendable()));
        }
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == LIMIT_CONTROL_ADDRESS && outcome.result.is_ok() {
            self.pending = Some(U256::from_be_slice(&outcome.result.output).to::<u64>());
        }
    }
}

/// `remainingComputeGas()` called from a constructor answers what the constructor could still
/// spend: exactly the spendable regular gas it resumes with. Undetained, that is its own gas. A
/// constructor that read the block environment hears what the cap leaves it after the read. From
/// a sender that is the beneficiary, detained from its start, it hears the cap less all the
/// transaction computed — the call's overhead and `CREATE` opcode's regular gas included.
#[test]
fn test_remaining_compute_gas_answers_a_constructor_its_allowance() {
    let query = IMegaLimitControl::remainingComputeGasCall::SELECTOR;
    let init_code = |first: u8| {
        let prefix = static_calls(
            BytecodeBuilder::default().append_many([first, POP]),
            LIMIT_CONTROL_ADDRESS,
            &query,
        );
        constructor(&prefix.build_vec(), &runtime(1))
    };
    for gas_limit in GAS_LIMITS {
        for (first, sender_is_beneficiary) in [(PUSH0, false), (TIMESTAMP, false), (PUSH0, true)] {
            let init_code = init_code(first);
            let deployment = Deployment::new(init_code.clone());
            let beneficiary = if sender_is_beneficiary { RELAYER } else { BENEFICIARY };
            let ctx = context_in(db_for(&deployment, U256::ZERO), beneficiary, capped(CAP));
            let mut evm = MegaEvm::new(ctx).with_inspector(AnswerAndResume::default());
            let outcome = evm
                .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
                .expect("the transaction is valid");
            assert_eq!(returned(&outcome).deployedAddress, deployment.address);
            let pairs = evm.inspector().pairs.clone();
            let [(answer, resumes)] = pairs[..] else { panic!("one answer: {pairs:?}") };
            assert_eq!(answer, resumes, "{first} {sender_is_beneficiary} at {gas_limit}");
            let charges = KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(init_code.len());
            match (first, sender_is_beneficiary) {
                (TIMESTAMP, _) => assert!(answer < CAP && answer > CAP - 5_000, "{answer}"),
                (_, true) => {
                    assert!(answer < CAP - charges && answer > CAP - charges - 5_000, "{answer}")
                }
                _ => assert!(answer > CAP, "its own gas: {answer}"),
            }
        }
    }
}

/* ---------- the Oracle ---------- */

/// A context over `db` whose oracle service answers `envs`, under the default limits with the
/// Oracle's cap at `oracle_cap`.
fn oracle_context(
    db: MemoryDatabase,
    envs: TestExternalEnvs<String>,
    oracle_cap: u64,
) -> MegaContext<MemoryDatabase, TestExternalEnvs<String>> {
    MegaContext::<_, TestExternalEnvs<String>>::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs { salt_env: envs.clone(), oracle_env: envs },
    )
    .with_block(BlockEnv { beneficiary: BENEFICIARY, ..block() })
    .with_chain(zero_fee_l1_block_info())
    .with_tx_runtime_limits(
        EvmTxRuntimeLimits::default().with_oracle_access_compute_gas_limit(oracle_cap),
    )
}

/// Init code that reads the Oracle's slot 0 through `getSlot`, keeps what it read in its own slot
/// 0, runs `rounds` rounds of [`burn`] and deploys a one-byte runtime.
fn reads_the_oracle(rounds: u16) -> Bytes {
    let get_slot = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let prefix = static_calls(BytecodeBuilder::default(), ORACLE_CONTRACT_ADDRESS, &get_slot)
        .push_number(0x100_u16)
        .append_many([MLOAD, PUSH0, SSTORE]);
    constructor(&burn(prefix, rounds).build_vec(), &runtime(1))
}

/// A constructor reads the Oracle's storage as any frame does: it gets the service's value when
/// the service answers and the chain's otherwise, at the same price either way, and the read is
/// volatile, under the Oracle's cap. Past the cap, the constructor stops at the limit, and the
/// deployment is taken back whole.
#[test]
fn test_a_constructor_reads_the_oracle_as_any_frame_does() {
    let deployment = Deployment::new(reads_the_oracle(1_000));
    let db = || {
        db_for(&deployment, U256::ZERO).account_storage(
            ORACLE_CONTRACT_ADDRESS,
            U256::ZERO,
            U256::from(5),
        )
    };
    let service = TestExternalEnvs::new().with_oracle_storage(U256::ZERO, U256::from(7));
    let stored = |run: &Run| {
        run.outcome.state[&deployment.address]
            .storage
            .get(&U256::ZERO)
            .map(|slot| slot.present_value)
    };
    for gas_limit in GAS_LIMITS {
        let tx = || keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit);
        let answered =
            run_tx(oracle_context(db(), service.clone(), ORACLE_ACCESS_COMPUTE_GAS), tx(), false);
        let from_state = run_tx(
            oracle_context(db(), TestExternalEnvs::new(), ORACLE_ACCESS_COMPUTE_GAS),
            tx(),
            false,
        );
        assert_eq!(returned(&answered.outcome).deployedAddress, deployment.address);
        assert_eq!(
            (stored(&answered), stored(&from_state)),
            (Some(U256::from(7)), Some(U256::from(5)))
        );
        assert_eq!(answered.outcome.gas, from_state.outcome.gas, "priced alike, at {gas_limit}");
        assert_eq!(answered.accessed, VolatileDataAccess::ORACLE);
        assert!(answered.limit.is_some());

        let stopped = run_tx(oracle_context(db(), service.clone(), 1_000), tx(), true);
        assert_stopped(&stopped, &deployment, gas_limit);
        assert_eq!(stopped.accessed, VolatileDataAccess::ORACLE);
    }
}

/* ---------- what a deployment reports it read ---------- */

/// The volatile data a constructor reads is the transaction's: the block environment, the
/// coinbase — which is not a read of the beneficiary's account — the beneficiary's balance, and
/// the Oracle's storage, each recorded as the kind it is, and each capping the transaction.
#[test]
fn test_a_constructors_reads_are_the_transactions() {
    let balance_of_coinbase = BytecodeBuilder::default().append_many([COINBASE, BALANCE, POP]);
    let cases = [
        (reads_then_burns(TIMESTAMP, 1), VolatileDataAccess::TIMESTAMP),
        (reads_then_burns(COINBASE, 1), VolatileDataAccess::COINBASE),
        (
            constructor(&balance_of_coinbase.build_vec(), &runtime(1)),
            VolatileDataAccess::COINBASE | VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (reads_the_oracle(1), VolatileDataAccess::ORACLE),
    ];
    for (init_code, read) in cases {
        let deployment = Deployment::new(init_code);
        for gas_limit in GAS_LIMITS {
            let run = run_both(&deployment, gas_limit, capped(CAP));
            assert_eq!(returned(&run.outcome).deployedAddress, deployment.address);
            assert_eq!(run.accessed, read, "at {gas_limit}");
            assert!(run.limit.is_some(), "the read caps the transaction");
        }
    }
}

/// A deployment that fails still keeps the reads its creation made: a constructor that reads the
/// timestamp and deploys no code fails with `EmptyCodeDeployed`, and the read stands.
#[test]
fn test_a_failed_deployment_keeps_the_reads_its_creation_made() {
    let deployment =
        Deployment::new(BytecodeBuilder::default().append_many([TIMESTAMP, POP, STOP]).build());
    for gas_limit in GAS_LIMITS {
        let run = run_both(&deployment, gas_limit, capped(CAP));
        assert!(
            matches!(failure(&run.outcome), KeylessDeployError::EmptyCodeDeployed { .. }),
            "at {gas_limit}"
        );
        assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP);
        assert!(run.limit.is_some());
    }
}

/// A deployment another limit stops still reports the reads its creation made, and the stop stays
/// that limit's: a constructor that reads the timestamp and fills a slot the state-gas limit has no
/// room for stops the transaction on state growth, not on compute.
#[test]
fn test_a_deployment_another_limit_stops_keeps_the_reads_its_creation_made() {
    let prefix =
        BytecodeBuilder::default().append_many([TIMESTAMP, POP]).sstore(U256::ZERO, U256::ONE);
    let deployment = Deployment::new(constructor(&prefix.build_vec(), &runtime(1)));
    let upfront = entry(GasId::new_account_state_gas()) + entry(GasId::create_state_gas());
    let limit = upfront + entry(GasId::sstore_set_state_gas()) - 1;
    for gas_limit in GAS_LIMITS {
        let run = run_both(&deployment, gas_limit, capped(CAP).with_tx_state_gas_limit(limit));
        let ExecutionResult::Revert { output, .. } = &run.outcome.result else {
            panic!("expected the state-gas stop, got {:?}", run.outcome.result);
        };
        assert_eq!(
            MegaLimitExceeded::abi_decode(output).unwrap(),
            MegaLimitExceeded { kind: LimitKind::StateGrowth.as_u8(), limit },
        );
        assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP);
        assert!(run.limit.is_some());
        assert_eq!(nonce(&run.outcome, deployment.signer), 0, "the deployment is taken back");
    }
}
