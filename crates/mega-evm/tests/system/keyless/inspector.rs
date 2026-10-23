//! What an inspector sees of a keyless deployment: the frames it is made of. The transaction calls
//! `KeylessDeploy`, and that call creates: the inspector is told the call starts, then the
//! creation, one journal depth below it, then every step of the init code, the creation's end and
//! the call's, each with the gas and the output it had.

use alloy_sol_types::SolError;
use mega_evm::{
    system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS, test_utils::BytecodeBuilder, EvmTxRuntimeLimits,
    LimitKind, MegaLimitExceeded, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{LOG0, PUSH0, REVERT},
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, CreateScheme, Gas, InstructionResult, Interpreter,
        InterpreterResult, InterpreterTypes,
    },
    primitives::KECCAK_EMPTY,
    Database, Inspector,
};
use revm_inspectors::tracing::{types::CallKind, TracingInspector, TracingInspectorConfig};

use super::*;
use crate::common::{context, keyless_tx, CALLER};

/// What an inspector was told, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Call(Address),
    CallEnd(InstructionResult),
    Create(Address, CreateScheme),
    CreateEnd(InstructionResult, Option<Address>),
    Step(u8),
    Log(Address),
}

/// Records every frame start and end, every step and every log.
#[derive(Default)]
struct Recorder {
    events: Vec<Event>,
    /// When set, answers every creation with a revert instead of letting it run.
    answer_creations: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Recorder {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.events.push(Event::Step(interp.bytecode.opcode()));
    }

    fn log(&mut self, _: &mut MegaContext<DB>, log: alloy_primitives::Log) {
        self.events.push(Event::Log(log.address));
    }

    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.events.push(Event::Call(inputs.target_address));
        None
    }

    fn call_end(&mut self, _: &mut MegaContext<DB>, _: &CallInputs, outcome: &mut CallOutcome) {
        self.events.push(Event::CallEnd(outcome.result.result));
    }

    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.events.push(Event::Create(inputs.caller(), inputs.scheme()));
        self.answer_creations.then(|| CreateOutcome {
            result: InterpreterResult::new(
                InstructionResult::Revert,
                Bytes::new(),
                Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit(), inputs.reservoir()),
            ),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
            charged_state_gas_address: inputs.charged_state_gas_address(),
            charged_start: Default::default(),
        })
    }

    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.events.push(Event::CreateEnd(outcome.result.result, outcome.address));
    }
}

/// Runs `deployment` under `recorder`, and returns what the recorder saw.
fn inspect(deployment: &Deployment, recorder: Recorder, gas_limit: u64) -> (Outcome, Recorder) {
    let mut evm = MegaEvm::new(context(system_db())).with_inspector(recorder);
    let outcome = evm
        .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
        .expect("valid");
    let seen = Recorder {
        events: evm.inspector().events.clone(),
        answer_creations: evm.inspector().answer_creations,
    };
    (outcome, seen)
}

type Outcome = MegaTransactionOutcome;

/// The inspector sees the call to `KeylessDeploy` start, then the creation it starts — the signer
/// creating at its Nick's-Method address — every step of the init code and its log, the creation
/// ending at that address, and last the call ending with the answer it gave.
#[test]
fn test_the_inspector_sees_the_call_and_the_creation_it_starts() {
    let prefix =
        BytecodeBuilder::default().push_number(0_u64).push_number(0_u64).append(LOG0).build_vec();
    let init_code = constructor(&prefix, &runtime(1));
    let deployment = Deployment::new(init_code.clone());
    for gas_limit in GAS_LIMITS {
        let (outcome, recorder) = inspect(&deployment, Recorder::default(), gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let events = recorder.events;
        assert_eq!(
            events[..2],
            [
                Event::Call(KEYLESS_DEPLOY_ADDRESS),
                Event::Create(
                    deployment.signer,
                    CreateScheme::Custom { address: deployment.address }
                ),
            ],
        );
        assert_eq!(
            events[events.len() - 2..],
            [
                Event::CreateEnd(InstructionResult::Return, Some(deployment.address)),
                Event::CallEnd(InstructionResult::Return),
            ],
        );
        let steps: Vec<u8> = events
            .iter()
            .filter_map(|event| match event {
                Event::Step(opcode) => Some(*opcode),
                _ => None,
            })
            .collect();
        assert_eq!(steps.len(), 10, "every instruction of the init code: {steps:02x?}");
        assert_eq!(steps[0], init_code[0]);
        assert!(steps.contains(&LOG0));
        assert!(events.contains(&Event::Log(deployment.address)));
    }
}

/// A call the rules refuse is reported as the call it is, its start and end paired around the
/// refusal; no creation starts.
#[test]
fn test_a_refused_call_is_seen_as_a_call() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::with_value(deploying(&runtime(1)), U256::from(1));
        let (outcome, recorder) = inspect(&deployment, Recorder::default(), gas_limit);
        assert_eq!(refusal(&outcome), KeylessDeployError::InsufficientBalance);
        assert_eq!(
            recorder.events,
            [Event::Call(KEYLESS_DEPLOY_ADDRESS), Event::CallEnd(InstructionResult::Revert)],
        );
    }
}

