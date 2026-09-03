//! Corner-case sandbox shapes seen through the read-only observer channel.
//!
//! Each case uses one of the init codes in `mega_evm::test_utils::keyless` — the same bytes
//! the offline state-test corpus and the end-to-end suite run — and pins two things on every
//! spec: the exact event stream the observer receives, and that attaching the observer does
//! not change the result, state, or usage of the transaction.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{Address, Bytes, U256};
use mega_evm::{
    revm::interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        Interpreter,
    },
    sandbox::{SandboxCompletionKind, SandboxEndOutcome, SandboxInspector, SandboxObserver},
    test_utils::{
        deep_mixed_init, nested_keyless_call_init, revert_after_create_init, MemoryDatabase,
        REVERTING_RUNTIME,
    },
    ExternalEnvTypes, MegaContext, MegaSpecId, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
};

use crate::keyless_sandbox_support::{
    assert_result_and_state_eq, assert_usage_eq, create_pre_eip155_deploy_tx,
    create_pre_eip155_deploy_tx_with_value_and_gas_limit, funded_db,
    keyless_deploy_call_tx_with_outer_gas, run_keyless_with_usage, success_constructor, RunConfig,
    DEFAULT_OUTER_GAS_LIMIT, LARGE_GAS_LIMIT_OVERRIDE, REVERTER, SPECS,
};

/// The frame-level events these cases assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Create,
    CreateEnd { ok: bool, address: Option<Address> },
    Call { target: Address },
    CallEnd { target: Address, reverted: bool },
    Log,
    End(SandboxEndOutcome),
}

#[derive(Default)]
struct Recorder {
    events: Vec<Ev>,
}

impl<E: ExternalEnvTypes> SandboxObserver<E> for Recorder {
    fn call(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
    ) {
        self.events.push(Ev::Call { target: inputs.target_address });
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &CallOutcome,
    ) {
        self.events.push(Ev::CallEnd {
            target: inputs.target_address,
            reverted: outcome.result.is_revert(),
        });
    }

    fn create(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &CreateInputs,
    ) {
        self.events.push(Ev::Create);
    }

    fn create_end(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &CreateInputs,
        outcome: &CreateOutcome,
    ) {
        self.events.push(Ev::CreateEnd { ok: outcome.result.is_ok(), address: outcome.address });
    }

    fn log(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _log: alloy_primitives::Log,
    ) {
        self.events.push(Ev::Log);
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.events.push(Ev::End(outcome.clone()));
    }
}

/// The same recorder on the rewriting channel: records identically and never intervenes, so
/// both channels must produce the same stream and the same execution.
impl<E: ExternalEnvTypes> SandboxInspector<E> for Recorder {
    fn call(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.events.push(Ev::Call { target: inputs.target_address });
        None
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.events.push(Ev::CallEnd {
            target: inputs.target_address,
            reverted: outcome.result.is_revert(),
        });
    }

    fn create(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.events.push(Ev::Create);
        None
    }

    fn create_end(
        &mut self,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.events.push(Ev::CreateEnd { ok: outcome.result.is_ok(), address: outcome.address });
    }

    fn log(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<mega_evm::sandbox::SandboxDb<'_>, E>,
        _log: alloy_primitives::Log,
    ) {
        self.events.push(Ev::Log);
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.events.push(Ev::End(outcome.clone()));
    }
}

/// Runs `tx_bytes` with the recorder attached through the rewriting channel.
fn run_through_inspector_channel(
    spec: MegaSpecId,
    tx_bytes: &Bytes,
    signer: Address,
    gas_limit_override: u64,
    setup: &impl Fn(&mut MemoryDatabase),
) -> (
    mega_evm::revm::context::result::ResultAndState<mega_evm::MegaHaltReason>,
    mega_evm::LimitUsage,
    Vec<Ev>,
) {
    use mega_evm::{
        revm::{inspector::NoOpInspector, ExecuteEvm},
        MegaEvm,
    };
    let recorder = Rc::new(RefCell::new(Recorder::default()));
    let mut db = funded_db(signer);
    setup(&mut db);
    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context.set_keyless_sandbox_inspector(Rc::clone(&recorder));
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let tx = keyless_deploy_call_tx_with_outer_gas(
        tx_bytes.clone(),
        gas_limit_override,
        DEFAULT_OUTER_GAS_LIMIT,
    );
    let result = evm.transact(tx).expect("keyless deploy through the inspector channel");
    let usage = evm.ctx.additional_limit.borrow().get_usage();
    let events = recorder.borrow().events.clone();
    (result, usage, events)
}

