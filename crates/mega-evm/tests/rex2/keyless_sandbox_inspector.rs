//! Rewriting sandbox inspector channel: intervention cases with a no-intervention control arm.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{address, hex, Address, Bytes, U256};
use mega_evm::{
    revm::context::result::{ExecutionResult, ResultAndState},
    sandbox::{
        decode_error_result, KeylessDeployError, SandboxCompletionKind, SandboxEndOutcome,
        SandboxInspector, SandboxObserver, SandboxRejectKind, SandboxStartInfo,
    },
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{CODECOPY, ISZERO, JUMPDEST, JUMPI, MLOAD, RETURN, SSTORE, STATICCALL},
    context::{ContextTr, JournalTr},
    handler::EvmTr,
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
    },
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use super::keyless_sandbox_support::{
    assert_result_and_state_eq, assert_usage_eq, constructor_calls_reverter,
    constructor_touches_sentinel, create_pre_eip155_deploy_tx,
    create_pre_eip155_deploy_tx_with_value, crowded_parent_env, empty_code_constructor, funded_db,
    keyless_deploy_call_tx, keyless_deploy_call_tx_with_outer_gas, parent_compute_gas_used,
    revert_constructor, run_keyless, run_keyless_with_usage, split_create_initcode,
    success_constructor, RunConfig, DEFAULT_OUTER_GAS_LIMIT, IDENTITY_PRECOMPILE,
    LARGE_GAS_LIMIT_OVERRIDE, LARGE_SIGNER_BALANCE, MERGE_FAIL_SENTINEL, REVERTER, SPECS,
};

const SUCCESS_TARGET: Address = address!("0000000000000000000000000000000000cccccc");
const OTHER_DEPLOY_ADDRESS: Address = address!("0000000000000000000000000000000000dddddd");
const IDENTITY_INPUT: U256 = U256::from_limbs([0x11, 0, 0, 0]);
const IDENTITY_OVERRIDE: U256 = U256::from_limbs([0x42, 0, 0, 0]);
const JOURNAL_SLOT: U256 = U256::from_limbs([0x53, 0, 0, 0]);
const JOURNAL_VALUE: U256 = U256::from_limbs([0x54, 0, 0, 0]);
const STEP_GAS_SURCHARGE: u64 = 1_000;

fn constructor_calls_identity_and_stores_return() -> Bytes {
    BytecodeBuilder::default()
        .push_u256(IDENTITY_INPUT)
        .push_number(0_u8)
        .append(revm::bytecode::opcode::MSTORE)
        .push_number(32_u8)
        .push_number(0_u8)
        .push_number(32_u8)
        .push_number(0_u8)
        .push_address(IDENTITY_PRECOMPILE)
        .push_number(50_000_u32)
        .append(STATICCALL)
        .append(revm::bytecode::opcode::POP)
        .push_number(0_u8)
        .append(MLOAD)
        .push_number(0_u8)
        .append(SSTORE)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

fn constructor_calls_success_target() -> Bytes {
    let prefix = BytecodeBuilder::default()
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(SUCCESS_TARGET)
        .push_number(50_000_u32)
        .append(revm::bytecode::opcode::CALL)
        .append(ISZERO)
        .build_vec();
    let success_body = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build_vec();
    let revert_pc = prefix.len() + 3 + success_body.len();
    assert!(revert_pc <= 255, "revert dest must fit in PUSH1");
    let mut code = prefix;
    code.push(0x60);
    code.push(revert_pc as u8);
    code.push(JUMPI);
    code.extend(success_body);
    code.push(JUMPDEST);
    code.extend(BytecodeBuilder::default().revert().build_vec());
    Bytes::from(code)
}

struct InspectorRunConfig<'a, I> {
    spec: MegaSpecId,
    db: &'a mut MemoryDatabase,
    tx_bytes: Bytes,
    gas_limit_override: u64,
    inspector: Option<Rc<RefCell<I>>>,
    tx_limits: Option<EvmTxRuntimeLimits>,
    outer_gas_limit: u64,
}

