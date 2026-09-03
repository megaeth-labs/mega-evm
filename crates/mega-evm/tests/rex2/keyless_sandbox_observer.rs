//! Sandbox observer: parity with the no-observer path, event-stream contracts,
//! and HRTB adapter coverage for generic inspectors.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{hex, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants,
    revm::context::result::{ExecutionResult, ResultAndState},
    sandbox::{
        decode_error_result, KeylessDeployError, SandboxCompletionKind, SandboxEndOutcome,
        SandboxObserver, SandboxRejectKind, SandboxStartInfo,
    },
    test_utils::{ErrorInjectingDatabase, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use revm::{
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
    },
    Inspector,
};

use super::keyless_sandbox_support::{
    assert_result_and_state_eq, assert_usage_eq, constructor_calls_identity_and_stores_return,
    constructor_calls_identity_precompile, constructor_calls_reverter,
    constructor_touches_sentinel, create_pre_eip155_deploy_tx,
    create_pre_eip155_deploy_tx_with_value, crowded_parent_env, empty_code_constructor, funded_db,
    keyless_deploy_call_tx, keyless_deploy_call_tx_with_override_u256, parent_compute_gas_used,
    revert_constructor, run_keyless, run_keyless_with_parent_env,
    run_keyless_with_parent_env_usage, run_keyless_with_usage, selfdestructing_constructor,
    split_create_initcode, success_constructor, RunConfig, DEFAULT_OUTER_GAS_LIMIT, IDENTITY_INPUT,
    IDENTITY_OVERRIDE, IDENTITY_PRECOMPILE, LARGE_GAS_LIMIT_OVERRIDE, MERGE_FAIL_SENTINEL,
    REVERTER, SIGNED_TX_GAS_LIMIT, SPECS, SPLIT_CREATE_CODE_LEN, SPLIT_CREATE_SLOT,
    SPLIT_CREATE_SLOT_VALUE,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum ObservedEvent {
    Start {
        spec: MegaSpecId,
        signer: Address,
        deploy_address: Address,
        gas_limit_override: u64,
        effective_gas_limit: u64,
        tx_gas_limit: u64,
    },
    End(SandboxEndOutcome),
    InitializeInterp,
    Step(u8),
    StepEnd(u8),
    Call {
        target: Address,
    },
    CallEnd {
        target: Address,
    },
    Create,
    CreateEnd,
    Log,
    Selfdestruct,
}

#[derive(Default)]
struct RecordingObserver {
    events: Vec<ObservedEvent>,
}

impl<E: mega_evm::ExternalEnvTypes> SandboxObserver<E> for RecordingObserver {
    fn initialize_interp(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        self.events.push(ObservedEvent::InitializeInterp);
    }

    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        self.events.push(ObservedEvent::Step(interp.bytecode.opcode()));
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        self.events.push(ObservedEvent::StepEnd(interp.bytecode.opcode()));
    }

    fn call(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
    ) {
        self.events.push(ObservedEvent::Call { target: inputs.target_address });
    }

    fn call_end(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
        _outcome: &CallOutcome,
    ) {
        self.events.push(ObservedEvent::CallEnd { target: inputs.target_address });
    }

    fn create(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &CreateInputs,
    ) {
        self.events.push(ObservedEvent::Create);
    }

    fn create_end(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &CreateInputs,
        _outcome: &CreateOutcome,
    ) {
        self.events.push(ObservedEvent::CreateEnd);
    }

    fn log(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _log: alloy_primitives::Log,
    ) {
        self.events.push(ObservedEvent::Log);
    }

    fn selfdestruct(&mut self, _contract: Address, _target: Address, _value: U256) {
        self.events.push(ObservedEvent::Selfdestruct);
    }

    fn sandbox_start(&mut self, info: &SandboxStartInfo) {
        self.events.push(ObservedEvent::Start {
            spec: info.spec,
            signer: info.signer,
            deploy_address: info.deploy_address,
            gas_limit_override: info.gas_limit_override,
            effective_gas_limit: info.effective_gas_limit,
            tx_gas_limit: info.tx_gas_limit,
        });
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.events.push(ObservedEvent::End(outcome.clone()));
    }
}

#[derive(Clone, Default)]
struct GenericCreateCounter {
    creates: Rc<RefCell<usize>>,
}

impl<CTX> Inspector<CTX> for GenericCreateCounter {
    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        *self.creates.borrow_mut() += 1;
        None
    }
}