/// A creation whose init code reverts ends as a revert in the trace, and the call, which reports
/// the failure in its return, ends as a success.
#[test]
fn test_a_failed_deployment_ends_as_a_revert_in_the_trace() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
        let (outcome, recorder) = inspect(&deployment, Recorder::default(), gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
        let events = recorder.events;
        assert_eq!(
            events[events.len() - 2..],
            [
                Event::CreateEnd(InstructionResult::Revert, Some(deployment.address)),
                Event::CallEnd(InstructionResult::Return),
            ],
        );
    }
}

/// An inspector that answers the creation itself — a tool's rewrite — starts no creation: the
/// signer's nonce is not spent, and the call reports what the inspector answered. The answer is
/// taken as given, as revm takes it: what the call was charged for the creation's start stays
/// charged — the history of the signer's nonce record and of the created account's record — and
/// no record is counted, the signer's nonce never having been bumped. The transaction spends the
/// overhead, the regular gas the `CREATE` opcode charges whatever its frame does, those two
/// records' history, and the reference's gas.
#[test]
fn test_a_creation_an_inspector_answers_keeps_what_its_start_was_charged() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let opcode = KEYLESS_DEPLOY_OVERHEAD_GAS + create_regular(deploying(&runtime(1)).len());
    for gas_limit in GAS_LIMITS {
        let recorder = Recorder { answer_creations: true, ..Default::default() };
        let (outcome, recorder) = inspect(&deployment, recorder, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
        assert_eq!(nonce(&outcome, deployment.signer), 0);
        let [total, regular, state, history_gas, history_bytes] =
            beyond(&outcome, &reference(deployment.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            [total, regular, state, history_gas, history_bytes],
            [opcode + 2 * record(), opcode, 0, 2 * record(), 0],
            "at {gas_limit}",
        );
        assert_eq!(
            recorder.events,
            [
                Event::Call(KEYLESS_DEPLOY_ADDRESS),
                Event::Create(
                    deployment.signer,
                    CreateScheme::Custom { address: deployment.address }
                ),
                Event::CreateEnd(InstructionResult::Revert, None),
                Event::CallEnd(InstructionResult::Return),
            ],
        );
    }
}

/// Answers every creation with a revert on the gas it was given, carrying back the created
/// account's charge: a tool's rewrite, for any context.
struct AnswersCreations;

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for AnswersCreations {
    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        Some(CreateOutcome {
            result: InterpreterResult::new(
                InstructionResult::Revert,
                Bytes::new(),
                Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit(), inputs.reservoir()),
            ),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
            charged_state_gas_address: inputs.charged_state_gas_address(),
            charged_start: Default::default(),
        })
    }
}

/// The signer's account a creation's start never created, because an inspector answered the
/// creation, comes back at the price it was charged, however crowded the signer's bucket: priced
/// again at the signer's account, whose bucket the transaction reads once. With the created
/// account's charge, which the answer carries back, the deployment keeps no state gas.
#[test]
fn test_an_answered_creation_gives_the_signers_account_back_at_its_price() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if crate::common::state_is_free() {
        return;
    }
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        for m in [1, 2, 8] {
            let envs = crowded(TestExternalEnvs::new(), deployment.signer, m);
            let outcome = MegaEvm::new(salt_context(system_db(), envs.clone()))
                .with_inspector(AnswersCreations)
                .execute_transaction(salt_tx(&deployment, gas_limit))
                .expect("valid");
            let case = format!("m = {m}, gas limit {gas_limit}");
            assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
            assert_eq!(nonce(&outcome, deployment.signer), 0, "{case}: no creation started");
            assert_eq!(outcome.gas.state, 0, "{case}: both charges came back");
            assert_eq!(
                envs.bucket_queries(bucket(deployment.signer)),
                1,
                "{case}: one read priced the charge and its give-back",
            );
        }
    }
}