fn run_keyless_inspector<I>(config: InspectorRunConfig<'_, I>) -> ResultAndState<MegaHaltReason>
where
    I: SandboxInspector<EmptyExternalEnv> + 'static,
{
    run_keyless_inspector_with_usage(config).0
}

fn run_keyless_inspector_with_usage<I>(
    config: InspectorRunConfig<'_, I>,
) -> (ResultAndState<MegaHaltReason>, LimitUsage)
where
    I: SandboxInspector<EmptyExternalEnv> + 'static,
{
    let mut context = MegaContext::new(config.db, config.spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    if let Some(limits) = config.tx_limits {
        context = context.with_tx_runtime_limits(limits);
    }
    context.set_keyless_sandbox_inspector(config.inspector);
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx_with_outer_gas(
        config.tx_bytes,
        config.gas_limit_override,
        config.outer_gas_limit,
    );
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact");
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result, usage)
}

fn run_keyless_inspector_with_parent_env<I>(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    env: TestExternalEnvs,
    inspector: Option<Rc<RefCell<I>>>,
) -> ResultAndState<MegaHaltReason>
where
    I: SandboxInspector<TestExternalEnvs> + SandboxInspector<EmptyExternalEnv> + 'static,
{
    let mut context = MegaContext::new(db, spec).with_external_envs(env.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_inspector(inspector);
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact")
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InspectedEvent {
    Start,
    End(SandboxEndOutcome),
    Step(u8),
    Call { target: Address },
    CallEnd { target: Address },
    Create,
    CreateEnd,
}

#[derive(Default)]
struct NopSandboxInspector;

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for NopSandboxInspector {}

#[derive(Default)]
struct RecordingInspector {
    events: Vec<InspectedEvent>,
}

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for RecordingInspector {
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        self.events.push(InspectedEvent::Step(interp.bytecode.opcode()));
    }

    fn call(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.events.push(InspectedEvent::Call { target: inputs.target_address });
        None
    }

    fn call_end(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
        _outcome: &mut CallOutcome,
    ) {
        self.events.push(InspectedEvent::CallEnd { target: inputs.target_address });
    }

    fn create(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.events.push(InspectedEvent::Create);
        None
    }

    fn create_end(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.events.push(InspectedEvent::CreateEnd);
    }

    fn sandbox_start(&mut self, _info: &SandboxStartInfo) {
        self.events.push(InspectedEvent::Start);
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.events.push(InspectedEvent::End(outcome.clone()));
    }
}

#[derive(Default)]
struct RecordingObserver {
    events: Vec<InspectedEvent>,
}

impl<E: mega_evm::ExternalEnvTypes> SandboxObserver<E> for RecordingObserver {
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        self.events.push(InspectedEvent::Step(interp.bytecode.opcode()));
    }

    fn create(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &revm::interpreter::CreateInputs,
    ) {
        self.events.push(InspectedEvent::Create);
    }

    fn sandbox_start(&mut self, _info: &SandboxStartInfo) {
        self.events.push(InspectedEvent::Start);
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.events.push(InspectedEvent::End(outcome.clone()));
    }
}

#[derive(Default)]
struct IdentityShortCircuit {
    events: Vec<InspectedEvent>,
}

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for IdentityShortCircuit {
    fn call(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.events.push(InspectedEvent::Call { target: inputs.target_address });
        if inputs.target_address == IDENTITY_PRECOMPILE {
            return Some(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    Bytes::from(IDENTITY_OVERRIDE.to_be_bytes::<32>()),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            ));
        }
        None
    }

    fn call_end(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
        _outcome: &mut CallOutcome,
    ) {
        self.events.push(InspectedEvent::CallEnd { target: inputs.target_address });
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.events.push(InspectedEvent::End(outcome.clone()));
    }
}

struct CreateShortCircuit {
    address: Option<Address>,
    intercepted: bool,
    end: Option<SandboxEndOutcome>,
}

impl CreateShortCircuit {
    fn new(address: Option<Address>) -> Self {
        Self { address, intercepted: false, end: None }
    }
}

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for CreateShortCircuit {
    fn create(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        if self.intercepted {
            return None;
        }
        self.intercepted = true;
        Some(CreateOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::from_static(&[0x00]),
                Gas::new(inputs.gas_limit),
            ),
            self.address,
        ))
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.end = Some(outcome.clone());
    }
}

#[derive(Default)]
struct CallEndReverterRecorder {
    end: Option<SandboxEndOutcome>,
}

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for CallEndReverterRecorder {
    fn call_end(
        &mut self,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == SUCCESS_TARGET {
            outcome.result.result = InstructionResult::Revert;
            outcome.result.output = Bytes::new();
        }
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.end = Some(outcome.clone());
    }
}

#[derive(Default)]
struct StepGasSurcharge {
    charged: bool,
    end: Option<SandboxEndOutcome>,
}

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for StepGasSurcharge {
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        if !self.charged && interp.bytecode.opcode() == SSTORE {
            self.charged = true;
            let ok = interp.gas.record_cost(STEP_GAS_SURCHARGE);
            assert!(ok, "surcharge must fit in remaining gas");
        }
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.end = Some(outcome.clone());
    }
}

struct JournalWriter {
    deploy_address: Address,
    written: bool,
}