/// Loud split-CREATE shape: constructor finished (SSTORE stuck, account created) but the
/// sandbox reported `ExecutionFailed` and the ABI does not claim a deploy. An empty spin
/// that actually deployed would fail here as `Applied(Deployed)` / empty `errorData`.
fn assert_split_create_shape(
    spec: MegaSpecId,
    result: &ResultAndState<MegaHaltReason>,
    signer: Address,
    events: Option<&[ObservedEvent]>,
) {
    if let Some(events) = events {
        assert_single_start_end_pair(events);
        match start_and_end(events).1 {
            ObservedEvent::End(SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::ExecutionFailed,
                ..
            }) => {}
            other => panic!("{spec:?}: expected Applied(ExecutionFailed), got {other:?}"),
        }
    }

    assert!(
        result.result.is_success(),
        "{spec:?}: split CREATE is a success-style outer return: {:?}",
        result.result
    );
    let output = result.result.output().expect("split CREATE outer output");
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output)
        .expect("split CREATE ABI return");
    assert_eq!(
        ret.deployedAddress,
        Address::ZERO,
        "{spec:?}: split CREATE must not report a deployed address"
    );
    assert!(
        !ret.errorData.is_empty(),
        "{spec:?}: split CREATE must carry errorData (empty spin succeeds with empty errorData)"
    );
    let error = decode_error_result(&ret.errorData);
    assert!(
        matches!(
            error,
            Some(
                KeylessDeployError::ExecutionHalted { .. } |
                    KeylessDeployError::ExecutionReverted { .. }
            )
        ),
        "{spec:?}: split CREATE errorData must be halt/revert, got {error:?}"
    );

    let deploy_address = signer.create(0);
    let account = result.state.get(&deploy_address).unwrap_or_else(|| {
        panic!("{spec:?}: split CREATE must leave a state entry at {deploy_address}")
    });
    assert!(
        account.is_created(),
        "{spec:?}: deploy address must be is_created(); status={:?}",
        account.status
    );
    let slot = account.storage.get(&U256::from(SPLIT_CREATE_SLOT)).map(|slot| slot.present_value());
    assert_eq!(
        slot,
        Some(U256::from(SPLIT_CREATE_SLOT_VALUE)),
        "{spec:?}: constructor SSTORE must survive the failed code deposit"
    );

    // Pre-REX5 `return_create` commits the runtime blob before the code-deposit compute
    // charge marks the frame as exceeding, so the account still carries the 8_000-byte
    // code. The ABI `deployedAddress == 0` above is the "not deployed" signal.
    // Length and hash are checked independently: a non-empty deposit of the wrong size
    // (e.g. 7999) must fail rather than pass via the hash arm of a disjunction.
    let code_len = account.info.code.as_ref().map(|c| c.len()).unwrap_or(0);
    assert_eq!(
        code_len, SPLIT_CREATE_CODE_LEN as usize,
        "{spec:?}: pre-REX5 split commits 8000-byte returned bytecode; code_len={code_len}"
    );
    assert_ne!(
        account.info.code_hash,
        revm::primitives::KECCAK_EMPTY,
        "{spec:?}: pre-REX5 split commits non-empty code_hash; hash={:?}",
        account.info.code_hash
    );
}

