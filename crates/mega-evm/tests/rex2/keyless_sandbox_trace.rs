//! Tests for `sandbox::trace`: the paired outer inspector and `SandboxTracer`, and
//! `splice_sandbox_traces`, which grafts each recorded sandbox under the intercepted
//! `KeylessDeploy` CALL that started it.
//!
//! Compiled only with the `inspectors` feature, which gates `sandbox::trace` itself.

use std::{cell::RefCell, convert::Infallible, rc::Rc};

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
    revm::{
        context::result::{ExecutionResult, ResultAndState},
        InspectEvm,
    },
    sandbox::{
        decode_error_result,
        trace::{paired, splice_sandbox_traces, SandboxTracer, SharedTracingInspector},
        KeylessDeployError,
    },
    test_utils::{
        deep_mixed_init, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase, REVERTING_RUNTIME,
    },
    BlockLimits, IKeylessDeploy, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaContext,
    MegaEvm, MegaEvmFactory, MegaHaltReason, MegaHardforkConfig, MegaSpecId, MegaTransaction,
    MegaTxEnvelope, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{
        OpCode, BALANCE, CALL, CODECOPY, CREATE, LOG0, POP, PUSH0, RETURN, SELFDESTRUCT, STOP,
    },
    context::{BlockEnv, CfgEnv, TxEnv},
    database::State,
    inspector::NoOpInspector,
    interpreter::InstructionResult,
};
use revm_inspectors::tracing::{
    types::{CallKind, CallTraceNode, TraceMemberOrder},
    TracingInspector, TracingInspectorConfig,
};

use super::keyless_sandbox_support::{
    assert_result_and_state_eq, constructor_calls_reverter, create_pre_eip155_deploy_tx,
    empty_code_constructor, funded_db, keyless_deploy_call_tx, run_keyless,
    selfdestructing_constructor, success_constructor, RunConfig, DEFAULT_OUTER_GAS_LIMIT,
    LARGE_GAS_LIMIT_OVERRIDE, LARGE_SIGNER_BALANCE, MERGE_FAIL_SENTINEL, REVERTER, SPECS,
    TEST_CALLER,
};

/// A contract whose runtime is a single `STOP`, for transactions that never enter the sandbox.
const STOPPER: Address = address!("0000000000000000000000000000000000cccccc");
/// A helper whose runtime reads the balance of [`MERGE_FAIL_SENTINEL`], the account an
/// [`ErrorInjectingDatabase`] fails on.
const SENTINEL_READER: Address = address!("0000000000000000000000000000000000dddddd");

type SandboxHandle = Rc<RefCell<SandboxTracer>>;

fn all_config() -> TracingInspectorConfig {
    TracingInspectorConfig::all()
}