impl JournalWriter {
    fn new(deploy_address: Address) -> Self {
        Self { deploy_address, written: false }
    }
}

impl<E: mega_evm::ExternalEnvTypes> SandboxInspector<E> for JournalWriter {
    fn step(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        context: &mut mega_evm::MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
    ) {
        if self.written {
            return;
        }
        self.written = true;
        context.journal_mut().load_account(self.deploy_address).expect("load deploy account");
        context
            .journal_mut()
            .sstore(self.deploy_address, JOURNAL_SLOT, JOURNAL_VALUE)
            .expect("journal sstore");
    }
}

fn slot_value(result: &ResultAndState<MegaHaltReason>, addr: Address, slot: U256) -> Option<U256> {
    result
        .state
        .get(&addr)
        .and_then(|account| account.storage.get(&slot).map(|storage| storage.present_value()))
}

fn account_has_code(result: &ResultAndState<MegaHaltReason>, addr: Address) -> bool {
    result.state.get(&addr).is_some_and(|account| {
        account.info.code.as_ref().is_some_and(|code| !code.is_empty()) ||
            account.info.code_hash != revm::primitives::KECCAK_EMPTY
    })
}

fn last_end(events: &[InspectedEvent]) -> SandboxEndOutcome {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            InspectedEvent::End(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .expect("sandbox_end")
}

fn sandbox_applied_deployed_gas(events: &[InspectedEvent], case: &str) -> u64 {
    match last_end(events) {
        SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, gas_used } => {
            gas_used
        }
        other => panic!("{case}: expected Applied(Deployed), got {other:?}"),
    }
}

/// No-intervention success deploy used as the control arm for CREATE short-circuit cases.
fn run_unintervened_success_deploy(
    spec: MegaSpecId,
    tx_bytes: Bytes,
    signer: Address,
) -> ResultAndState<MegaHaltReason> {
    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let mut db = funded_db(signer);
    let result = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, .. } => {}
        other => panic!("{spec:?}: control arm expected Applied(Deployed), got {other:?}"),
    }
    assert!(account_has_code(&result, signer.create(0)), "{spec:?}: control arm deploys code");
    assert_control_signer_applied(&result, signer, spec);
    result
}

fn assert_control_signer_applied(
    result: &ResultAndState<MegaHaltReason>,
    signer: Address,
    spec: MegaSpecId,
) {
    let account = result.state.get(&signer).unwrap_or_else(|| {
        panic!("{spec:?}: control arm parent state must contain signer after apply")
    });
    assert!(account.is_touched(), "{spec:?}: control arm signer must be touched after apply");
    assert_eq!(account.info.nonce, 1, "{spec:?}: control arm signer nonce after apply");
    if spec.is_enabled(MegaSpecId::REX5) {
        assert_eq!(
            account.info.balance,
            U256::from(LARGE_SIGNER_BALANCE),
            "{spec:?}: REX5+ control arm signer balance stays at LARGE_SIGNER_BALANCE"
        );
    } else {
        assert!(
            account.info.balance < U256::from(LARGE_SIGNER_BALANCE),
            "{spec:?}: pre-REX5 control arm signer balance must drop, got {}",
            account.info.balance
        );
        let beneficiary = result.state.get(&Address::ZERO).unwrap_or_else(|| {
            panic!(
                "{spec:?}: pre-REX5 control arm parent state must contain beneficiary after apply"
            )
        });
        assert!(
            beneficiary.is_touched(),
            "{spec:?}: pre-REX5 control arm beneficiary must be touched after apply"
        );
        assert!(
            beneficiary.info.balance > U256::ZERO,
            "{spec:?}: pre-REX5 control arm beneficiary balance must increase after apply, got {}",
            beneficiary.info.balance
        );
    }
}