fn parity_pair(
    spec: MegaSpecId,
    init_code: Bytes,
    fund_signer: bool,
    tx_limits: Option<EvmTxRuntimeLimits>,
    case: &str,
) {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
    let mut db_obs = if fund_signer { funded_db(signer) } else { MemoryDatabase::default() };
    let mut db_base = db_obs.clone();
    let observer = Rc::new(RefCell::new(RecordingObserver::default()));

    let (observed, observed_usage) = run_keyless_with_usage(RunConfig {
        spec,
        db: &mut db_obs,
        tx_bytes: tx_bytes.clone(),
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: Some(observer),
        tx_limits,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
        spec,
        db: &mut db_base,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: None::<Rc<RefCell<RecordingObserver>>>,
        tx_limits,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    assert_result_and_state_eq(&observed, &baseline, case);
    assert_usage_eq(observed_usage, baseline_usage, case);
}

fn run_with_recorder(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    tx_limits: Option<EvmTxRuntimeLimits>,
) -> (ResultAndState<MegaHaltReason>, Vec<ObservedEvent>) {
    let recorder = Rc::new(RefCell::new(RecordingObserver::default()));
    let observer = Rc::clone(&recorder);
    let result = run_keyless(RunConfig {
        spec,
        db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: Some(observer),
        tx_limits,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    let events = recorder.borrow().events.clone();
    (result, events)
}

fn start_and_end(events: &[ObservedEvent]) -> (&ObservedEvent, &ObservedEvent) {
    let start =
        events.iter().find(|e| matches!(e, ObservedEvent::Start { .. })).expect("sandbox_start");
    let end =
        events.iter().rev().find(|e| matches!(e, ObservedEvent::End(_))).expect("sandbox_end");
    (start, end)
}

fn assert_single_start_end_pair(events: &[ObservedEvent]) {
    let starts = events.iter().filter(|e| matches!(e, ObservedEvent::Start { .. })).count();
    let ends = events.iter().filter(|e| matches!(e, ObservedEvent::End(_))).count();
    assert_eq!(starts, 1, "sandbox_start once, got {starts}");
    assert_eq!(ends, 1, "sandbox_end once, got {ends}");
    let start_idx = events.iter().position(|e| matches!(e, ObservedEvent::Start { .. })).unwrap();
    let end_idx = events.iter().position(|e| matches!(e, ObservedEvent::End(_))).unwrap();
    assert!(start_idx < end_idx, "sandbox_start before sandbox_end");
}

fn assert_call_create_balanced(events: &[ObservedEvent]) {
    let mut call_depth = 0isize;
    let mut create_depth = 0isize;
    for event in events {
        match event {
            ObservedEvent::Call { .. } => call_depth += 1,
            ObservedEvent::CallEnd { .. } => call_depth -= 1,
            ObservedEvent::Create => create_depth += 1,
            ObservedEvent::CreateEnd => create_depth -= 1,
            _ => {}
        }
        assert!(call_depth >= 0, "call_end without call: {events:?}");
        assert!(create_depth >= 0, "create_end without create: {events:?}");
    }
    assert_eq!(call_depth, 0, "unbalanced call frames: {events:?}");
    assert_eq!(create_depth, 0, "unbalanced create frames: {events:?}");
}

#[test]
fn test_observer_parity_pre_rex4_with_nonempty_parent_env() {
    for spec in [MegaSpecId::REX2, MegaSpecId::REX3] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db_obs = funded_db(signer);
        let mut db_base = db_obs.clone();
        let observer = Rc::new(RefCell::new(RecordingObserver::default()));
        let env = crowded_parent_env();

        let (observed, observed_usage) = run_keyless_with_parent_env_usage(
            spec,
            &mut db_obs,
            tx_bytes.clone(),
            env.clone(),
            Some(observer),
        );
        let (baseline, baseline_usage) = run_keyless_with_parent_env_usage(
            spec,
            &mut db_base,
            tx_bytes,
            env,
            None::<Rc<RefCell<RecordingObserver>>>,
        );

        assert!(
            baseline.result.is_success(),
            "{spec:?} baseline must succeed: {:?}",
            baseline.result
        );
        let case = format!("pre-REX4 nonempty parent env {spec:?}");
        assert_result_and_state_eq(&observed, &baseline, &case);
        assert_usage_eq(observed_usage, baseline_usage, &case);
    }
}

#[test]
fn test_observer_parity_pre_rex4_with_nonempty_parent_env_on_constructor_revert() {
    for spec in [MegaSpecId::REX2, MegaSpecId::REX3] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(revert_constructor());
        let mut db_obs = funded_db(signer);
        let mut db_base = db_obs.clone();
        let observer = Rc::new(RefCell::new(RecordingObserver::default()));
        let env = crowded_parent_env();

        let (observed, observed_usage) = run_keyless_with_parent_env_usage(
            spec,
            &mut db_obs,
            tx_bytes.clone(),
            env.clone(),
            Some(observer),
        );
        let (baseline, baseline_usage) = run_keyless_with_parent_env_usage(
            spec,
            &mut db_base,
            tx_bytes,
            env,
            None::<Rc<RefCell<RecordingObserver>>>,
        );

        assert!(
            baseline.result.is_success(),
            "{spec:?} constructor revert is a success-style outer return: {:?}",
            baseline.result
        );
        let case = format!("pre-REX4 nonempty parent env revert {spec:?}");
        assert_result_and_state_eq(&observed, &baseline, &case);
        assert_usage_eq(observed_usage, baseline_usage, &case);
    }
}

#[test]
fn test_opcode_events_flow_on_pre_rex4_with_nonempty_parent_env() {
    for spec in [MegaSpecId::REX2, MegaSpecId::REX3] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let recorder = Rc::new(RefCell::new(RecordingObserver::default()));
        let result = run_keyless_with_parent_env(
            spec,
            &mut db,
            tx_bytes,
            crowded_parent_env(),
            Some(Rc::clone(&recorder)),
        );
        let events = recorder.borrow().events.clone();

        assert!(result.result.is_success(), "{spec:?} should succeed: {:?}", result.result);
        assert_single_start_end_pair(&events);
        assert_call_create_balanced(&events);
        assert!(
            events.iter().any(|e| matches!(e, ObservedEvent::Create)),
            "{spec:?}: pre-REX4 nonempty parent env must emit CREATE: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, ObservedEvent::Step(_))),
            "{spec:?}: pre-REX4 nonempty parent env must emit opcode steps: {events:?}"
        );
    }
}

#[test]
fn test_observer_parity_success_across_specs() {
    for spec in SPECS {
        parity_pair(spec, success_constructor(), true, None, &format!("success {spec:?}"));
    }
}

#[test]
fn test_observer_parity_constructor_revert_across_specs() {
    for spec in SPECS {
        parity_pair(spec, revert_constructor(), true, None, &format!("revert {spec:?}"));
    }
}

#[test]
fn test_observer_parity_rex5_resource_limit_halt() {
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let used = parent_compute_gas_used(spec, signer, tx_bytes);
        assert!(used > 1, "baseline compute gas");
        let limits =
            EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(used.saturating_sub(1));
        parity_pair(
            spec,
            success_constructor(),
            true,
            Some(limits),
            &format!("resource halt {spec:?}"),
        );
    }
}

