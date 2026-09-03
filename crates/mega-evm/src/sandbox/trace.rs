//! Shared helpers for tracing nested sandbox execution with `revm-inspectors`.
//!
//! A [`TracingInspector`] attached to the outer EVM never sees sandbox frames: the sandbox
//! is a separate EVM whose journal reports its top-level CREATE at depth 0, and recording that
//! into the outer arena would overwrite the CALL root. The pattern here is two recorders: the
//! outer [`TracingInspector`] installed on the EVM, and a [`SandboxTracer`] attached through
//! the keyless sandbox observer channel, which records every sandbox execution into an arena
//! of its own. After the transaction returns, [`splice_sandbox_traces`] grafts each sandbox
//! arena under the outer `KeylessDeploy` CALL frame that started it.
//!
//! ```ignore
//! let (outer, sandbox) = paired(TracingInspectorConfig::all());
//! let mut evm = MegaEvm::new(ctx).with_inspector(outer.clone());
//! evm.set_keyless_sandbox_observer(sandbox.clone());
//! let result = evm.inspect_tx(tx)?;
//! splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
//! ```
//!
//! When the outer inspector is owned elsewhere (a node's tracing RPC hands out a plain
//! [`TracingInspector`]), attach [`SandboxTracer::handle`] on its own and splice into that
//! inspector after each transaction. Splicing consumes the recorded sandboxes, so it is
//! idempotent and a tracer reused across transactions never carries frames over: a sandbox
//! whose outer frame is not in the arena being spliced into is dropped.
//!
//! Requires the `inspectors` feature.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::cell::{Ref, RefCell, RefMut};
use std::{rc::Rc, vec, vec::Vec};

use alloy_primitives::{Address, Bytes, Log, U256};
use revm::{
    bytecode::opcode::{self, OpCode},
    context::ContextTr,
    inspector::JournalExt,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        InstructionResult, Interpreter,
    },
    Inspector,
};
use revm_inspectors::tracing::{
    types::{CallTraceNode, CallTraceStep, TraceMemberOrder},
    TracingInspector, TracingInspectorConfig,
};

use crate::{ExternalEnvTypes, MegaContext, MegaSpecId, KEYLESS_DEPLOY_ADDRESS};

use super::{
    observer::{OuterCallInfo, SandboxCompletionKind, SandboxEndOutcome, SandboxStartInfo},
    state::SandboxDb,
    SandboxObserver,
};

/// [`Inspector`] adapter over a [`TracingInspector`] shared via [`Rc`]/[`RefCell`].
///
/// Serves as the outer EVM inspector when the caller wants to keep a handle to read the
/// recorded traces after execution. It is the outer half of [`paired`]; the sandbox half is a
/// [`SandboxTracer`]. Do not attach a handle to this type through the sandbox observer
/// channel: the sandbox would record into the outer arena and overwrite the `KeylessDeploy`
/// CALL root with its own CREATE.
#[derive(Clone, Debug)]
pub struct SharedTracingInspector(Rc<RefCell<TracingInspector>>);

impl SharedTracingInspector {
    /// Wraps an existing [`TracingInspector`].
    pub fn new(inspector: TracingInspector) -> Self {
        Self(Rc::new(RefCell::new(inspector)))
    }

    /// Resets recorded traces so a subsequent transaction starts from a clean arena.
    pub fn fuse(&self) {
        self.0.borrow_mut().fuse();
    }

    /// Borrows the inner inspector.
    pub fn borrow(&self) -> Ref<'_, TracingInspector> {
        self.0.borrow()
    }

    /// Mutably borrows the inner inspector.
    pub fn borrow_mut(&self) -> RefMut<'_, TracingInspector> {
        self.0.borrow_mut()
    }
}

impl<CTX> Inspector<CTX> for SharedTracingInspector
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        Inspector::initialize_interp(&mut *self.0.borrow_mut(), interp, context);
    }

    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        Inspector::step(&mut *self.0.borrow_mut(), interp, context);
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        Inspector::step_end(&mut *self.0.borrow_mut(), interp, context);
    }

    fn log(&mut self, interp: &mut Interpreter, context: &mut CTX, log: Log) {
        Inspector::log(&mut *self.0.borrow_mut(), interp, context, log);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        Inspector::call(&mut *self.0.borrow_mut(), context, inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        Inspector::call_end(&mut *self.0.borrow_mut(), context, inputs, outcome);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        Inspector::create(&mut *self.0.borrow_mut(), context, inputs)
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        Inspector::create_end(&mut *self.0.borrow_mut(), context, inputs, outcome);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        <TracingInspector as Inspector<CTX>>::selfdestruct(
            &mut self.0.borrow_mut(),
            contract,
            target,
            value,
        );
    }
}

