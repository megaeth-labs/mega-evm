//! Sandbox observer: parity with the no-observer path, event-stream contracts,
//! and HRTB adapter coverage for generic inspectors.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::{ExecutionResult, ResultAndState},
    sandbox::{
        decode_error_result, InspectorSandboxObserver, SandboxCompletionKind, SandboxEndOutcome,
        SandboxObserver, SandboxRejectKind, SandboxStartInfo,
    },
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, POP, PUSH0, RETURN, STATICCALL},
    context::TxEnv,
    handler::EvmTr,
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Interpreter,
    },
    Inspector,
};

const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;
const LARGE_SIGNER_BALANCE: u128 = 1_000_000_000_000_000_000_000;
const SIGNED_TX_GAS_LIMIT: u64 = 1_000_000;
const REVERTER: Address = address!("0000000000000000000000000000000000aaaaaa");
const IDENTITY_PRECOMPILE: Address = address!("0000000000000000000000000000000000000004");

const SPECS: [MegaSpecId; 5] =
    [MegaSpecId::REX2, MegaSpecId::REX3, MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

#[derive(Clone, Debug, PartialEq, Eq)]
enum ObservedEvent {
    Start {
        spec: MegaSpecId,
        signer: Address,
        deploy_address: Address,
        gas_limit_override: u64,
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
        inputs: &mut CallInputs,
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
        _inputs: &mut CreateInputs,
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

fn success_constructor() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1))
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

fn revert_constructor() -> Bytes {
    Bytes::from_static(&hex!("60006000fd"))
}

fn empty_code_constructor() -> Bytes {
    BytecodeBuilder::default().append_many([PUSH0, PUSH0, RETURN]).build()
}

fn constructor_calls_reverter() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(REVERTER)
        .push_number(50_000_u32)
        .append(CALL)
        .append(POP)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

fn constructor_calls_identity_precompile() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(IDENTITY_PRECOMPILE)
        .push_number(50_000_u32)
        .append(STATICCALL)
        .append(POP)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

fn create_pre_eip155_deploy_tx_with_value(init_code: Bytes, value: U256) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: SIGNED_TX_GAS_LIMIT,
        to: TxKind::Create,
        value,
        input: init_code,
        chain_id: None,
    };
    let r = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let s = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let sig = Signature::new(r, s, false);
    let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    let tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("should recover signer");
    (tx_bytes, signer)
}

fn create_pre_eip155_deploy_tx(init_code: Bytes) -> (Bytes, Address) {
    create_pre_eip155_deploy_tx_with_value(init_code, U256::ZERO)
}

fn keyless_deploy_call_tx(
    keyless_deployment_tx: Bytes,
    gas_limit_override: u64,
) -> MegaTransaction {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_deployment_tx,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();
    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn funded_db(signer: Address) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(signer, U256::from(LARGE_SIGNER_BALANCE));
    db
}

struct RunConfig<'a> {
    spec: MegaSpecId,
    db: &'a mut MemoryDatabase,
    tx_bytes: Bytes,
    gas_limit_override: u64,
    observer: Option<Rc<RefCell<dyn SandboxObserver<EmptyExternalEnv>>>>,
    tx_limits: Option<EvmTxRuntimeLimits>,
}

fn run_keyless(config: RunConfig<'_>) -> ResultAndState<MegaHaltReason> {
    let mut context = MegaContext::new(config.db, config.spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    if let Some(limits) = config.tx_limits {
        context = context.with_tx_runtime_limits(limits);
    }
    context.set_keyless_sandbox_observer(config.observer);
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(config.tx_bytes, config.gas_limit_override);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact")
}

fn assert_result_and_state_eq(
    with_observer: &ResultAndState<MegaHaltReason>,
    without_observer: &ResultAndState<MegaHaltReason>,
    case: &str,
) {
    assert_eq!(
        with_observer.result, without_observer.result,
        "{case}: execution result must match"
    );
    assert_eq!(
        with_observer.result.gas_used(),
        without_observer.result.gas_used(),
        "{case}: gas_used must match"
    );
    assert_eq!(
        with_observer.state.len(),
        without_observer.state.len(),
        "{case}: state account count must match"
    );
    for (addr, account) in &with_observer.state {
        let other = without_observer
            .state
            .get(addr)
            .unwrap_or_else(|| panic!("{case}: missing account {addr:?} in no-observer state"));
        assert_eq!(account, other, "{case}: account {addr:?}");
    }
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
    let observer: Rc<RefCell<dyn SandboxObserver<EmptyExternalEnv>>> =
        Rc::new(RefCell::new(RecordingObserver::default()));

    let observed = run_keyless(RunConfig {
        spec,
        db: &mut db_obs,
        tx_bytes: tx_bytes.clone(),
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: Some(observer),
        tx_limits,
    });
    let baseline = run_keyless(RunConfig {
        spec,
        db: &mut db_base,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: None,
        tx_limits,
    });
    assert_result_and_state_eq(&observed, &baseline, case);
}

fn run_with_recorder(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    tx_limits: Option<EvmTxRuntimeLimits>,
) -> (ResultAndState<MegaHaltReason>, Vec<ObservedEvent>) {
    let recorder = Rc::new(RefCell::new(RecordingObserver::default()));
    let observer: Rc<RefCell<dyn SandboxObserver<EmptyExternalEnv>>> = recorder.clone();
    let result = run_keyless(RunConfig {
        spec,
        db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: Some(observer),
        tx_limits,
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

fn parent_compute_gas_used(spec: MegaSpecId, signer: Address, tx_bytes: Bytes) -> u64 {
    let mut usage_db = funded_db(signer);
    let mut context = MegaContext::new(&mut usage_db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let used = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    used
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
                tx_gas_limit,
            } => {
                assert_eq!(*start_spec, spec);
                assert_eq!(*start_signer, signer);
                assert_eq!(*start_deploy, deploy_address);
                assert_eq!(*gas_limit_override, LARGE_GAS_LIMIT_OVERRIDE);
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

#[test]
fn test_sandbox_end_applied_empty_code_on_rex5() {
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
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

#[test]
fn test_inspector_adapter_records_sandbox_create_for_generic_inspector() {
    let tracer = GenericCreateCounter::default();
    let creates = Rc::clone(&tracer.creates);
    let adapter = InspectorSandboxObserver(tracer);
    let observer: Rc<RefCell<dyn SandboxObserver<EmptyExternalEnv>>> =
        Rc::new(RefCell::new(adapter));

    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let result = run_keyless(RunConfig {
        spec: MegaSpecId::REX5,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: Some(observer),
        tx_limits: None,
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
        observer: None,
        tx_limits: None,
    });
    assert!(result.result.is_success(), "default path must still succeed");
}