/// A transaction whose body crosses the data-size limit is rejected at validation on the inspected
/// path too: the inspector sees no frame, neither the call nor a creation.
#[test]
fn test_a_rejected_transaction_is_seen_as_nothing() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100);
        let mut evm = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits))
            .with_inspector(Recorder::default());
        let error = evm
            .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
            .expect_err("rejected at validation");
        let error = format!("{error:?}");
        assert!(error.contains(mega_evm::TX_BODY_OVER_DATA_SIZE_LIMIT), "at {gas_limit}: {error}");
        assert_eq!(evm.inspector().events, [], "at {gas_limit}");
    }
}

/// A tracer sees the frames a keyless deployment is made of: at the root, at journal depth 0, the
/// transaction's call to `KeylessDeploy`, which returns the ABI answer; as its one child, one
/// depth below, the creation from the signer at the deploy address, which returns the runtime it
/// deploys. Each reports the gas it took from its gas limit — the call what the transaction spent
/// beyond its intrinsic gas, the creation its `gasUsed` — less, above the execution cap, the state
/// and history gas the reservoir paid, as for any frame. The parity and geth call traces built
/// from it say the same, and no trace has a frame of its own.
#[test]
fn test_a_tracer_sees_the_call_and_the_creation_it_starts() {
    let deployment = Deployment::new(deploying(&runtime(3)));
    for gas_limit in GAS_LIMITS {
        let tracer = TracingInspector::new(TracingInspectorConfig::default_parity());
        let mut evm = MegaEvm::new(context(system_db())).with_inspector(tracer);
        let outcome = evm
            .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
            .expect("valid");
        let answer = returned(&outcome);
        assert_eq!(answer.deployedAddress, deployment.address);
        let tracer = evm.inspector().clone();

        let [root, child] = tracer.traces().nodes() else {
            panic!("expected two frames: {:#?}", tracer.traces().nodes());
        };
        let reference = reference(deployment.call_data(LARGE_OVERRIDE), gas_limit);
        let [spent, regular, ..] = beyond(&outcome, &reference);
        let deposit = satin_gas_params().code_deposit_state_gas(3) + history(3);
        let (call_took, creation_took) = if gas_limit > TX_GAS_LIMIT_CAP {
            (regular, answer.gasUsed - deposit)
        } else {
            (spent, answer.gasUsed)
        };
        assert_eq!(
            (root.trace.kind, root.trace.depth, root.trace.caller, root.trace.address),
            (CallKind::Call, 0, CALLER, KEYLESS_DEPLOY_ADDRESS),
        );
        assert_eq!((root.parent, root.children.as_slice()), (None, &[1][..]));
        assert_eq!(root.trace.status, Some(InstructionResult::Return));
        assert_eq!(Some(&root.trace.output), outcome.result.output());
        assert_eq!(root.trace.gas_used, call_took, "at {gas_limit}");
        assert_eq!(
            (child.trace.kind, child.trace.depth, child.trace.caller, child.trace.address),
            (CallKind::Create, 1, deployment.signer, deployment.address),
        );
        assert_eq!(child.parent, Some(0));
        assert_eq!(child.trace.status, Some(InstructionResult::Return));
        assert_eq!(child.trace.output, Bytes::from(runtime(3)));
        assert_eq!(child.trace.gas_used, creation_took, "at {gas_limit}");

        let parity =
            serde_json::to_value(tracer.clone().into_parity_builder().into_transaction_traces())
                .unwrap();
        let [call, create] = parity.as_array().expect("a list of traces").as_slice() else {
            panic!("expected two parity traces: {parity:#}");
        };
        assert_eq!(call["type"], "call", "{parity:#}");
        assert_eq!(call["action"]["from"], json_address(CALLER));
        assert_eq!(call["action"]["to"], json_address(KEYLESS_DEPLOY_ADDRESS));
        assert_eq!(call["traceAddress"], serde_json::json!([]));
        assert_eq!(call["subtraces"], 1);
        assert_eq!(create["type"], "create", "{parity:#}");
        assert_eq!(create["action"]["from"], json_address(deployment.signer));
        assert_eq!(create["result"]["address"], json_address(deployment.address));
        assert_eq!(create["traceAddress"], serde_json::json!([0]));

        let geth = serde_json::to_value(
            tracer
                .into_geth_builder()
                .geth_call_traces(Default::default(), outcome.result.gas().tx_gas_used()),
        )
        .unwrap();
        assert_eq!(geth["type"], "CALL", "{geth:#}");
        assert_eq!(geth["from"], json_address(CALLER));
        assert_eq!(geth["to"], json_address(KEYLESS_DEPLOY_ADDRESS));
        let [create] = geth["calls"].as_array().expect("the call's children").as_slice() else {
            panic!("expected one child: {geth:#}");
        };
        assert_eq!(create["type"], "CREATE", "{geth:#}");
        assert_eq!(create["from"], json_address(deployment.signer));
        assert_eq!(create["to"], json_address(deployment.address));
        assert!(create.get("calls").is_none(), "{geth:#}");
    }
}