fn assert_parent_did_not_receive_sandbox_apply(
    result: &ResultAndState<MegaHaltReason>,
    signer: Address,
    deploy_address: Address,
    spec: MegaSpecId,
) {
    match result.state.get(&signer) {
        None => {}
        Some(account) => {
            assert!(
                !account.is_touched(),
                "{spec:?}: signer must not be touched when apply is skipped"
            );
            assert_eq!(
                account.info.balance,
                U256::from(LARGE_SIGNER_BALANCE),
                "{spec:?}: signer balance must stay at LARGE_SIGNER_BALANCE when apply is skipped"
            );
            assert_eq!(
                account.info.nonce, 0,
                "{spec:?}: signer nonce must stay 0 when apply is skipped"
            );
        }
    }

    // Tests use `MegaContext::new`, whose `BlockEnv` beneficiary defaults to `Address::ZERO`.
    // The outer tx still marks the coinbase touched; apply is visible here as a balance change.
    match result.state.get(&Address::ZERO) {
        None => {}
        Some(account) => {
            assert_eq!(
                account.info.balance,
                U256::ZERO,
                "{spec:?}: beneficiary balance must be unchanged when apply is skipped"
            );
        }
    }

    assert!(
        !account_has_code(result, deploy_address),
        "{spec:?}: parent must not hold code at deploy_address"
    );
    assert!(
        !account_has_code(result, OTHER_DEPLOY_ADDRESS),
        "{spec:?}: parent must not hold code at OTHER"
    );
    assert!(
        result.state.get(&OTHER_DEPLOY_ADDRESS).is_none_or(|account| {
            account.storage.is_empty() && account.info.code.as_ref().is_none_or(|c| c.is_empty())
        }),
        "{spec:?}: parent must not hold sandbox state at OTHER"
    );
    assert!(
        result.state.get(&deploy_address).is_none_or(|account| {
            account.storage.is_empty() && account.info.code.as_ref().is_none_or(|c| c.is_empty())
        }),
        "{spec:?}: parent must not hold sandbox state at deploy_address"
    );
}

#[test]
fn test_inspector_no_intervention_parity_across_specs_and_constructors() {
    let cases: [(&str, Bytes, bool, bool); 6] = [
        ("success", success_constructor(), false, false),
        ("revert", revert_constructor(), false, false),
        ("empty", empty_code_constructor(), false, false),
        ("calls_reverter", constructor_calls_reverter(), true, false),
        ("touches_sentinel", constructor_touches_sentinel(), false, true),
        ("split", split_create_initcode(), false, false),
    ];

    for spec in SPECS {
        for (name, init_code, with_reverter, with_sentinel) in &cases {
            let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code.clone());
            let mut db_base = funded_db(signer);
            if *with_reverter {
                db_base.set_account_code(REVERTER, Bytes::from_static(&hex!("60006000fd")));
            }
            if *with_sentinel {
                db_base.set_account_balance(MERGE_FAIL_SENTINEL, U256::from(1));
            }
            let mut db_nop = db_base.clone();
            let mut db_tracer = db_base.clone();

            let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
                spec,
                db: &mut db_base,
                tx_bytes: tx_bytes.clone(),
                gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
                observer: None::<Rc<RefCell<RecordingObserver>>>,
                tx_limits: None,
                outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
            });

            let nop = Rc::new(RefCell::new(NopSandboxInspector));
            let (nop_result, nop_usage) = run_keyless_inspector_with_usage(InspectorRunConfig {
                spec,
                db: &mut db_nop,
                tx_bytes: tx_bytes.clone(),
                gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
                inspector: Some(nop),
                tx_limits: None,
                outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
            });

            let case = format!("nop {name} {spec:?}");
            assert_result_and_state_eq(&nop_result, &baseline, &case);
            assert_usage_eq(nop_usage, baseline_usage, &case);

            let tracer = Rc::new(RefCell::new(TracingInspector::new(
                TracingInspectorConfig::default_parity(),
            )));
            let (traced, traced_usage) = run_keyless_inspector_with_usage(InspectorRunConfig {
                spec,
                db: &mut db_tracer,
                tx_bytes,
                gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
                inspector: Some(Rc::clone(&tracer)),
                tx_limits: None,
                outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
            });
            let case = format!("tracing {name} {spec:?}");
            assert_result_and_state_eq(&traced, &baseline, &case);
            assert_usage_eq(traced_usage, baseline_usage, &case);
            assert!(
                tracer.borrow().traces().nodes().iter().any(|node| node.trace.kind.is_any_create()),
                "{case}: TracingInspector must record a sandbox CREATE frame"
            );
        }
    }
}