/// Init code that `CALL`s `target` and then deploys one byte of runtime code.
fn constructor_calls(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(target)
        .push_number(50_000_u32)
        .append(CALL)
        .append(POP)
        .push_number(1_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(CODECOPY)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Init code that emits one empty `LOG0` and returns empty runtime code.
fn logging_empty_code_constructor() -> Bytes {
    BytecodeBuilder::default().append_many([PUSH0, PUSH0, LOG0, PUSH0, PUSH0, RETURN]).build()
}

fn configured_context<DB: mega_evm::alloy_evm::Database>(
    db: DB,
    spec: MegaSpecId,
) -> MegaContext<DB, mega_evm::EmptyExternalEnv> {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

/// Runs one keyless deploy of `tx_bytes` on a fresh `MegaEvm` with `outer` installed as the
/// EVM inspector and `sandbox` attached through the observer channel.
fn trace_keyless_deploy(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    outer: SharedTracingInspector,
    sandbox: &SandboxHandle,
) -> ResultAndState<MegaHaltReason> {
    let context = configured_context(db, spec);
    let mut evm = MegaEvm::new(context).with_inspector(outer);
    evm.set_keyless_sandbox_observer(Rc::clone(sandbox));
    evm.inspect_tx(keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE))
        .expect("keyless deploy transact")
}

/// Runs a plain call to [`STOPPER`] with `outer` installed and `sandbox` attached, so the
/// outer arena holds a frame that is not a `KeylessDeploy` CALL.
fn trace_plain_call(
    db: &mut MemoryDatabase,
    outer: SharedTracingInspector,
    sandbox: &SandboxHandle,
) {
    db.set_account_code(STOPPER, Bytes::from_static(&[STOP]));
    let context = configured_context(db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context).with_inspector(outer);
    evm.set_keyless_sandbox_observer(Rc::clone(sandbox));
    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(STOPPER),
        gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = evm.inspect_tx(tx).expect("plain call transact");
    assert!(result.result.is_success(), "plain call must succeed: {:?}", result.result);
}

/// Traces the three-level `deep_mixed_init` deployment and splices it, returning the outer
/// tracer, the sandbox tracer, the execution result, and the deployer.
fn traced_deep_mixed(
    spec: MegaSpecId,
) -> (SharedTracingInspector, SandboxHandle, ResultAndState<MegaHaltReason>, Address) {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(deep_mixed_init(REVERTER));
    let mut db = funded_db(signer);
    db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
    let (outer, sandbox) = paired(all_config());
    assert_eq!(sandbox.borrow().pending(), 0, "{spec:?}: fresh tracer records nothing");
    let result = trace_keyless_deploy(spec, &mut db, tx_bytes, outer.clone(), &sandbox);
    assert!(result.result.is_success(), "{spec:?}: {:?}", result.result);
    assert_eq!(sandbox.borrow().pending(), 1, "{spec:?}: one sandbox execution recorded");
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    assert_eq!(sandbox.borrow().pending(), 0, "{spec:?}: splicing drains the tracer");
    (outer, sandbox, result, signer)
}

/// Checks that arena positions, parent links, child links, and depths agree for every node.
///
/// Steps are recorded at the journal depth of the frame that executes them, one level below
/// the frame's own trace depth, and the splice preserves that relation for grafted frames.
fn assert_arena_consistent(nodes: &[CallTraceNode], case: &str) {
    for (idx, node) in nodes.iter().enumerate() {
        assert_eq!(node.idx, idx, "{case}: node idx must match its arena position");
        for step in &node.trace.steps {
            assert_eq!(
                step.depth,
                node.trace.depth as u64 + 1,
                "{case}: step {} of node {idx} must sit one level below its frame",
                step.op
            );
        }
        for &child in &node.children {
            assert_eq!(nodes[child].parent, Some(idx), "{case}: child {child} parent link");
            assert_eq!(
                nodes[child].trace.depth,
                node.trace.depth + 1,
                "{case}: child {child} must sit one level below {idx}"
            );
        }
        if let Some(parent) = node.parent {
            assert!(
                nodes[parent].children.contains(&idx),
                "{case}: parent {parent} must list child {idx}"
            );
        }
    }
}

/// Asserts the spliced shape of `deep_mixed_init`: CALL `KeylessDeploy` → CREATE parent →
/// { CREATE child → { CREATE grandchild, reverted CALL }, reverted CALL }.
fn assert_deep_mixed_shape(outer: &SharedTracingInspector, signer: Address, case: &str) {
    let parent_addr = signer.create(0);
    let child_addr = parent_addr.create(1);
    let grandchild_addr = child_addr.create(1);

    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    assert_arena_consistent(nodes, case);
    assert_eq!(nodes.len(), 6, "{case}: outer CALL plus five sandbox frames");

    let root = &nodes[0];
    assert_eq!(
        root.trace.address, KEYLESS_DEPLOY_ADDRESS,
        "{case}: root is the KeylessDeploy CALL"
    );
    assert_eq!(root.trace.depth, 0, "{case}: root depth");
    assert!(!root.trace.kind.is_any_create(), "{case}: root is a CALL");
    assert_eq!(root.children.len(), 1, "{case}: the sandbox CREATE hangs under the root");
    let synthetic =
        root.trace.steps.iter().find(|step| step.op.get() == CREATE).unwrap_or_else(|| {
            panic!("{case}: the intercepted CALL carries a synthetic CREATE step")
        });
    assert_eq!(synthetic.contract, KEYLESS_DEPLOY_ADDRESS, "{case}: synthetic step contract");
    assert_eq!(
        synthetic.gas_cost, LARGE_GAS_LIMIT_OVERRIDE,
        "{case}: the synthetic step charges the sandbox reservation"
    );
    assert!(
        synthetic.gas_remaining <= DEFAULT_OUTER_GAS_LIMIT - KEYLESS_DEPLOY_OVERHEAD_GAS &&
            synthetic.gas_remaining > DEFAULT_OUTER_GAS_LIMIT - 1_000_000,
        "{case}: the synthetic step reports the outer frame's remaining gas, got {}",
        synthetic.gas_remaining
    );
    assert!(
        matches!(root.ordering.last(), Some(TraceMemberOrder::Call(0))),
        "{case}: the grafted child is the last member of the root, got {:?}",
        root.ordering.last()
    );

    let parent = &nodes[root.children[0]];
    assert!(parent.trace.kind.is_any_create(), "{case}: sandbox root is a CREATE");
    assert_eq!(
        parent.trace.address, parent_addr,
        "{case}: sandbox CREATE lands at signer.create(0)"
    );
    assert_eq!(parent.trace.depth, 1, "{case}: sandbox CREATE sits under the CALL");
    assert!(parent.trace.success, "{case}: the deployment succeeds");
    assert_eq!(parent.children.len(), 2, "{case}: parent CREATEs a child and CALLs the reverter");

    let child = &nodes[parent.children[0]];
    assert!(child.trace.kind.is_any_create(), "{case}: first parent child is the CREATE");
    assert_eq!(child.trace.address, child_addr, "{case}: child address");
    assert_eq!(
        child.children.len(),
        2,
        "{case}: child CREATEs a grandchild and CALLs the reverter"
    );

    let grandchild = &nodes[child.children[0]];
    assert!(grandchild.trace.kind.is_any_create(), "{case}: grandchild is a CREATE");
    assert_eq!(grandchild.trace.address, grandchild_addr, "{case}: grandchild address");
    assert_eq!(grandchild.trace.depth, 3, "{case}: grandchild depth");
    assert!(grandchild.children.is_empty(), "{case}: grandchild is a leaf");

    for (label, idx) in [("child", child.children[1]), ("parent", parent.children[1])] {
        let call = &nodes[idx];
        assert!(!call.trace.kind.is_any_create(), "{case}: {label}'s reverter frame is a CALL");
        assert_eq!(call.trace.address, REVERTER, "{case}: {label} calls the reverter");
        assert!(!call.trace.success, "{case}: {label}'s reverter call reverts");
        assert!(call.children.is_empty(), "{case}: reverter frame is a leaf");
    }
}

#[test]
fn test_tracer_splices_deep_mixed_shape_with_no_hook_parity() {
    for spec in SPECS {
        let (outer, _sandbox, result, signer) = traced_deep_mixed(spec);
        assert_deep_mixed_shape(&outer, signer, &format!("{spec:?}"));

        let (tx_bytes, _) = create_pre_eip155_deploy_tx(deep_mixed_init(REVERTER));
        let mut db = funded_db(signer);
        db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
        let baseline = run_keyless(RunConfig {
            spec,
            db: &mut db,
            tx_bytes,
            gas_limit_override: LARGE_GAS_LIMIT_OVERRIDE,
            observer: None::<Rc<RefCell<SandboxTracer>>>,
            tx_limits: None,
            outer_gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        });
        assert_result_and_state_eq(&result, &baseline, &format!("traced {spec:?}"));
    }
}

/// Splicing consumes what the tracer recorded: a second splice grafts nothing more, and
/// `fuse` on the outer handle starts the next transaction from a clean arena.
#[test]
fn test_splice_is_idempotent_and_fuse_resets_the_outer() {
    let (outer, sandbox, _result, _signer) = traced_deep_mixed(MegaSpecId::REX5);
    let before = outer.borrow().traces().nodes().len();
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    assert_eq!(outer.borrow().traces().nodes().len(), before, "a second splice is a no-op");
    assert_eq!(outer.borrow().traces().nodes()[0].children.len(), 1, "no duplicate child");

    outer.fuse();
    assert_eq!(outer.borrow().traces().nodes().len(), 1, "a fused arena keeps its default root");
    assert!(outer.borrow().traces().nodes()[0].children.is_empty());
}

/// One pair of handles serves consecutive transactions: each splice grafts only the
/// sandbox of the transaction just traced, and nothing from an earlier one.
#[test]
fn test_one_tracer_pair_across_two_transactions() {
    let (outer, sandbox) = paired(all_config());

    let (tx_a, signer_a) = create_pre_eip155_deploy_tx(deep_mixed_init(REVERTER));
    let mut db = funded_db(signer_a);
    db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
    let result = trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_a, outer.clone(), &sandbox);
    assert!(result.result.is_success(), "{:?}", result.result);
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    assert_deep_mixed_shape(&outer, signer_a, "tx A");

    outer.fuse();
    let (tx_b, signer_b) = create_pre_eip155_deploy_tx(success_constructor());
    assert_ne!(signer_a, signer_b);
    let mut db = funded_db(signer_b);
    let result = trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_b, outer.clone(), &sandbox);
    assert!(result.result.is_success(), "{:?}", result.result);
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    {
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_arena_consistent(nodes, "tx B");
        assert_eq!(
            nodes.len(),
            2,
            "tx B: the outer CALL and one sandbox CREATE, nothing from tx A"
        );
        assert_eq!(nodes[0].children, vec![1]);
        assert_eq!(nodes[1].trace.address, signer_b.create(0));
        assert!(nodes[1].children.is_empty());
    }
}