#[test]
fn test_sandbox_start_end_pair_on_successful_deploy() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let deploy_address = signer.create(0);
        let mut db = funded_db(signer);
        let (result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);

        assert!(result.result.is_success(), "{spec:?} should succeed: {:?}", result.result);
        assert_single_start_end_pair(&events);
        assert_call_create_balanced(&events);

        let (start, end) = start_and_end(&events);
        match start {
            ObservedEvent::Start {
                spec: start_spec,
                signer: start_signer,
                deploy_address: start_deploy,
                gas_limit_override,
                effective_gas_limit,
                tx_gas_limit,
            } => {
                assert_eq!(*start_spec, spec);
                assert_eq!(*start_signer, signer);
                assert_eq!(*start_deploy, deploy_address);
                assert_eq!(*gas_limit_override, LARGE_GAS_LIMIT_OVERRIDE);
                assert_eq!(*effective_gas_limit, LARGE_GAS_LIMIT_OVERRIDE);
                assert_eq!(*tx_gas_limit, SIGNED_TX_GAS_LIMIT);
            }
            other => panic!("expected Start, got {other:?}"),
        }
        match end {
            ObservedEvent::End(SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::Deployed,
                gas_used,
            }) => {
                assert!(*gas_used > 0, "{spec:?} applied gas");
            }
            other => panic!("{spec:?}: expected Applied(Deployed), got {other:?}"),
        }
    }
}

#[test]
fn test_sandbox_end_applied_execution_failed_on_constructor_revert() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(revert_constructor());
        let mut db = funded_db(signer);
        let (result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);
        assert!(result.result.is_success(), "{spec:?} outer success-style revert");
        assert_single_start_end_pair(&events);
        match start_and_end(&events).1 {
            ObservedEvent::End(SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::ExecutionFailed,
                ..
            }) => {}
            other => panic!("{spec:?}: expected Applied(ExecutionFailed), got {other:?}"),
        }
    }
}

/// An empty-code deployment is reported as `EmptyCode` on every spec, while the outer
/// caller's wire shape is the frozen one: `EmptyCodeDeployed` errorData everywhere, with the
/// constructor's logs forwarded only from REX5 on.
#[test]
fn test_sandbox_end_applied_empty_code_on_every_spec() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(empty_code_constructor());
        let mut db = funded_db(signer);
        let (result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);
        assert!(result.result.is_success(), "{spec:?} empty-code outer success");
        match start_and_end(&events).1 {
            ObservedEvent::End(SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::EmptyCode,
                ..
            }) => {}
            other => panic!("{spec:?}: expected Applied(EmptyCode), got {other:?}"),
        }
        let output = result.result.output().expect("empty-code outer output");
        let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output)
            .expect("empty-code ABI return");
        assert_eq!(ret.deployedAddress, Address::ZERO, "{spec:?}: nothing deployed");
        assert!(
            matches!(
                decode_error_result(&ret.errorData),
                Some(KeylessDeployError::EmptyCodeDeployed { .. })
            ),
            "{spec:?}: the wire shape stays EmptyCodeDeployed"
        );
    }
}