#[test]
fn test_inspector_call_short_circuit_writes_override_and_still_emits_call_end() {
    for spec in SPECS {
        let (tx_bytes, signer) =
            create_pre_eip155_deploy_tx(constructor_calls_identity_and_stores_return());
        let deploy_address = signer.create(0);
        let mut db_ctrl = funded_db(signer);
        let mut db_int = db_ctrl.clone();

        let control = run_keyless(RunConfig {
            spec,
            db: &mut db_ctrl,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: None::<Rc<RefCell<RecordingObserver>>>,
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_eq!(
            slot_value(&control, deploy_address, U256::ZERO),
            Some(IDENTITY_INPUT),
            "{spec:?}: control arm stores identity echo"
        );

        let inspector = Rc::new(RefCell::new(IdentityShortCircuit::default()));
        let result = run_keyless_inspector(InspectorRunConfig {
            spec,
            db: &mut db_int,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&inspector)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_eq!(
            slot_value(&result, deploy_address, U256::ZERO),
            Some(IDENTITY_OVERRIDE),
            "{spec:?}: short-circuit arm stores override"
        );

        let events = inspector.borrow().events.clone();
        let call_ends = events
            .iter()
            .filter(|e| matches!(e, InspectedEvent::CallEnd { target } if *target == IDENTITY_PRECOMPILE))
            .count();
        assert_eq!(call_ends, 1, "{spec:?}: call_end on short-circuit frame once, got {events:?}");
        match last_end(&events) {
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, .. } => {}
            other => panic!("{spec:?}: expected Applied(Deployed), got {other:?}"),
        }
    }
}

#[test]
fn test_inspector_create_short_circuit_address_mismatch_skips_apply() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let deploy_address = signer.create(0);

        let _control = run_unintervened_success_deploy(spec, tx_bytes.clone(), signer);

        let inspector = Rc::new(RefCell::new(CreateShortCircuit::new(Some(OTHER_DEPLOY_ADDRESS))));
        let mut db_int = funded_db(signer);
        let result = run_keyless_inspector(InspectorRunConfig {
            spec,
            db: &mut db_int,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&inspector)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert!(
            matches!(result.result, ExecutionResult::Revert { .. }),
            "{spec:?}: AddressMismatch is a reverting outer CALL: {:?}",
            result.result
        );
        let error = match &result.result {
            ExecutionResult::Revert { output, .. } => decode_error_result(output),
            other => panic!("{spec:?}: expected revert, got {other:?}"),
        };
        assert!(
            matches!(error, Some(KeylessDeployError::AddressMismatch)),
            "{spec:?}: expected AddressMismatch, got {error:?}"
        );
        match inspector.borrow().end.clone() {
            Some(SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::AddressMismatch }) => {}
            other => panic!("{spec:?}: expected NotApplied(AddressMismatch), got {other:?}"),
        }
        assert_parent_did_not_receive_sandbox_apply(&result, signer, deploy_address, spec);
    }
}

#[test]
fn test_inspector_create_short_circuit_without_address_is_rejected() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let deploy_address = signer.create(0);
        let _control = run_unintervened_success_deploy(spec, tx_bytes.clone(), signer);

        let mut db = funded_db(signer);
        let inspector = Rc::new(RefCell::new(CreateShortCircuit::new(None)));
        let result = run_keyless_inspector(InspectorRunConfig {
            spec,
            db: &mut db,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&inspector)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert!(
            matches!(result.result, ExecutionResult::Revert { .. }),
            "{spec:?}: NoContractCreated reverts the outer CALL: {:?}",
            result.result
        );
        let error = match &result.result {
            ExecutionResult::Revert { output, .. } => decode_error_result(output),
            other => panic!("{spec:?}: expected revert, got {other:?}"),
        };
        assert!(
            matches!(error, Some(KeylessDeployError::NoContractCreated)),
            "{spec:?}: expected NoContractCreated, got {error:?}"
        );
        match inspector.borrow().end.clone() {
            Some(SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::Rejected }) => {}
            other => panic!("{spec:?}: expected NotApplied(Rejected), got {other:?}"),
        }
        assert!(
            result.state.get(&deploy_address).is_none_or(|account| {
                account.storage.is_empty() &&
                    account.info.code.as_ref().is_none_or(|c| c.is_empty())
            }),
            "{spec:?}: parent must not hold sandbox state"
        );
    }
}

#[test]
fn test_inspector_call_end_rewrite_reverts_constructor() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_calls_success_target());
        let mut db_ctrl = funded_db(signer);
        db_ctrl.set_account_code(SUCCESS_TARGET, Bytes::from_static(&[0x00]));
        let mut db_int = db_ctrl.clone();

        let control_rec = Rc::new(RefCell::new(RecordingInspector::default()));
        let control = run_keyless_inspector(InspectorRunConfig {
            spec,
            db: &mut db_ctrl,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&control_rec)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        match last_end(&control_rec.borrow().events) {
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, .. } => {}
            other => panic!("{spec:?}: control expected Applied(Deployed), got {other:?}"),
        }
        assert!(control.result.is_success(), "{spec:?}: control deploys");

        let inspector = Rc::new(RefCell::new(CallEndReverterRecorder::default()));
        let result = run_keyless_inspector(InspectorRunConfig {
            spec,
            db: &mut db_int,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&inspector)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        match inspector.borrow().end.clone() {
            Some(SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::ExecutionFailed,
                ..
            }) => {}
            other => panic!("{spec:?}: expected Applied(ExecutionFailed), got {other:?}"),
        }
        assert!(result.result.is_success(), "{spec:?}: execution-failed is success-style outer");
    }
}