/// A recorded sandbox whose outer frame is not in the arena being spliced into is dropped:
/// an empty outer, an outer holding an unrelated transaction, and an outer that was fused
/// before the splice all end up untouched, and the tracer is left empty either way.
#[test]
fn test_splice_without_the_outer_frame_drops_the_sandbox() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());

    // Never spliced before the outer moved on to a plain transaction.
    let (outer, sandbox) = paired(all_config());
    let mut db = funded_db(signer);
    trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes.clone(), outer.clone(), &sandbox);
    assert_eq!(sandbox.borrow().pending(), 1);
    outer.fuse();
    let mut db = MemoryDatabase::default();
    trace_plain_call(&mut db, outer.clone(), &sandbox);
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    assert_eq!(sandbox.borrow().pending(), 0, "the stale sandbox is dropped, not kept");
    {
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_eq!(nodes.len(), 1, "no sandbox frame is appended to an unrelated trace");
        assert_eq!(nodes[0].trace.address, STOPPER);
        assert!(nodes[0].children.is_empty(), "the unrelated frame gains no child");
        assert!(
            nodes[0].trace.steps.iter().all(|step| step.op.get() != CREATE),
            "no synthetic CREATE step is added to an unrelated frame"
        );
    }

    // Spliced into an arena that recorded nothing.
    let (outer, sandbox) = paired(all_config());
    let mut db = funded_db(signer);
    trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, outer, &sandbox);
    let mut empty = TracingInspector::new(all_config());
    splice_sandbox_traces(&mut empty, &mut sandbox.borrow_mut());
    assert_eq!(sandbox.borrow().pending(), 0);
    assert_eq!(empty.traces().nodes().len(), 1, "the arena keeps only its default root");
    assert!(empty.traces().nodes()[0].children.is_empty());
}

