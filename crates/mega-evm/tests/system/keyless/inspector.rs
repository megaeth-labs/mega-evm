//! What an inspector sees of a keyless deployment: the creation it is, from its start to its end,
//! and every step of its init code. The rewrite runs before the inspector is told the frame
//! starts, so the root of the trace is a `create` from the signer, not an opaque call.

use mega_evm::{
    system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS, test_utils::BytecodeBuilder, MegaTransaction,
};
use revm::{
    bytecode::opcode::{LOG0, PUSH0, REVERT},
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, CreateScheme, Gas, InstructionResult, Interpreter,
        InterpreterResult,
    },
    Database, Inspector,
};

use super::*;
use crate::common::context;

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

/// The `keylessDeploy` transaction of `deployment` at `gas_limit`.
fn keyless_tx(deployment: &Deployment, gas_limit: u64) -> MegaTransaction {
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    tx
}

/// Runs `deployment` under `recorder`, and returns what the recorder saw.
fn inspect(deployment: &Deployment, recorder: Recorder, gas_limit: u64) -> (Outcome, Recorder) {
    let mut evm = MegaEvm::new(context(system_db())).with_inspector(recorder);
    let outcome = evm.execute_transaction(keyless_tx(deployment, gas_limit)).expect("valid");
    let seen = Recorder {
        events: evm.inspector().events.clone(),
        answer_creations: evm.inspector().answer_creations,
    };
    (outcome, seen)
}

type Outcome = MegaTransactionOutcome;

/// The root of a keyless deployment's trace is the creation: the signer creates at its
/// Nick's-Method address, the inspector sees every step of the init code and its log, and the
/// creation ends at that address. No `call` to `KeylessDeploy` is reported.
#[test]
fn test_the_inspector_sees_the_deployment_as_a_creation() {
    let prefix =
        BytecodeBuilder::default().push_number(0_u64).push_number(0_u64).append(LOG0).build_vec();
    let init_code = constructor(&prefix, &runtime(1));
    let deployment = Deployment::new(init_code.clone());
    for gas_limit in GAS_LIMITS {
        let (outcome, recorder) = inspect(&deployment, Recorder::default(), gas_limit);
        assert_eq!(returned(&outcome).deployedAddress, deployment.address);
        let events = recorder.events;
        assert_eq!(
            events.first(),
            Some(&Event::Create(
                deployment.signer,
                CreateScheme::Custom { address: deployment.address }
            )),
        );
        assert_eq!(
            events.last(),
            Some(&Event::CreateEnd(InstructionResult::Return, Some(deployment.address))),
        );
        assert!(!events.iter().any(|event| matches!(event, Event::Call(_) | Event::CallEnd(_))));
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

/// The traced run and the plain run agree to the gas.
#[test]
fn test_an_inspected_deployment_costs_what_a_plain_one_does() {
    let deployment = Deployment::new(deploying(&runtime(3)));
    for gas_limit in GAS_LIMITS {
        let (inspected, _) = inspect(&deployment, Recorder::default(), gas_limit);
        let plain = deploy(system_db(), &deployment, gas_limit);
        assert_eq!(inspected.result, plain.result, "at {gas_limit}");
        assert_eq!(inspected.gas, plain.gas);
        assert_eq!(inspected.usage, plain.usage);
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

/// A creation whose init code reverts ends as a revert in the trace, and the call reports it.
#[test]
fn test_a_failed_deployment_ends_as_a_revert_in_the_trace() {
    for gas_limit in GAS_LIMITS {
        let deployment = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
        let (outcome, recorder) = inspect(&deployment, Recorder::default(), gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
        assert_eq!(
            recorder.events.last(),
            Some(&Event::CreateEnd(InstructionResult::Revert, Some(deployment.address))),
        );
    }
}

/// An inspector that answers the creation itself — a tool's rewrite — starts no creation: the
/// signer's nonce is not spent, so neither its account nor the created one is charged, and the
/// call reports what the inspector answered. The transaction spends the overhead and the
/// reference's gas, and nothing else.
#[test]
fn test_a_creation_an_inspector_answers_charges_nothing_it_did_not_start() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let recorder = Recorder { answer_creations: true, ..Default::default() };
        let (outcome, recorder) = inspect(&deployment, recorder, gas_limit);
        assert!(matches!(failure(&outcome), KeylessDeployError::ExecutionReverted { .. }));
        assert_eq!(nonce(&outcome, deployment.signer), 0);
        let [total, regular, state, history_gas, history_bytes] =
            beyond(&outcome, &reference(deployment.call_data(LARGE_OVERRIDE), gas_limit));
        assert_eq!(
            [total, regular, state, history_gas, history_bytes],
            [KEYLESS_DEPLOY_OVERHEAD_GAS, KEYLESS_DEPLOY_OVERHEAD_GAS, 0, 0, 0],
            "at {gas_limit}",
        );
        assert_eq!(
            recorder.events,
            [
                Event::Create(
                    deployment.signer,
                    CreateScheme::Custom { address: deployment.address }
                ),
                Event::CreateEnd(InstructionResult::Revert, None),
            ],
        );
    }
}

/// A transaction the latch stopped before its first frame is not rewritten on the inspected path
/// either, where the rewrite runs before the latch is consulted: the inspector sees the call and
/// its stop, and no creation.
#[test]
fn test_a_latched_transaction_is_seen_as_the_call_it_is() {
    let deployment = Deployment::new(deploying(&runtime(1)));
    for gas_limit in GAS_LIMITS {
        let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100);
        let mut evm = MegaEvm::new(context(system_db()).with_tx_runtime_limits(limits))
            .with_inspector(Recorder::default());
        let outcome = evm.execute_transaction(keyless_tx(&deployment, gas_limit)).expect("valid");
        assert!(outcome.limit_exceeded.is_some(), "at {gas_limit}");
        assert_eq!(
            evm.inspector().events,
            [Event::Call(KEYLESS_DEPLOY_ADDRESS), Event::CallEnd(InstructionResult::Revert)],
        );
        assert!(!outcome.state.contains_key(&deployment.signer));
    }
}