#[test]
fn test_inspector_step_records_exact_sandbox_gas_surcharge() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db_base = funded_db(signer);
        let mut db_int = db_base.clone();

        let control_rec = Rc::new(RefCell::new(RecordingInspector::default()));
        let (baseline, _) = run_keyless_inspector_with_usage(InspectorRunConfig {
            spec,
            db: &mut db_base,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&control_rec)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        let baseline_sandbox = match last_end(&control_rec.borrow().events) {
            SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::Deployed,
                gas_used,
            } => gas_used,
            other => panic!("{spec:?}: baseline expected Applied(Deployed), got {other:?}"),
        };

        let inspector = Rc::new(RefCell::new(StepGasSurcharge::default()));
        let (result, _) = run_keyless_inspector_with_usage(InspectorRunConfig {
            spec,
            db: &mut db_int,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(Rc::clone(&inspector)),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        match inspector.borrow().end.clone() {
            Some(SandboxEndOutcome::Applied {
                completion: SandboxCompletionKind::Deployed,
                gas_used,
            }) => {
                assert_eq!(
                    gas_used,
                    baseline_sandbox + STEP_GAS_SURCHARGE,
                    "{spec:?}: sandbox_end.gas_used"
                );
            }
            other => panic!("{spec:?}: expected Applied(Deployed), got {other:?}"),
        }

        // Outer CALL gas: pre-REX5 the signer pays sandbox gas, so the surcharge stays
        // inside the sandbox. REX5+ pre-debits the reservation on the outer frame and
        // refunds unused, so the extra 1_000 is billed to the outer tx.
        let expected_outer_delta = match spec {
            MegaSpecId::REX2 | MegaSpecId::REX3 | MegaSpecId::REX4 => 0,
            MegaSpecId::REX5 | MegaSpecId::REX6 => STEP_GAS_SURCHARGE,
            other => panic!("unexpected spec {other:?}"),
        };
        assert_eq!(
            result.result.gas_used(),
            baseline.result.gas_used() + expected_outer_delta,
            "{spec:?}: outer gas_used (got {}, baseline {})",
            result.result.gas_used(),
            baseline.result.gas_used()
        );
    }
}