/// A sandbox that revm's validation rejects before it enters a frame (the keyless signer
/// cannot pay) is still recorded: it claims its outer frame at the splice and grafts nothing.
#[test]
fn test_validate_rejected_sandbox_claims_its_frame_and_grafts_nothing() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = MemoryDatabase::default();
    db.set_account_balance(signer, U256::from(1));
    let (outer, sandbox) = paired(all_config());
    let result = trace_keyless_deploy(MegaSpecId::REX2, &mut db, tx_bytes, outer.clone(), &sandbox);
    let error = match &result.result {
        ExecutionResult::Revert { output, .. } => decode_error_result(output),
        other => panic!("an unfunded signer is rejected, got {other:?}"),
    };
    assert!(matches!(error, Some(KeylessDeployError::InsufficientBalance)), "got {error:?}");
    assert_eq!(sandbox.borrow().pending(), 1, "the rejected sandbox is still recorded");

    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    assert_eq!(sandbox.borrow().pending(), 0, "and consumed by the splice");
    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    assert_eq!(nodes.len(), 1, "nothing to graft: the sandbox never entered a frame");
    assert!(nodes[0].children.is_empty());
    assert!(nodes[0].trace.steps.is_empty(), "no synthetic CREATE step either");
}

/// `clear` discards recorded sandbox executions, so a later splice grafts nothing.
#[test]
fn test_clear_discards_recorded_sandboxes() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let (outer, sandbox) = paired(all_config());
    trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, outer.clone(), &sandbox);
    assert_eq!(sandbox.borrow().pending(), 1);

    sandbox.borrow_mut().clear();
    assert_eq!(sandbox.borrow().pending(), 0, "clear drops the recording");
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    assert_eq!(nodes.len(), 1, "nothing left to graft");
    assert!(nodes[0].children.is_empty());
}