/// One keyless deploy of `tx_bytes` under `spec`: once with the recorder on the read-only
/// channel, once with no hook, and once with the recorder on the rewriting channel. The three
/// runs must agree on result, state, and usage, and both channels must stream the same
/// events. Returns the observed run and the event stream.
fn run_case(
    spec: MegaSpecId,
    tx_bytes: &Bytes,
    signer: Address,
    gas_limit_override: u64,
    setup: impl Fn(&mut MemoryDatabase),
) -> (mega_evm::revm::context::result::ResultAndState<mega_evm::MegaHaltReason>, Vec<Ev>) {
    let case = format!("{spec:?}");
    let recorder = Rc::new(RefCell::new(Recorder::default()));

    let mut db = funded_db(signer);
    setup(&mut db);
    let (observed, observed_usage) = run_keyless_with_usage(RunConfig {
        spec,
        db: &mut db,
        tx_bytes: tx_bytes.clone(),
        gas_limit_override,
        observer: Some(Rc::clone(&recorder)),
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });

    let mut db = funded_db(signer);
    setup(&mut db);
    let (baseline, baseline_usage) = run_keyless_with_usage(RunConfig {
        spec,
        db: &mut db,
        tx_bytes: tx_bytes.clone(),
        gas_limit_override,
        observer: None::<Rc<RefCell<Recorder>>>,
        tx_limits: None,
        outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
    });

    assert_result_and_state_eq(&observed, &baseline, &case);
    assert_usage_eq(observed_usage, baseline_usage, &case);
    let events = recorder.borrow().events.clone();

    let (rewritten, rewritten_usage, rewritten_events) =
        run_through_inspector_channel(spec, tx_bytes, signer, gas_limit_override, &setup);
    assert_result_and_state_eq(&rewritten, &baseline, &format!("{case} (inspector channel)"));
    assert_usage_eq(rewritten_usage, baseline_usage, &format!("{case} (inspector channel)"));
    assert_eq!(rewritten_events, events, "{case}: both channels must stream the same frame events");

    (observed, events)
}

fn has_code(
    result: &mega_evm::revm::context::result::ResultAndState<mega_evm::MegaHaltReason>,
    addr: Address,
) -> bool {
    result
        .state
        .get(&addr)
        .and_then(|account| account.info.code.as_ref())
        .is_some_and(|code| !code.is_empty())
}

fn last_end(events: &[Ev]) -> &SandboxEndOutcome {
    match events.last() {
        Some(Ev::End(outcome)) => outcome,
        other => panic!("stream must end with sandbox_end, got {other:?}"),
    }
}

fn count(events: &[Ev], pred: impl Fn(&Ev) -> bool) -> usize {
    events.iter().filter(|e| pred(e)).count()
}

/// The constructor CREATEs a child and then REVERTs: the nested CREATE streams through the
/// observer, the sandbox ends as `ExecutionFailed`, and the parent journal receives neither
/// contract.
#[test]
fn test_extreme_revert_after_create_streams_child_then_rolls_back() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(revert_after_create_init());
    let deploy_address = signer.create(0);
    let child = deploy_address.create(1);

    for spec in SPECS {
        let (result, events) = run_case(spec, &tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, |_| {});

        assert!(result.result.is_success(), "{spec:?}: outer call returns success with errorData");
        assert_eq!(
            count(&events, |e| matches!(e, Ev::Create)),
            2,
            "{spec:?}: parent + child CREATE"
        );
        let ends: Vec<_> = events.iter().filter(|e| matches!(e, Ev::CreateEnd { .. })).collect();
        assert_eq!(
            ends[0],
            &Ev::CreateEnd { ok: true, address: Some(child) },
            "{spec:?}: the child CREATE completed first and succeeded"
        );
        assert!(
            matches!(ends[1], Ev::CreateEnd { ok: false, .. }),
            "{spec:?}: the parent CREATE ended in revert: {:?}",
            ends[1]
        );
        assert!(
            matches!(
                last_end(&events),
                SandboxEndOutcome::Applied {
                    completion: SandboxCompletionKind::ExecutionFailed,
                    ..
                }
            ),
            "{spec:?}: {:?}",
            last_end(&events)
        );
        assert!(!has_code(&result, deploy_address), "{spec:?}: parent rolled back");
        assert!(!has_code(&result, child), "{spec:?}: child rolled back with the parent");
    }
}