/// Builds an outer inspector and a sandbox tracer with the same `config`, ready to be
/// attached to one EVM: the first as its inspector, the second through
/// `set_keyless_sandbox_observer`.
pub fn paired(
    config: TracingInspectorConfig,
) -> (SharedTracingInspector, Rc<RefCell<SandboxTracer>>) {
    (SharedTracingInspector::new(TracingInspector::new(config)), SandboxTracer::handle(config))
}

/// One recorded sandbox execution, waiting to be grafted under its outer frame.
#[derive(Debug)]
struct SandboxTrace {
    /// Lifecycle context delivered at `sandbox_start`; carries the outer frame identity.
    start: SandboxStartInfo,
    /// Arena of the sandbox EVM, one per execution.
    tracer: TracingInspector,
    /// Whether the sandbox EVM entered a frame; a validate-reject leaves this `false`.
    entered: bool,
}

impl SandboxTrace {
    /// Finalizes the arena once the sandbox reported its outcome.
    fn finish(&mut self, outcome: &SandboxEndOutcome) {
        if !self.entered {
            return;
        }
        let nodes = self.tracer.traces_mut().nodes_mut();

        // A frame the sandbox never closed (a database error aborted the sandbox from inside
        // a nested frame, so revm unwound without `create_end`) would otherwise render as a
        // successful CREATE with no gas used. Close it the way revm reports the abort.
        for node in nodes.iter_mut() {
            if node.trace.status.is_none() {
                node.trace.status = Some(InstructionResult::FatalExternalError);
                node.trace.success = false;
                node.trace.gas_used = node.trace.gas_limit;
            }
        }

        // Pre-REX5, an empty-code deployment drops the constructor's logs from the receipt
        // even though the CREATE frame completed successfully (frozen behavior). Geth's
        // `withLog` rendering shows a log only when the receipt holds it, so strip them here.
        let logs_dropped = matches!(
            outcome,
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::EmptyCode, .. }
        ) && !self.start.spec.is_enabled(MegaSpecId::REX5);
        if logs_dropped {
            for node in nodes.iter_mut() {
                node.logs.clear();
                node.ordering.retain(|member| !matches!(member, TraceMemberOrder::Log(_)));
            }
        }
    }
}

/// Sandbox-side recorder for the keyless sandbox observer channel.
///
/// Records each sandbox execution into a fresh [`TracingInspector`] arena and keeps it,
/// together with the identity of the outer `KeylessDeploy` frame that started it, until
/// [`splice_sandbox_traces`] grafts it into the outer trace. Attach it via
/// `set_keyless_sandbox_observer`; it implements [`SandboxObserver`] for every env type.
///
/// Recording is per execution, so a tracer kept across transactions does not need a reset;
/// splicing drains what was recorded, and [`Self::clear`] discards it. Do one or the other
/// after every transaction, before the outer inspector moves on: pairing matches a recorded
/// sandbox to an outer frame by the fields of the intercepted call, and a recording left
/// behind would match a later call with the same caller, gas, value, and calldata (a retry
/// of the same payload that the interceptor turns down before it reaches the sandbox, for
/// instance) even though it belongs to an earlier transaction.
#[derive(Debug)]
pub struct SandboxTracer {
    config: TracingInspectorConfig,
    current: Option<SandboxTrace>,
    finished: Vec<SandboxTrace>,
}

impl SandboxTracer {
    /// A tracer whose sandbox arenas use `config`.
    pub fn new(config: TracingInspectorConfig) -> Self {
        Self { config, current: None, finished: Vec::new() }
    }