/// The outer frame a sandbox is grafted under must match the intercepted call on every
/// field the outer inspector recorded for it; a frame that differs in any one of them is not
/// that call, and the sandbox is dropped rather than misplaced.
#[test]
fn test_outer_frame_pairing_requires_every_field() {
    type Mutation = fn(&mut CallTraceNode);
    let mutations: [(&str, Mutation); 7] = [
        ("address", |node| node.trace.address = STOPPER),
        ("kind", |node| node.trace.kind = CallKind::Create),
        ("depth", |node| node.trace.depth += 1),
        ("caller", |node| node.trace.caller = STOPPER),
        ("gas_limit", |node| node.trace.gas_limit += 1),
        ("value", |node| node.trace.value = U256::from(1)),
        ("data", |node| node.trace.data = Bytes::from_static(&[0xff])),
    ];
    for (field, mutate) in mutations {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let (outer, sandbox) = paired(all_config());
        trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, outer.clone(), &sandbox);
        mutate(&mut outer.borrow_mut().traces_mut().nodes_mut()[0]);

        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
        assert_eq!(sandbox.borrow().pending(), 0, "{field}: the sandbox is consumed");
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_eq!(nodes.len(), 1, "{field}: a frame differing in {field} is not the call");
        assert!(nodes[0].children.is_empty(), "{field}: nothing grafted");
    }
}

/// A database error inside a nested sandbox frame aborts the sandbox before the root CREATE
/// closes. The grafted frames report that abort instead of rendering as a success.
#[test]
fn test_sandbox_aborted_by_database_error_is_grafted_as_a_fatal_error() {
    for config in [TracingInspectorConfig::all(), TracingInspectorConfig::default_parity()] {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(constructor_calls(SENTINEL_READER));
        let mut inner = funded_db(signer);
        inner.set_account_code(
            SENTINEL_READER,
            BytecodeBuilder::default()
                .push_address(MERGE_FAIL_SENTINEL)
                .append(BALANCE)
                .append(POP)
                .stop()
                .build(),
        );
        let mut db = ErrorInjectingDatabase::new(inner);
        db.fail_on_account = Some(MERGE_FAIL_SENTINEL);

        let outer = SharedTracingInspector::new(TracingInspector::new(config));
        let sandbox = SandboxTracer::handle(config);
        let context = configured_context(&mut db, MegaSpecId::REX5);
        let mut evm = MegaEvm::new(context).with_inspector(outer.clone());
        evm.set_keyless_sandbox_observer(Rc::clone(&sandbox));
        let result = evm
            .inspect_tx(keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE))
            .expect("outer transact");
        let error = match &result.result {
            ExecutionResult::Revert { output, .. } => decode_error_result(output),
            other => panic!("the aborted sandbox reverts the outer call, got {other:?}"),
        };
        assert!(matches!(error, Some(KeylessDeployError::InternalError)), "got {error:?}");

        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_arena_consistent(nodes, "aborted");
        assert_eq!(nodes.len(), 3, "outer CALL, sandbox CREATE, helper CALL");
        assert!(!nodes[0].trace.success, "the outer CALL reverted");

        let create = &nodes[nodes[0].children[0]];
        assert!(create.trace.kind.is_any_create());
        assert_eq!(create.trace.status, Some(InstructionResult::FatalExternalError));
        assert!(!create.trace.success, "an unclosed CREATE is not a success");
        assert_eq!(create.trace.gas_used, create.trace.gas_limit, "an aborted frame keeps no gas");

        let helper = &nodes[create.children[0]];
        assert_eq!(helper.trace.address, SENTINEL_READER);
        assert_eq!(helper.trace.status, Some(InstructionResult::FatalExternalError));
        assert!(!helper.trace.success);
    }
}