/// `address` as a trace's JSON holds it.
fn json_address(address: Address) -> serde_json::Value {
    serde_json::to_value(address).unwrap()
}

/// Revives every creation that failed, as Foundry's `expectRevert` does: the result becomes a
/// success at `0x…01`, which is no address the deployment pinned.
struct Revives;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Revives {
    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if !outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Return;
            outcome.address = Some(Address::with_last_byte(1));
        }
    }
}

/// A deployment whose creation fails and is revived by an inspector reports a creation at an
/// address the deployment did not pin: the call reverts with `AddressMismatch()`, which a plain
/// run never produces — it answers `ExecutionReverted`. The call's revert takes the whole
/// deployment back, the signer's nonce bump included, which the plain run keeps.
#[test]
fn test_a_revived_keyless_creation_answers_address_mismatch() {
    let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    for gas_limit in GAS_LIMITS {
        let plain = MegaEvm::new(context(system_db()))
            .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit));
        let plain = plain.expect("valid");
        assert!(matches!(failure(&plain), KeylessDeployError::ExecutionReverted { .. }));

        let mut evm = MegaEvm::new(context(system_db())).with_inspector(Revives);
        let revived = evm
            .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
            .expect("valid");
        assert_eq!(refusal(&revived), KeylessDeployError::AddressMismatch, "at {gas_limit}");
        assert_eq!(nonce(&plain, deployment.signer), 1, "the plain run spends the nonce");
        assert_eq!(nonce(&revived, deployment.signer), 0, "the reverted call takes the bump back");
        assert!(code_hash(&revived, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
    }
}

/// A creation a transaction-level limit stopped is a failed creation like any other, so a revived
/// one is answered as the reverted one above: the constructor's `LOG0` of 2,000 bytes crosses the
/// transaction's data-size limit, which holds the body, the start's two records and 100 bytes
/// more. Without the inspector the call answers the stop as `ExecutionReverted` and keeps the
/// signer's nonce; with it, the creation is revived at an address the deployment did not pin, and
/// the call reverts with `AddressMismatch()`, taking the bump back, and reports no stop.
#[test]
fn test_a_revived_keyless_creation_a_limit_stopped_answers_address_mismatch() {
    let prefix = BytecodeBuilder::default().push_number(2_000_u64).push_number(0_u64).append(LOG0);
    let deployment = Deployment::new(constructor(&prefix.build_vec(), &runtime(1)));
    let body = reference(deployment.call_data(LARGE_OVERRIDE), GAS_LIMITS[0]).usage.data_size;
    let limit = body + 2 * WRITE_RECORD_SIZE + 100;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
    for gas_limit in GAS_LIMITS {
        let plain = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits))
            .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
            .expect("valid");
        let KeylessDeployError::ExecutionReverted { output, .. } = failure(&plain) else {
            panic!("the creation did not revert: {:?}", plain.result);
        };
        // The creation's budget: 98% of what the call has left, which the log crosses with the
        // transaction's limit.
        let budget = (limit - body) * 98 / 100;
        assert_eq!(
            MegaLimitExceeded::abi_decode(&output).expect("a limit stop"),
            MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: budget },
            "at {gas_limit}: the constructor's log crossed the creation's budget"
        );
        assert_eq!(nonce(&plain, deployment.signer), 1, "the plain run spends the nonce");

        let mut evm = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits))
            .with_inspector(Revives);
        let revived = evm
            .execute_transaction(keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit))
            .expect("valid");
        assert_eq!(refusal(&revived), KeylessDeployError::AddressMismatch, "at {gas_limit}");
        assert_eq!(revived.limit_exceeded, None, "at {gas_limit}: no stop is the call's result");
        assert_eq!(nonce(&revived, deployment.signer), 0, "the reverted call takes the bump back");
        assert!(code_hash(&revived, deployment.address).is_none_or(|hash| hash == KECCAK_EMPTY));
    }
}