    /// [`Self::new`] wrapped for `set_keyless_sandbox_observer`.
    pub fn handle(config: TracingInspectorConfig) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::new(config)))
    }

    /// Number of recorded sandbox executions not yet spliced.
    pub fn pending(&self) -> usize {
        self.finished.len()
    }

    /// Discards every recorded sandbox execution, including one in flight.
    pub fn clear(&mut self) {
        self.current = None;
        self.finished.clear();
    }

    fn with_current<R>(&mut self, f: impl FnOnce(&mut SandboxTrace) -> R) -> Option<R> {
        self.current.as_mut().map(f)
    }
}

impl<E: ExternalEnvTypes> SandboxObserver<E> for SandboxTracer {
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.with_current(|t| Inspector::initialize_interp(&mut t.tracer, interp, context));
    }

    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.with_current(|t| Inspector::step(&mut t.tracer, interp, context));
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.with_current(|t| Inspector::step_end(&mut t.tracer, interp, context));
    }

    fn log(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        log: Log,
    ) {
        self.with_current(|t| Inspector::log(&mut t.tracer, interp, context, log));
    }

    fn call(&mut self, context: &mut MegaContext<SandboxDb<'_>, E>, inputs: &CallInputs) {
        self.with_current(|t| {
            t.entered = true;
            let mut inputs = inputs.clone();
            let _ = Inspector::call(&mut t.tracer, context, &mut inputs);
        });
    }

    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &CallOutcome,
    ) {
        self.with_current(|t| {
            let mut outcome = outcome.clone();
            Inspector::call_end(&mut t.tracer, context, inputs, &mut outcome);
        });
    }

    fn create(&mut self, context: &mut MegaContext<SandboxDb<'_>, E>, inputs: &CreateInputs) {
        self.with_current(|t| {
            t.entered = true;
            let mut inputs = inputs.clone();
            let _ = Inspector::create(&mut t.tracer, context, &mut inputs);
        });
    }

    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CreateInputs,
        outcome: &CreateOutcome,
    ) {
        self.with_current(|t| {
            let mut outcome = outcome.clone();
            Inspector::create_end(&mut t.tracer, context, inputs, &mut outcome);
        });
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.with_current(|t| {
            <TracingInspector as Inspector<MegaContext<SandboxDb<'static>, E>>>::selfdestruct(
                &mut t.tracer,
                contract,
                target,
                value,
            );
        });
    }

    fn sandbox_start(&mut self, info: &SandboxStartInfo) {
        debug_assert!(self.current.is_none(), "sandbox_start while a sandbox is in flight");
        // The interceptor only sandboxes top-level calls, so a second execution always belongs
        // to a later transaction: a recording still pending here was neither spliced nor
        // cleared after the transaction that produced it.
        debug_assert!(
            self.finished.is_empty(),
            "a recorded sandbox was neither spliced nor cleared before the next transaction"
        );
        self.current = Some(SandboxTrace {
            start: info.clone(),
            tracer: TracingInspector::new(self.config),
            entered: false,
        });
    }

    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        if let Some(mut trace) = self.current.take() {
            trace.finish(outcome);
            self.finished.push(trace);
        }
    }
}

/// Whether `node` is the outer frame described by `call`: the intercepted `KeylessDeploy`
/// CALL, matched on the fields revm's `call` hook recorded for it.
fn is_outer_frame(node: &CallTraceNode, call: &OuterCallInfo) -> bool {
    let trace = &node.trace;
    trace.address == KEYLESS_DEPLOY_ADDRESS &&
        !trace.kind.is_any_create() &&
        trace.depth == call.depth &&
        trace.caller == call.caller &&
        trace.gas_limit == call.gas_limit &&
        trace.value == call.value &&
        trace.data == call.data
}