/// Pre-REX5, an empty-code deployment drops the constructor's logs from the receipt; the
/// grafted CREATE carries no logs there, and keeps them from REX5 on where the receipt has
/// them.
#[test]
fn test_empty_code_logs_follow_the_receipt() {
    for spec in SPECS {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(logging_empty_code_constructor());
        let mut db = funded_db(signer);
        let (outer, sandbox) = paired(all_config());
        let result = trace_keyless_deploy(spec, &mut db, tx_bytes, outer.clone(), &sandbox);
        assert!(result.result.is_success(), "{spec:?}: {:?}", result.result);
        let receipt_logs = result.result.logs().len();

        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_arena_consistent(nodes, &format!("{spec:?}"));
        let create = &nodes[nodes[0].children[0]];
        assert!(create.trace.success, "{spec:?}: the constructor returned");
        let ordered_logs =
            create.ordering.iter().filter(|m| matches!(m, TraceMemberOrder::Log(_))).count();
        if spec.is_enabled(MegaSpecId::REX5) {
            assert_eq!(receipt_logs, 1, "{spec:?}: the receipt keeps the log");
            assert_eq!(create.logs.len(), 1, "{spec:?}: the trace shows the log");
            assert_eq!(ordered_logs, 1, "{spec:?}: the log stays in the member order");
        } else {
            assert_eq!(receipt_logs, 0, "{spec:?}: the receipt dropped the log");
            assert!(create.logs.is_empty(), "{spec:?}: the trace shows no log the receipt lacks");
            assert_eq!(ordered_logs, 0, "{spec:?}: no dangling log member");
        }
    }
}

/// A parity-style tracer records no steps, so a leaf sandbox CREATE is a node with a status
/// but neither steps nor children. It is still grafted, and the parent receives the
/// synthetic CREATE step that struct-log rendering needs.
#[test]
fn test_tracer_without_steps_still_splices_a_leaf_create() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let (outer, sandbox) = paired(TracingInspectorConfig::default_parity());
    let result = trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, outer.clone(), &sandbox);
    assert!(result.result.is_success(), "{:?}", result.result);

    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    assert_arena_consistent(nodes, "parity");
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].children, vec![1]);
    let create = &nodes[1];
    assert_eq!(create.trace.address, signer.create(0));
    assert!(create.trace.status.is_some(), "the CREATE frame completed");
    assert!(create.trace.steps.is_empty(), "parity config records no steps");
    assert!(create.children.is_empty(), "success_constructor makes no calls");
    assert_eq!(nodes[0].trace.steps.len(), 1, "exactly the synthetic CREATE step");
    assert_eq!(nodes[0].trace.steps[0].op.get(), CREATE);
    assert_eq!(nodes[0].trace.steps[0].contract, KEYLESS_DEPLOY_ADDRESS);
    assert_eq!(
        nodes[0].ordering,
        vec![TraceMemberOrder::Step(0), TraceMemberOrder::Call(0)],
        "the step precedes the child it introduces"
    );
}

/// The synthetic CREATE step exists to pair one call-like step with each child. A parent that
/// already carries a call-like step per child gets no extra step.
#[test]
fn test_splice_adds_no_create_step_when_the_parent_has_a_call_like_step_per_child() {
    // Borrow a real step from a plain call's trace and relabel it as a CALL.
    let (plain_outer, plain_sandbox) = paired(all_config());
    let mut db = MemoryDatabase::default();
    trace_plain_call(&mut db, plain_outer.clone(), &plain_sandbox);
    let mut call_step = plain_outer.borrow().traces().nodes()[0].trace.steps[0].clone();
    call_step.op = OpCode::new(CALL).expect("CALL is a defined opcode");

    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
    let mut db = funded_db(signer);
    let (outer, sandbox) = paired(all_config());
    let result = trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, outer.clone(), &sandbox);
    assert!(result.result.is_success(), "{:?}", result.result);
    {
        let mut outer_mut = outer.borrow_mut();
        let root = &mut outer_mut.traces_mut().nodes_mut()[0];
        assert!(root.trace.steps.is_empty(), "an intercepted CALL records no steps");
        root.trace.steps.push(call_step);
        root.ordering.push(TraceMemberOrder::Step(0));
    }

    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    assert_arena_consistent(nodes, "paired");
    assert_eq!(nodes[0].children, vec![1], "the sandbox CREATE is still grafted");
    assert_eq!(nodes[0].trace.steps.len(), 1, "no synthetic CREATE step is added");
    assert_eq!(nodes[0].trace.steps[0].op.get(), CALL);
    assert_eq!(nodes[0].ordering, vec![TraceMemberOrder::Step(0), TraceMemberOrder::Call(0)]);
}