/// Three nesting levels, two reverting calls, a log, and a storage write, all inside one
/// sandbox: every frame boundary streams, and all three contracts land in the parent.
#[test]
fn test_extreme_deep_mixed_streams_every_nested_frame() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(deep_mixed_init(REVERTER));
    let parent = signer.create(0);
    let child = parent.create(1);
    let grandchild = child.create(1);
    let setup = |db: &mut MemoryDatabase| {
        db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
    };

    for spec in SPECS {
        let (result, events) = run_case(spec, &tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, setup);

        assert!(result.result.is_success(), "{spec:?}");
        assert_eq!(count(&events, |e| matches!(e, Ev::Create)), 3, "{spec:?}: three CREATE levels");
        assert_eq!(
            count(&events, |e| matches!(e, Ev::CreateEnd { ok: true, .. })),
            3,
            "{spec:?}: all three CREATEs succeed"
        );
        assert_eq!(
            count(&events, |e| *e == Ev::Call { target: REVERTER }),
            2,
            "{spec:?}: child and parent each CALL the reverter"
        );
        assert_eq!(
            count(&events, |e| *e == Ev::CallEnd { target: REVERTER, reverted: true }),
            2,
            "{spec:?}: both reverter calls report revert through call_end"
        );
        assert_eq!(count(&events, |e| matches!(e, Ev::Log)), 1, "{spec:?}: the parent's LOG1");
        assert!(matches!(
            last_end(&events),
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, .. }
        ));
        assert!(has_code(&result, parent), "{spec:?}: parent deployed");
        assert!(has_code(&result, child), "{spec:?}: child deployed");
        assert!(has_code(&result, grandchild), "{spec:?}: grandchild deployed");
        let slot0 = result.state[&parent].storage[&U256::ZERO].present_value();
        assert_eq!(
            slot0,
            U256::from_be_slice(child.as_slice()),
            "{spec:?}: slot 0 holds the child"
        );
    }
}

/// An inner gas limit above the CREATE transaction's intrinsic cost (97,198 here: the calldata
/// storage surcharge nearly doubles the standard figure) but below what the
/// constructor's SSTORE and code deposit need: the sandbox halts out of gas, reported as
/// `ExecutionFailed`, nothing deployed.
#[test]
fn test_extreme_sandbox_out_of_gas_is_applied_execution_failed() {
    const TIGHT_GAS: u64 = 110_000;
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx_with_value_and_gas_limit(
        success_constructor(),
        U256::ZERO,
        TIGHT_GAS,
    );
    let deploy_address = signer.create(0);

    for spec in SPECS {
        let (result, events) = run_case(spec, &tx_bytes, signer, TIGHT_GAS, |_| {});

        assert!(
            result.result.is_success(),
            "{spec:?}: outer call returns success with errorData; got {:?}; events {events:?}",
            result.result
        );
        assert_eq!(count(&events, |e| matches!(e, Ev::Create)), 1, "{spec:?}");
        assert!(
            matches!(
                events.iter().find(|e| matches!(e, Ev::CreateEnd { .. })),
                Some(Ev::CreateEnd { ok: false, .. })
            ),
            "{spec:?}: the CREATE frame halted"
        );
        assert!(matches!(
            last_end(&events),
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::ExecutionFailed, .. }
        ));
        assert!(!has_code(&result, deploy_address), "{spec:?}: nothing deployed");
    }
}

/// A constructor that CALLs `KeylessDeploy` from inside the sandbox: interception is
/// depth-0 only, so the call reaches the contract's bytecode and reverts, and the observer
/// sees that nested call as an ordinary reverted frame.
#[test]
fn test_extreme_nested_keyless_call_reverts_inside_sandbox() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(nested_keyless_call_init());
    let deploy_address = signer.create(0);
    let setup = |db: &mut MemoryDatabase| {
        db.set_account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);
    };

    for spec in SPECS {
        let (result, events) = run_case(spec, &tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, setup);

        assert!(result.result.is_success(), "{spec:?}");
        assert_eq!(
            count(&events, |e| *e == Ev::Call { target: KEYLESS_DEPLOY_ADDRESS }),
            1,
            "{spec:?}: the nested call is observed, not intercepted"
        );
        assert_eq!(
            count(&events, |e| *e ==
                Ev::CallEnd { target: KEYLESS_DEPLOY_ADDRESS, reverted: true }),
            1,
            "{spec:?}: the nested call reverted"
        );
        assert!(matches!(
            last_end(&events),
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, .. }
        ));
        assert!(has_code(&result, deploy_address), "{spec:?}: the constructor still deployed");
    }
}