/// An inspector sees the creation of a deployment that returned empty code end as a revert, the
/// outcome of the refused deposit, and the call answers `EmptyCodeDeployed`.
#[test]
fn test_an_inspector_sees_an_empty_code_deployment_end_as_a_revert() {
    let deployment = Deployment::new(Bytes::new());
    for gas_limit in GAS_LIMITS {
        let (outcome, recorder) = inspect(&deployment, Recorder::default(), gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::EmptyCodeDeployed { .. }));
        let ends: Vec<_> =
            recorder.events.iter().filter(|event| matches!(event, Event::CreateEnd(..))).collect();
        let end = Event::CreateEnd(InstructionResult::Revert, Some(deployment.address));
        assert_eq!(ends, [&end], "at {gas_limit}");
    }
}

/// Halts the `keylessDeploy` call's frame whenever it is handed to the inspector, in
/// `initialize_interp` or in a `step`, and counts the times it was.
#[derive(Default)]
struct HaltsTheCall {
    handed: usize,
}

impl HaltsTheCall {
    fn halt(&mut self, interp: &mut Interpreter<EthInterpreter>) {
        if interp.input.target_address == KEYLESS_DEPLOY_ADDRESS {
            self.handed += 1;
            interp.halt(InstructionResult::OutOfGas);
        }
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for HaltsTheCall {
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _: &mut MegaContext<DB>,
    ) {
        self.halt(interp);
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.halt(interp);
    }
}

/// The actions the engine sets on the call's frame — the creation it starts, the answer once the
/// creation returned, and the stop of a call its creation's nonce record left over the frame's
/// budget — are the frame's own: revm hands a frame built with an action to no
/// `initialize_interp`, and returns an action set before a run without a `step`. So an inspector
/// that would halt the call's frame there is never handed it, and the run is the plain run, in a
/// debug build and a release build alike.
#[test]
fn test_an_inspector_that_would_halt_the_calls_frame_is_never_handed_it() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    let over_the_budget = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(39);
    for gas_limit in GAS_LIMITS {
        for limits in [None, Some(over_the_budget)] {
            let case = format!("{limits:?} at {gas_limit}");
            let context = || match limits {
                Some(limits) => context(system_db()).with_tx_runtime_limits(limits),
                None => context(system_db()),
            };
            let tx = keyless_tx(deployment.call_data(LARGE_OVERRIDE), gas_limit);
            let plain = MegaEvm::new(context()).execute_transaction(tx.clone()).expect("valid");
            if limits.is_none() {
                assert_eq!(returned(&plain).deployedAddress, deployment.address, "{case}");
            } else {
                // The call's first action started its creation, which returned into it, leaving
                // the call over its budget: the call returns the stop on its resume.
                assert!(plain.limit_exceeded.is_some(), "{case}: the call returns the stop");
                let mut evm = MegaEvm::new(context()).with_inspector(Recorder::default());
                evm.execute_transaction(tx.clone()).expect("valid");
                let events = &evm.inspector().events;
                let started = events.iter().any(|event| matches!(event, Event::Create(..)));
                let returned = events.iter().any(|event| matches!(event, Event::CreateEnd(..)));
                assert!(started && returned, "{case}: the creation started and returned");
            }
            let mut evm = MegaEvm::new(context()).with_inspector(HaltsTheCall::default());
            let halted = evm.execute_transaction(tx).expect("valid");
            assert_eq!(evm.inspector().handed, 0, "{case}: the call's frame is never handed over");
            assert_eq!(halted.result_and_state, plain.result_and_state, "{case}");
            assert_eq!(halted.gas, plain.gas, "{case}");
            assert_eq!(halted.usage, plain.usage, "{case}");
            assert_eq!(halted.limit_exceeded, plain.limit_exceeded, "{case}");
        }
    }
}