/// Grafts every sandbox execution recorded by `sandbox` under the outer `KeylessDeploy` CALL
/// frame that started it, then leaves `sandbox` empty.
///
/// Each recorded sandbox is paired with the first unclaimed outer frame that matches the
/// call the sandbox reported at `sandbox_start`; frames are matched in arena order, which is
/// execution order. A sandbox whose frame is not in `outer` (the outer inspector was fused
/// before splicing, or belongs to another transaction) is dropped. A sandbox that never
/// entered a frame (a validate-reject) claims its frame and grafts nothing.
///
/// Sandbox execution uses a fresh journal whose top-level CREATE is depth 0. Depths and arena
/// indices are rewritten so the Geth call tree hangs the CREATE under the CALL.
///
/// Geth-style struct logs only descend into a child when the parent frame has a matching
/// call-like opcode in its steps. An intercepted `KeylessDeploy` CALL records no bytecode, so
/// a CREATE step is grafted onto the parent; without it the sandbox constructor steps would
/// be omitted from struct-log output instead of nested in execution order. That step reports
/// the outer frame's remaining gas at sandbox start as `gas` and the sandbox reservation as
/// `gasCost`; the contract never executed it.
pub fn splice_sandbox_traces(outer: &mut TracingInspector, sandbox: &mut SandboxTracer) {
    let original_len = outer.traces().nodes().len();
    let mut claimed = vec![false; original_len];

    for trace in sandbox.finished.drain(..) {
        let parent_idx = outer.traces().nodes()[..original_len]
            .iter()
            .enumerate()
            .position(|(idx, node)| !claimed[idx] && is_outer_frame(node, &trace.start.outer_call));
        let Some(parent_idx) = parent_idx else {
            continue;
        };
        claimed[parent_idx] = true;
        if trace.entered {
            graft(outer, parent_idx, trace);
        }
    }
}

/// Moves the arena of `trace` under `outer`'s node `parent_idx`.
fn graft(outer: &mut TracingInspector, parent_idx: usize, mut trace: SandboxTrace) {
    let depth_offset = outer.traces().nodes()[parent_idx].trace.depth + 1;
    let base = outer.traces().nodes().len();

    let sandbox_nodes = core::mem::take(trace.tracer.traces_mut().nodes_mut());
    let remapped = sandbox_nodes.into_iter().enumerate().map(|(old_idx, mut node)| {
        node.idx = base + old_idx;
        node.trace.depth += depth_offset;
        for step in &mut node.trace.steps {
            step.depth += depth_offset as u64;
        }
        node.parent = Some(match node.parent {
            None => parent_idx,
            Some(parent) => base + parent,
        });
        for child in &mut node.children {
            *child += base;
        }
        node
    });

    outer.traces_mut().nodes_mut().extend(remapped);
    let parent = &mut outer.traces_mut().nodes_mut()[parent_idx];
    let child_slot = parent.children.len();
    parent.children.push(base);

    // Struct-log rendering pairs call-like parent steps with `children` in order. A missing
    // CREATE step here would leave the grafted child unreachable.
    let calllike = parent.trace.steps.iter().filter(|s| is_calllike_opcode(s.op.get())).count();
    if calllike < parent.children.len() {
        let step_idx = parent.trace.steps.len();
        let create_depth = parent.trace.depth as u64 + 1;
        let contract = parent.trace.address;
        parent.trace.steps.push(intercepted_create_step(
            create_depth,
            contract,
            trace.start.outer_call.gas_remaining,
            trace.start.effective_gas_limit,
        ));
        parent.ordering.push(TraceMemberOrder::Step(step_idx));
    }
    parent.ordering.push(TraceMemberOrder::Call(child_slot));
}

/// CREATE/CALL-family opcodes that struct-log rendering uses to walk into children.
fn is_calllike_opcode(op: u8) -> bool {
    matches!(
        op,
        opcode::CALL |
            opcode::DELEGATECALL |
            opcode::STATICCALL |
            opcode::CREATE |
            opcode::CALLCODE |
            opcode::CREATE2
    )
}

/// Placeholder CREATE recorded on an intercepted `KeylessDeploy` CALL so struct-log output can
/// descend into the spliced sandbox frame.
///
/// `gas_remaining` is the outer frame's gas when the sandbox started; `gas_cost` is the gas
/// reserved for the sandbox, the way Geth reports the gas handed to a callee on the calling
/// step. Pre-REX5 the reservation is the caller's override and can exceed the outer frame's
/// gas; the sandbox pays for itself there.
fn intercepted_create_step(
    depth: u64,
    contract: Address,
    gas_remaining: u64,
    gas_cost: u64,
) -> CallTraceStep {
    CallTraceStep {
        depth,
        pc: 0,
        op: OpCode::new(opcode::CREATE).expect("CREATE is a defined opcode"),
        contract,
        stack: None,
        push_stack: None,
        memory: None,
        returndata: Bytes::new(),
        gas_remaining,
        gas_refund_counter: 0,
        gas_used: 0,
        gas_cost,
        storage_change: None,
        status: None,
        immediate_bytes: None,
        decoded: None,
    }
}