/// Records, per env impl, which hooks a dual-impl observer received.
#[derive(Default)]
struct SplitImplObserver {
    /// Events seen by the `EmptyExternalEnv` impl.
    empty: Vec<&'static str>,
    /// Events seen by the parent-env impl.
    parent: Vec<&'static str>,
}

impl SandboxObserver<mega_evm::EmptyExternalEnv> for SplitImplObserver {
    fn step(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<
            mega_evm::sandbox::SandboxDb<'_>,
            mega_evm::EmptyExternalEnv,
        >,
    ) {
        if self.empty.last() != Some(&"step") {
            self.empty.push("step");
        }
    }

    fn sandbox_start(&mut self, _info: &SandboxStartInfo) {
        self.empty.push("start");
    }

    fn sandbox_end(&mut self, _outcome: &SandboxEndOutcome) {
        self.empty.push("end");
    }
}

impl SandboxObserver<mega_evm::TestExternalEnvs> for SplitImplObserver {
    fn step(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<
            mega_evm::sandbox::SandboxDb<'_>,
            mega_evm::TestExternalEnvs,
        >,
    ) {
        if self.parent.last() != Some(&"step") {
            self.parent.push("step");
        }
    }

    fn sandbox_start(&mut self, _info: &SandboxStartInfo) {
        self.parent.push("start");
    }

    fn sandbox_end(&mut self, _outcome: &SandboxEndOutcome) {
        self.parent.push("end");
    }
}

/// With a non-empty parent env, an observer with one impl per env sees a sandbox's
/// lifecycle and opcode hooks on the same impl: `EmptyExternalEnv` pre-REX4, the parent env
/// from REX4 on.
#[test]
fn test_lifecycle_and_opcode_hooks_land_on_the_same_env_impl() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let observer = Rc::new(RefCell::new(SplitImplObserver::default()));
        let result = run_keyless_with_parent_env(
            spec,
            &mut db,
            tx_bytes,
            crowded_parent_env(),
            Some(Rc::clone(&observer)),
        );
        assert!(result.result.is_success(), "{spec:?}: {:?}", result.result);
        let observer = observer.borrow();
        let (used, idle, label) = if spec.is_enabled(MegaSpecId::REX4) {
            (&observer.parent, &observer.empty, "parent env")
        } else {
            (&observer.empty, &observer.parent, "EmptyExternalEnv")
        };
        assert_eq!(used, &["start", "step", "end"], "{spec:?}: whole sandbox on the {label} impl");
        assert!(idle.is_empty(), "{spec:?}: the other impl sees nothing, got {idle:?}");
    }
}

#[test]
fn test_sandbox_end_not_applied_rejected_on_insufficient_balance() {
    for spec in SPECS {
        let (tx_bytes, _signer) =
            create_pre_eip155_deploy_tx_with_value(success_constructor(), U256::from(1));
        let mut db = MemoryDatabase::default();
        let (result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);
        assert!(
            matches!(result.result, ExecutionResult::Revert { .. }),
            "{spec:?} insufficient balance should revert the outer call: {:?}",
            result.result
        );
        assert_single_start_end_pair(&events);
        match start_and_end(&events).1 {
            ObservedEvent::End(SandboxEndOutcome::NotApplied {
                reason: SandboxRejectKind::Rejected,
            }) => {}
            other => panic!("{spec:?}: expected NotApplied(Rejected), got {other:?}"),
        }
        let error = match &result.result {
            ExecutionResult::Revert { output, .. } => decode_error_result(output),
            other => panic!("expected revert, got {other:?}"),
        };
        assert!(
            matches!(error, Some(mega_evm::sandbox::KeylessDeployError::InsufficientBalance)),
            "{spec:?}: expected InsufficientBalance, got {error:?}"
        );
    }
}

#[test]
fn test_sandbox_end_not_applied_post_accounting_halt_on_rex5() {
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let used = parent_compute_gas_used(spec, signer, tx_bytes.clone());
        let limits =
            EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(used.saturating_sub(1));

        let mut db = funded_db(signer);
        let (result, events) = run_with_recorder(spec, &mut db, tx_bytes, Some(limits));
        assert!(result.result.is_halt(), "{spec:?} residual overflow should halt");
        assert_single_start_end_pair(&events);
        match start_and_end(&events).1 {
            ObservedEvent::End(SandboxEndOutcome::NotApplied {
                reason: SandboxRejectKind::PostAccountingHalt,
            }) => {}
            other => panic!("{spec:?}: expected PostAccountingHalt, got {other:?}"),
        }
    }
}