/// The outer adapter forwards every hook of a frame the outer EVM itself executes: a plain
/// CREATE transaction whose constructor logs and self-destructs lands as one CREATE node
/// with the log and the self-destruct recorded.
#[test]
fn test_outer_adapter_forwards_create_log_and_selfdestruct() {
    let init_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, LOG0, PUSH0, PUSH0, SELFDESTRUCT])
        .build();
    let mut db = MemoryDatabase::default();
    let outer = SharedTracingInspector::new(TracingInspector::new(all_config()));
    let context = configured_context(&mut db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context).with_inspector(outer.clone());
    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Create,
        data: init_code,
        gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = evm.inspect_tx(tx).expect("create transact");
    assert!(result.result.is_success(), "{:?}", result.result);

    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    assert_eq!(nodes.len(), 1);
    let create = &nodes[0];
    assert!(create.trace.kind.is_any_create(), "the root is the CREATE frame");
    assert_eq!(create.trace.address, TEST_CALLER.create(0));
    assert_eq!(create.logs.len(), 1, "the constructor's LOG0 was forwarded");
    assert_eq!(create.trace.selfdestruct_address, Some(TEST_CALLER.create(0)));
    assert!(create.trace.steps.iter().any(|step| step.op.get() == SELFDESTRUCT));
}

/// A plain `TracingInspector` attached through either channel reaches the sandbox through
/// the blanket impls, including the `log` and `selfdestruct` hooks.
#[test]
fn test_blanket_channels_forward_log_and_selfdestruct_to_a_tracing_inspector() {
    for attach_as_inspector in [false, true] {
        for (name, init_code, expect_log, expect_selfdestruct) in [
            ("deep mixed", deep_mixed_init(REVERTER), true, false),
            ("selfdestruct", selfdestructing_constructor(), false, true),
        ] {
            let (tx_bytes, signer) = create_pre_eip155_deploy_tx(init_code);
            let mut db = funded_db(signer);
            db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
            let tracer = Rc::new(RefCell::new(TracingInspector::new(all_config())));
            let context = configured_context(&mut db, MegaSpecId::REX5);
            let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
            if attach_as_inspector {
                evm.set_keyless_sandbox_inspector(Rc::clone(&tracer));
            } else {
                evm.set_keyless_sandbox_observer(Rc::clone(&tracer));
            }
            let result = evm
                .inspect_tx(keyless_deploy_call_tx(tx_bytes, LARGE_GAS_LIMIT_OVERRIDE))
                .expect("keyless deploy transact");
            let case = format!("{name} inspector={attach_as_inspector}");
            assert!(result.result.is_success(), "{case}: {:?}", result.result);

            let tracer_ref = tracer.borrow();
            let root = &tracer_ref.traces().nodes()[0];
            assert!(root.trace.kind.is_any_create(), "{case}: sandbox CREATE recorded");
            let logs: usize = tracer_ref.traces().nodes().iter().map(|n| n.logs.len()).sum();
            assert_eq!(logs > 0, expect_log, "{case}: log forwarding, got {logs} logs");
            assert_eq!(
                root.trace.selfdestruct_address.is_some(),
                expect_selfdestruct,
                "{case}: selfdestruct forwarding"
            );
        }
    }
}

/// A `SELFDESTRUCT` inside the sandbox reaches the sandbox tracer's `selfdestruct` hook.
#[test]
fn test_sandbox_selfdestruct_reaches_the_tracer() {
    let (tx_bytes, signer) = create_pre_eip155_deploy_tx(selfdestructing_constructor());
    let mut db = funded_db(signer);
    let (outer, sandbox) = paired(all_config());
    let result = trace_keyless_deploy(MegaSpecId::REX5, &mut db, tx_bytes, outer.clone(), &sandbox);
    assert!(result.result.is_success(), "{:?}", result.result);

    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    let outer_ref = outer.borrow();
    let nodes = outer_ref.traces().nodes();
    let create = &nodes[nodes[0].children[0]];
    assert!(create.trace.kind.is_any_create());
    assert_eq!(
        create.trace.selfdestruct_address,
        Some(signer.create(0)),
        "the tracer saw the constructor self-destruct"
    );
    assert_eq!(
        create.trace.selfdestruct_refund_target,
        Some(Address::ZERO),
        "beneficiary as pushed by the constructor"
    );
}

fn keyless_deploy_envelope(nonce: u64, tx_bytes: Bytes) -> Recovered<MegaTxEnvelope> {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: tx_bytes,
        gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
    }
    .abi_encode();
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce,
        gas_price: 0,
        gas_limit: DEFAULT_OUTER_GAS_LIMIT,
        to: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        value: U256::ZERO,
        input: call_data.into(),
    };
    let signed = Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO);
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), TEST_CALLER)
}