#[test]
fn test_inspector_journal_write_commits_on_success_and_rolls_back_on_revert() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let deploy_address = signer.create(0);
        let mut db_base = funded_db(signer);
        let mut db_int = db_base.clone();

        let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
            spec,
            db: &mut db_base,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: None::<Rc<RefCell<RecordingObserver>>>,
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        let inspector = Rc::new(RefCell::new(JournalWriter::new(deploy_address)));
        let (result, usage) = run_keyless_inspector_with_usage(InspectorRunConfig {
            spec,
            db: &mut db_int,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(inspector),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_eq!(
            slot_value(&result, deploy_address, JOURNAL_SLOT),
            Some(JOURNAL_VALUE),
            "{spec:?}: successful deploy keeps journal write"
        );
        assert_usage_eq(usage, baseline_usage, &format!("journal success {spec:?}"));
        let _ = baseline;

        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(revert_constructor());
        let deploy_address = signer.create(0);
        let mut db_base = funded_db(signer);
        let mut db_int = db_base.clone();
        let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
            spec,
            db: &mut db_base,
            tx_bytes: tx_bytes.clone(),
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: None::<Rc<RefCell<RecordingObserver>>>,
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        let inspector = Rc::new(RefCell::new(JournalWriter::new(deploy_address)));
        let (result, usage) = run_keyless_inspector_with_usage(InspectorRunConfig {
            spec,
            db: &mut db_int,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            inspector: Some(inspector),
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_ne!(
            slot_value(&result, deploy_address, JOURNAL_SLOT),
            Some(JOURNAL_VALUE),
            "{spec:?}: reverted constructor must not keep journal write"
        );
        assert_usage_eq(usage, baseline_usage, &format!("journal revert {spec:?}"));
        let _ = baseline;
    }
}

fn run_inspector_empty_and_crowded_parent_env(
    spec: MegaSpecId,
    tx_bytes: Bytes,
    signer: Address,
) -> (ResultAndState<MegaHaltReason>, u64, ResultAndState<MegaHaltReason>, u64, Vec<InspectedEvent>)
{
    let mut db_empty = funded_db(signer);
    let mut db_crowded = db_empty.clone();

    let empty_rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let empty = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db_empty,
        tx_bytes: tx_bytes.clone(),
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&empty_rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    let empty_sandbox_gas =
        sandbox_applied_deployed_gas(&empty_rec.borrow().events, &format!("empty {spec:?}"));

    let crowded_rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let crowded = run_keyless_inspector_with_parent_env(
        spec,
        &mut db_crowded,
        tx_bytes,
        crowded_parent_env(),
        Some(Rc::clone(&crowded_rec)),
    );
    let crowded_events = crowded_rec.borrow().events.clone();
    let crowded_sandbox_gas =
        sandbox_applied_deployed_gas(&crowded_events, &format!("crowded {spec:?}"));

    (empty, empty_sandbox_gas, crowded, crowded_sandbox_gas, crowded_events)
}

#[test]
fn test_inspector_pre_rex4_crowded_parent_env_keeps_empty_env_gas() {
    for spec in [MegaSpecId::REX2, MegaSpecId::REX3] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let (empty, empty_sandbox_gas, crowded, crowded_sandbox_gas, crowded_events) =
            run_inspector_empty_and_crowded_parent_env(spec, tx_bytes, signer);

        assert_eq!(
            crowded_sandbox_gas, empty_sandbox_gas,
            "{spec:?}: crowded parent env must not change pre-REX4 sandbox gas"
        );
        assert_eq!(
            crowded.result.gas_used(),
            empty.result.gas_used(),
            "{spec:?}: crowded parent env must not change pre-REX4 outer gas"
        );
        assert_result_and_state_eq(
            &crowded,
            &empty,
            &format!("pre-REX4 crowded vs empty parent env {spec:?}"),
        );
        assert!(
            crowded_events.iter().any(|e| matches!(e, InspectedEvent::Step(_))),
            "{spec:?}: opcode event stream must be non-empty: {crowded_events:?}"
        );
    }

    // REX4 shares the parent env, so crowded buckets must change sandbox gas.
    // This arm proves the equality above can fail when env freezing is absent.
    let spec = MegaSpecId::REX4;
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let (_empty, empty_sandbox_gas, _crowded, crowded_sandbox_gas, _) =
        run_inspector_empty_and_crowded_parent_env(spec, tx_bytes, signer);
    assert_ne!(
        crowded_sandbox_gas, empty_sandbox_gas,
        "{spec:?}: crowded parent env must change sandbox gas once env sharing is on"
    );
    assert_eq!(empty_sandbox_gas, 129_528, "{spec:?}: empty-env sandbox gas (env-freeze baseline)");
    assert_eq!(
        crowded_sandbox_gas, 493_528,
        "{spec:?}: crowded-env sandbox gas (env-sharing witness)"
    );
}

#[test]
fn test_inspector_for_parent_env_skips_pre_rex4_opcode_hooks() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let recorder = Rc::new(RefCell::new(RecordingInspector::default()));
        let inspector: Rc<RefCell<dyn SandboxInspector<TestExternalEnvs>>> = recorder.clone();

        let mut context =
            MegaContext::new(&mut db, spec).with_external_envs(crowded_parent_env().into());
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context.set_keyless_sandbox_inspector_for_parent_env(Some(inspector));
        let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
        let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("keyless deploy transact");
        let events = recorder.borrow().events.clone();

        assert!(result.result.is_success(), "{spec:?} deploy should succeed: {:?}", result.result);
        let starts = events.iter().filter(|e| matches!(e, InspectedEvent::Start)).count();
        let ends = events.iter().filter(|e| matches!(e, InspectedEvent::End(_))).count();
        assert_eq!(starts, 1, "{spec:?}: sandbox_start once");
        assert_eq!(ends, 1, "{spec:?}: sandbox_end once");
        let has_opcode =
            events.iter().any(|e| matches!(e, InspectedEvent::Step(_) | InspectedEvent::Create));
        if spec.is_enabled(MegaSpecId::REX4) {
            assert!(
                has_opcode,
                "{spec:?}: REX4+ parent-env inspector sees opcode hooks: {events:?}"
            );
        } else {
            assert!(
                !has_opcode,
                "{spec:?}: pre-REX4 parent-env inspector must not emit opcode hooks: {events:?}"
            );
        }
    }
}