#[test]
fn test_reverted_subframe_events_still_stream() {
    let spec = MegaSpecId::REX5;
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_calls_reverter());
    let mut db = funded_db(signer);
    db.set_account_code(REVERTER, Bytes::from_static(&hex!("60006000fd")));
    let (_result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);

    assert_single_start_end_pair(&events);
    assert_call_create_balanced(&events);
    assert!(
        events.iter().any(|e| matches!(e, ObservedEvent::Call { target } if *target == REVERTER)),
        "reverted call must still emit call: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ObservedEvent::CallEnd { target } if *target == REVERTER)),
        "reverted call must still emit call_end: {events:?}"
    );
}

#[test]
fn test_short_circuit_precompile_call_is_balanced() {
    for spec in SPECS {
        let (tx_bytes, signer) =
            create_pre_eip155_deploy_tx(constructor_calls_identity_precompile());
        let mut db = funded_db(signer);
        let (_result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);
        assert_call_create_balanced(&events);
        assert!(
            events.iter().any(|e| {
                matches!(e, ObservedEvent::Call { target } if *target == IDENTITY_PRECOMPILE)
            }),
            "{spec:?}: missing identity precompile call"
        );
        assert!(
            events.iter().any(|e| {
                matches!(e, ObservedEvent::CallEnd { target } if *target == IDENTITY_PRECOMPILE)
            }),
            "{spec:?}: missing identity precompile call_end"
        );
    }
}

#[test]
fn test_event_order_create_wraps_steps() {
    let spec = MegaSpecId::REX5;
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let (_result, events) = run_with_recorder(spec, &mut db, tx_bytes, None);

    let create = events.iter().position(|e| matches!(e, ObservedEvent::Create)).unwrap();
    let create_end = events.iter().position(|e| matches!(e, ObservedEvent::CreateEnd)).unwrap();
    let first_step = events.iter().position(|e| matches!(e, ObservedEvent::Step(_))).unwrap();
    let last_step_end =
        events.iter().rposition(|e| matches!(e, ObservedEvent::StepEnd(_))).unwrap();
    assert!(create < first_step, "create before steps");
    assert!(last_step_end < create_end, "step_end before create_end");
}

/// A revm inspector that answers every identity-precompile CALL with a fixed return.
///
/// Attached through the read-only channel it must have no effect: the blanket
/// `SandboxObserver` impl hands it a copy of the inputs and discards the override. The
/// same type attached through the rewriting channel is the control arm proving the
/// override is real.
struct OverridingInspector;