/// The `MegaBlockExecutor` forwarders: attach a plain `TracingInspector` through the
/// inspector channel, detach, then attach a `SandboxTracer` through the observer channel,
/// one keyless deployment per step of a single block.
#[test]
fn test_block_executor_forwards_sandbox_hooks() {
    let (tx_a, signer_a) = create_pre_eip155_deploy_tx(success_constructor());
    let (tx_b, signer_b) = create_pre_eip155_deploy_tx(empty_code_constructor());
    let (tx_c, signer_c) = create_pre_eip155_deploy_tx(constructor_calls_reverter());
    assert_ne!(signer_a, signer_b);
    assert_ne!(signer_b, signer_c);

    let mut db = MemoryDatabase::default();
    for signer in [signer_a, signer_b, signer_c] {
        db.set_account_balance(signer, U256::from(LARGE_SIGNER_BALANCE));
    }
    db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
    let mut state = State::builder().with_database(&mut db).build();

    let evm_factory =
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new());
    let hardforks = MegaHardforkConfig::default().with_all_activated();
    let factory =
        MegaBlockExecutorFactory::new(&hardforks, evm_factory, OpAlloyReceiptBuilder::default());

    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX5;
    let block_env = BlockEnv { gas_limit: 4 * DEFAULT_OUTER_GAS_LIMIT, ..Default::default() };
    let evm_env = EvmEnv::new(cfg_env, block_env);
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());

    let (outer, sandbox) = paired(all_config());
    let mut executor =
        factory.create_executor_with_inspector(&mut state, block_ctx, evm_env, outer.clone());
    // `apply_pre_execution_changes` needs a configured SequencerRegistry on REX5+, and
    // KeylessDeploy interception does not depend on the predeploy bytecode.
    executor.evm.ctx.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    // Inspector channel: a plain tracing inspector records the sandbox CREATE on its own.
    let direct = Rc::new(RefCell::new(TracingInspector::new(all_config())));
    executor.set_keyless_sandbox_inspector(Rc::clone(&direct));
    outer.fuse();
    executor
        .execute_transaction_with_result_closure(&keyless_deploy_envelope(0, tx_a), |result| {
            assert!(result.is_success(), "tx A: {result:?}");
        })
        .expect("tx A executes");
    {
        let direct_ref = direct.borrow();
        let nodes = direct_ref.traces().nodes();
        assert!(nodes[0].trace.kind.is_any_create(), "tx A: the sandbox CREATE is its root");
        assert_eq!(nodes[0].trace.address, signer_a.create(0));
        assert!(nodes[0].trace.success);
    }
    direct.borrow_mut().fuse();

    // Detached: the sandbox still runs and nothing records it.
    executor.clear_keyless_sandbox_hook();
    outer.fuse();
    executor
        .execute_transaction_with_result_closure(&keyless_deploy_envelope(1, tx_b), |result| {
            assert!(result.is_success(), "tx B: {result:?}");
        })
        .expect("tx B executes");
    assert!(direct.borrow().traces().nodes()[0].trace.status.is_none(), "tx B: nothing recorded");
    assert_eq!(sandbox.borrow().pending(), 0, "tx B: nothing recorded");

    // Observer channel: the sandbox tracer records, and the splice grafts the constructor's
    // reverted CALL under the CREATE.
    executor.set_keyless_sandbox_observer(Rc::clone(&sandbox));
    outer.fuse();
    executor
        .execute_transaction_with_result_closure(&keyless_deploy_envelope(2, tx_c), |result| {
            assert!(result.is_success(), "tx C: {result:?}");
        })
        .expect("tx C executes");
    assert_eq!(sandbox.borrow().pending(), 1, "tx C: sandbox recorded");
    splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
    {
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_arena_consistent(nodes, "tx C");
        assert_eq!(nodes[0].trace.address, KEYLESS_DEPLOY_ADDRESS);
        assert_eq!(nodes[0].children.len(), 1, "tx C: one grafted CREATE");
        let create = &nodes[nodes[0].children[0]];
        assert_eq!(create.trace.address, signer_c.create(0));
        assert_eq!(create.children.len(), 1, "tx C: the constructor's CALL");
        let call = &nodes[create.children[0]];
        assert_eq!(call.trace.address, REVERTER);
        assert!(!call.trace.success, "tx C: the reverter call reverts");
        assert_eq!(call.trace.depth, 2);
    }
}