#[test]
fn test_sandbox_hook_slots_are_exclusive_and_clear_restores_parity() {
    let spec = MegaSpecId::REX5;
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());

    let mut db_obs_then_insp = funded_db(signer);
    let observer = Rc::new(RefCell::new(RecordingObserver::default()));
    let inspector = Rc::new(RefCell::new(RecordingInspector::default()));
    let mut context = MegaContext::new(&mut db_obs_then_insp, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_observer(Some(Rc::clone(&observer)));
    context.set_keyless_sandbox_inspector(Some(Rc::clone(&inspector)));
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact");
    assert!(
        observer.borrow().events.is_empty(),
        "observer must be displaced by inspector: {:?}",
        observer.borrow().events
    );
    assert!(
        inspector.borrow().events.iter().any(|e| matches!(e, InspectedEvent::Start)),
        "inspector that replaced observer must receive events"
    );

    let mut db_insp_then_obs = funded_db(signer);
    let observer = Rc::new(RefCell::new(RecordingObserver::default()));
    let inspector = Rc::new(RefCell::new(RecordingInspector::default()));
    let mut context = MegaContext::new(&mut db_insp_then_obs, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_inspector(Some(Rc::clone(&inspector)));
    context.set_keyless_sandbox_observer(Some(Rc::clone(&observer)));
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact");
    assert!(
        inspector.borrow().events.is_empty(),
        "inspector must be displaced by observer: {:?}",
        inspector.borrow().events
    );
    assert!(
        observer.borrow().events.iter().any(|e| matches!(e, InspectedEvent::Start)),
        "observer that replaced inspector must receive events"
    );

    let mut db_cleared = funded_db(signer);
    let mut db_base = db_cleared.clone();
    let mut context = MegaContext::new(&mut db_cleared, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_inspector(Some(Rc::new(RefCell::new(NopSandboxInspector))));
    context.clear_keyless_sandbox_hook();
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes.clone(), LARGE_GAS_LIMIT_OVERRIDE);
    let cleared = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact");
    let cleared_usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
        spec,
        db: &mut db_base,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        observer: None::<Rc<RefCell<RecordingObserver>>>,
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    assert_result_and_state_eq(&cleared, &baseline, "cleared hook");
    assert_usage_eq(cleared_usage, baseline_usage, "cleared hook");
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "set sandbox hook after external envs are wired")]
fn test_with_external_envs_panics_in_debug_when_inspector_is_attached() {
    let mut context = MegaContext::new(revm::database::EmptyDB::default(), MegaSpecId::REX4);
    context.set_keyless_sandbox_inspector(Some(Rc::new(RefCell::new(NopSandboxInspector))));
    let _ = context.with_external_envs(TestExternalEnvs::<std::convert::Infallible>::new().into());
}

#[test]
fn test_inspector_reaches_all_seven_sandbox_end_outcomes() {
    let spec = MegaSpecId::REX5;

    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let deployed = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, .. } => {}
        other => panic!("Deployed: {other:?}"),
    }
    assert_control_signer_applied(&deployed, signer, spec);

    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(empty_code_constructor());
    let mut db = funded_db(signer);
    let _ = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::Applied { completion: SandboxCompletionKind::EmptyCode, .. } => {}
        other => panic!("EmptyCode: {other:?}"),
    }

    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(revert_constructor());
    let mut db = funded_db(signer);
    let _ = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::Applied {
            completion: SandboxCompletionKind::ExecutionFailed, ..
        } => {}
        other => panic!("ExecutionFailed: {other:?}"),
    }

    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let (tx_bytes, _signer) =
        create_pre_eip155_deploy_tx_with_value(success_constructor(), U256::from(1));
    let mut db = MemoryDatabase::default();
    let _ = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::Rejected } => {}
        other => panic!("Rejected: {other:?}"),
    }

    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let used = parent_compute_gas_used(spec, signer, tx_bytes.clone());
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(used.saturating_sub(1));
    let mut db = funded_db(signer);
    let _ = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: Some(limits),
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::PostAccountingHalt } => {}
        other => panic!("PostAccountingHalt: {other:?}"),
    }

    let rec = Rc::new(RefCell::new(RecordingInspector::default()));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_touches_sentinel());
    let mut inner = funded_db(signer);
    inner.set_account_balance(MERGE_FAIL_SENTINEL, U256::from(1));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(MERGE_FAIL_SENTINEL);
    db.fail_on_account_skip = 1;
    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_inspector(Some(Rc::clone(&rec)));
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("outer transact");
    match last_end(&rec.borrow().events) {
        SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::ApplyFailed } => {}
        other => panic!("ApplyFailed: {other:?}"),
    }

    let rec = Rc::new(RefCell::new(CreateShortCircuit::new(Some(OTHER_DEPLOY_ADDRESS))));
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let deploy_address = signer.create(0);
    let mut db = funded_db(signer);
    let mismatch = run_keyless_inspector(InspectorRunConfig {
        spec,
        db: &mut db,
        tx_bytes,
        gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
        inspector: Some(Rc::clone(&rec)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });
    let end = rec.borrow().end.clone();
    match end {
        Some(SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::AddressMismatch }) => {}
        other => panic!("AddressMismatch: {other:?}"),
    }
    assert_parent_did_not_receive_sandbox_apply(&mismatch, signer, deploy_address, spec);
}