impl<CTX> Inspector<CTX> for OverridingInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (inputs.target_address == IDENTITY_PRECOMPILE).then(|| {
            CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    Bytes::from(IDENTITY_OVERRIDE.to_be_bytes::<32>()),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

fn slot_zero(result: &ResultAndState<MegaHaltReason>, addr: Address) -> Option<U256> {
    result
        .state
        .get(&addr)
        .and_then(|account| account.storage.get(&U256::ZERO))
        .map(|slot| slot.present_value())
}

/// The read-only channel structurally drops `call` overrides: an inspector that short-circuits
/// the identity precompile changes nothing when attached as an observer (result, state, and
/// usage match the no-hook run and slot 0 holds the precompile's echo), while the same
/// inspector on the rewriting channel lands its override in slot 0.
#[test]
fn test_observer_channel_drops_call_overrides() {
    let (tx_bytes, signer) =
        create_pre_eip155_deploy_tx(constructor_calls_identity_and_stores_return());
    let deploy_address = signer.create(0);

    for spec in SPECS {
        let mut db_base = funded_db(signer);
        let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
            spec,
            db: &mut db_base,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: None::<Rc<RefCell<OverridingInspector>>>,
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_eq!(
            slot_zero(&baseline, deploy_address),
            Some(IDENTITY_INPUT),
            "{spec:?}: the real precompile echoes the input"
        );

        let mut db_obs = funded_db(signer);
        let (observed, observed_usage) = run_keyless_with_usage(RunConfig {
            spec,
            db: &mut db_obs,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: Some(Rc::new(RefCell::new(OverridingInspector))),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_result_and_state_eq(&observed, &baseline, &format!("{spec:?} observer override"));
        assert_usage_eq(observed_usage, baseline_usage, &format!("{spec:?} observer override"));

        let mut db_insp = funded_db(signer);
        let mut context = MegaContext::new(&mut db_insp, spec);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context.set_keyless_sandbox_inspector(Rc::new(RefCell::new(OverridingInspector)));
        let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
        let tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
        let rewritten = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("inspector channel");
        assert_eq!(
            slot_zero(&rewritten, deploy_address),
            Some(IDENTITY_OVERRIDE),
            "{spec:?}: the rewriting channel lands the same inspector's override"
        );
    }
}

#[test]
fn test_blanket_observer_records_sandbox_create_for_generic_inspector() {
    let tracer = GenericCreateCounter::default();
    let creates = Rc::clone(&tracer.creates);
    let observer = Rc::new(RefCell::new(tracer));

    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let result = run_keyless(RunConfig {
        spec: MegaSpecId::REX5,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: Some(observer),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    assert!(result.result.is_success(), "deploy should succeed: {:?}", result.result);
    assert!(
        *creates.borrow() >= 1,
        "generic inspector must see the sandbox CREATE frame, got {}",
        creates.borrow()
    );
}

#[test]
fn test_no_observer_skips_lifecycle_hooks() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let result = run_keyless(RunConfig {
        spec: MegaSpecId::REX5,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: None::<Rc<RefCell<RecordingObserver>>>,
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    assert!(result.result.is_success(), "default path must still succeed");
}

#[test]
fn test_sandbox_start_info_rex5_caps_effective_gas_limit_below_override() {
    const OVERRIDE: u64 = LARGE_GAS_LIMIT_OVERRIDE;
    const OUTER_GAS: u64 = 5_000_000;

    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let recorder = Rc::new(RefCell::new(RecordingObserver::default()));
        let result = run_keyless(RunConfig {
            spec,
            db: &mut db,
            tx_bytes,
            gas_limit_override: OVERRIDE,
            observer: Some(Rc::clone(&recorder)),
            tx_limits: None,
            outer_gas_limit: OUTER_GAS,
        });
        assert!(
            result.result.is_success(),
            "{spec:?} capped deploy should succeed: {:?}",
            result.result
        );

        let events = recorder.borrow().events.clone();
        assert_single_start_end_pair(&events);
        match start_and_end(&events).0 {
            ObservedEvent::Start {
                gas_limit_override, effective_gas_limit, tx_gas_limit, ..
            } => {
                assert_eq!(*gas_limit_override, OVERRIDE, "{spec:?}: decoded override");
                assert_eq!(*tx_gas_limit, SIGNED_TX_GAS_LIMIT);
                assert!(
                    *effective_gas_limit < OVERRIDE,
                    "{spec:?}: effective_gas_limit ({effective_gas_limit}) must be capped below override ({OVERRIDE})"
                );
                assert!(
                    *effective_gas_limit >= SIGNED_TX_GAS_LIMIT,
                    "{spec:?}: effective_gas_limit ({effective_gas_limit}) must still cover the signed tx gas limit"
                );
                assert!(
                    *effective_gas_limit + constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS
                        <= OUTER_GAS,
                    "{spec:?}: effective_gas_limit plus dispatch overhead must fit in the outer envelope"
                );
            }
            other => panic!("{spec:?}: expected Start, got {other:?}"),
        }
    }
}

#[test]
fn test_observer_parity_split_create_through_interceptor() {
    // Parent 1M compute is not forwarded into pre-REX5 sandboxes; the sandbox
    // runs at the 200M spec default. Do not set a parent limit — the initcode
    // itself must overflow that 200M default. EVM gas and compute gas are
    // independent: the 10B override / 1T outer envelope cover ~199M memory
    // expansion plus 80M code-deposit storage gas.
    for spec in [MegaSpecId::REX2, MegaSpecId::REX3, MegaSpecId::REX4] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(split_create_initcode());
        let mut db_obs = funded_db(signer);
        let mut db_base = db_obs.clone();
        let recorder = Rc::new(RefCell::new(RecordingObserver::default()));

        let (observed, observed_usage) = run_keyless_with_usage(RunConfig {
            spec,
            db: &mut db_obs,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: Some(Rc::clone(&recorder)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
            spec,
            db: &mut db_base,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: None::<Rc<RefCell<RecordingObserver>>>,
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });

        let events = recorder.borrow().events.clone();
        assert_split_create_shape(spec, &observed, signer, Some(&events));
        assert_split_create_shape(spec, &baseline, signer, None);
        let case = format!("split-CREATE interceptor {spec:?}");
        assert_result_and_state_eq(&observed, &baseline, &case);
        assert_usage_eq(observed_usage, baseline_usage, &case);
    }
}

#[test]
fn test_sandbox_start_info_saturates_gas_limit_override_above_u64_max() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let recorder = Rc::new(RefCell::new(RecordingObserver::default()));

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_observer(Rc::clone(&recorder));
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx =
        keyless_deploy_call_tx_with_override_u256(tx_bytes, U256::MAX, DEFAULT_OUTER_GAS_LIMIT);
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact");
    assert!(result.result.is_success(), "deploy should succeed: {:?}", result.result);

    let events = recorder.borrow().events.clone();
    assert_single_start_end_pair(&events);
    match start_and_end(&events).0 {
        ObservedEvent::Start { gas_limit_override, .. } => {
            assert_eq!(
                *gas_limit_override,
                u64::MAX,
                "payload > u64::MAX must saturate gas_limit_override to u64::MAX"
            );
        }
        other => panic!("expected Start, got {other:?}"),
    }
}

#[test]
fn test_sandbox_end_not_applied_apply_failed_on_merge_db_error() {
    let spec = MegaSpecId::REX5;
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_touches_sentinel());
    let mut inner = funded_db(signer);
    inner.set_account_balance(MERGE_FAIL_SENTINEL, U256::from(1));

    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(MERGE_FAIL_SENTINEL);
    // Occupancy / sandbox execution load the sentinel once; merge inspects it again.
    db.fail_on_account_skip = 1;

    let recorder = Rc::new(RefCell::new(RecordingObserver::default()));
    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_observer(Rc::clone(&recorder));
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("outer transact");
    let events = recorder.borrow().events.clone();

    assert!(
        matches!(result.result, ExecutionResult::Revert { .. }),
        "merge DB error must revert the outer call: {:?}",
        result.result
    );
    let error = match &result.result {
        ExecutionResult::Revert { output, .. } => decode_error_result(output),
        other => panic!("expected revert, got {other:?}"),
    };
    assert!(
        matches!(error, Some(mega_evm::sandbox::KeylessDeployError::InternalError)),
        "apply failure surfaces as InternalError, got {error:?}"
    );
    assert_single_start_end_pair(&events);
    match start_and_end(&events).1 {
        ObservedEvent::End(SandboxEndOutcome::NotApplied {
            reason: SandboxRejectKind::ApplyFailed,
        }) => {}
        other => panic!("expected NotApplied(ApplyFailed), got {other:?}"),
    }
}

/// An observer that overrides nothing: every hook is the trait's default.
struct DefaultsOnlyObserver;

impl<E: mega_evm::ExternalEnvTypes> SandboxObserver<E> for DefaultsOnlyObserver {}

/// The trait's default hook bodies are inert: with a defaults-only observer attached, a
/// deployment that logs, calls, creates, and self-destructs inside the sandbox ends with the
/// same result, state, and resource usage as the no-hook run.
#[test]
fn test_defaults_only_observer_is_inert() {
    let shapes: [(&str, Bytes); 2] = [
        ("deep mixed", mega_evm::test_utils::deep_mixed_init(REVERTER)),
        ("selfdestruct", selfdestructing_constructor()),
    ];
    for spec in SPECS {
        for (name, init_code) in &shapes {
            let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code.clone());
            let mut db_base = funded_db(signer);
            db_base.set_account_code(
                REVERTER,
                Bytes::from_static(&mega_evm::test_utils::REVERTING_RUNTIME),
            );
            let mut db_obs = db_base.clone();

            let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
                spec,
                db: &mut db_base,
                tx_bytes: tx_bytes.clone(),
                gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
                observer: None::<Rc<RefCell<DefaultsOnlyObserver>>>,
                tx_limits: None,
                outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
            });
            let (observed, observed_usage) = run_keyless_with_usage(RunConfig {
                spec,
                db: &mut db_obs,
                tx_bytes,
                gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
                observer: Some(Rc::new(RefCell::new(DefaultsOnlyObserver))),
                tx_limits: None,
                outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
            });
            let case = format!("defaults-only observer {name} {spec:?}");
            assert!(baseline.result.is_success(), "{case}: {:?}", baseline.result);
            assert_result_and_state_eq(&observed, &baseline, &case);
            assert_usage_eq(observed_usage, baseline_usage, &case);
        }
    }
}
