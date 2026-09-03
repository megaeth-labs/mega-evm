//! Shared helpers for tracing nested sandbox execution with `revm-inspectors`.
//!
//! A [`TracingInspector`] attached to the outer EVM never sees sandbox frames: the sandbox
//! is a separate EVM whose journal reports its top-level CREATE at depth 0, and recording that
//! into the outer arena would overwrite the CALL root. The pattern here is two inspectors —
//! the outer one installed on the EVM and a second one attached through the keyless sandbox
//! observer channel — followed by [`splice_sandbox_traces`], which grafts the sandbox arena
//! under the intercepted `KeylessDeploy` CALL after the transaction returns.
//!
//! Requires the `inspectors` feature.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::cell::{Ref, RefCell, RefMut};
use std::{rc::Rc, vec::Vec};

use alloy_primitives::{Address, Bytes, Log, U256};
use revm::{
    bytecode::opcode::{self, OpCode},
    context::ContextTr,
    inspector::JournalExt,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    Inspector,
};
use revm_inspectors::tracing::{
    types::{CallTraceStep, TraceMemberOrder},
    TracingInspector,
};

use crate::KEYLESS_DEPLOY_ADDRESS;

/// [`Inspector`] adapter over a [`TracingInspector`] shared via [`Rc`]/[`RefCell`].
///
/// One handle serves as the outer inspector; a second, independent handle is attached to the
/// sandbox through [`Self::as_sandbox_observer`]. Both are `Clone`, so a caller can keep a
/// handle to read the recorded traces after execution.
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

    /// Observer handle for the read-only keyless sandbox channel.
    pub fn as_sandbox_observer(&self) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(self.clone()))
    }
}

impl<CTX> Inspector<CTX> for SharedTracingInspector
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.0.borrow_mut().initialize_interp(interp, context);
    }

    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.0.borrow_mut().step(interp, context);
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.0.borrow_mut().step_end(interp, context);
    }

    fn log(&mut self, interp: &mut Interpreter, context: &mut CTX, log: Log) {
        self.0.borrow_mut().log(interp, context, log);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.0.borrow_mut().call(context, inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.0.borrow_mut().call_end(context, inputs, outcome);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.0.borrow_mut().create(context, inputs)
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.0.borrow_mut().create_end(context, inputs, outcome);
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

/// Returns true when `sandbox` recorded at least one completed frame.
pub fn sandbox_has_frames(sandbox: &TracingInspector) -> bool {
    has_frames(sandbox)
}

/// Whether `inspector` recorded any frame. A fresh or fused arena still holds one default
/// root node, so emptiness is judged by content, not by node count.
fn has_frames(inspector: &TracingInspector) -> bool {
    inspector.traces().nodes().iter().any(|node| {
        node.trace.status.is_some() || !node.trace.steps.is_empty() || !node.children.is_empty()
    })
}

/// Attaches `sandbox` traces as children of the outer [`KEYLESS_DEPLOY_ADDRESS`] CALL.
///
/// Sandbox execution uses a fresh journal whose top-level CREATE is depth 0. Recording that
/// into the outer inspector would replace the CALL root, so the sandbox is traced on a
/// sibling inspector and grafted here. Depths and arena indices are rewritten so the Geth
/// call tree hangs the CREATE under the CALL. When the sandbox recorded nothing, `outer` is
/// left untouched.
///
/// Geth-style struct logs only descend into a child when the parent frame has a matching
/// call-like opcode in its steps. An intercepted `KeylessDeploy` CALL records no bytecode, so
/// a CREATE step is grafted onto the parent; without it the sandbox constructor steps would
/// be omitted from struct-log output instead of nested in execution order.
///
/// An `outer` without a `KeylessDeploy` CALL frame is also left untouched. That covers an
/// arena that recorded no frames at all (already fused, or never run) and one that traced a
/// transaction which never entered the sandbox while `sandbox` still holds frames from an
/// earlier transaction; grafting under the default root or under an unrelated frame would
/// invent a call tree the outer EVM never executed.
pub fn splice_sandbox_traces(outer: &mut TracingInspector, sandbox: &TracingInspector) {
    if !has_frames(sandbox) || !has_frames(outer) {
        return;
    }

    let Some(parent_idx) = outer.traces().nodes().iter().rposition(|node| {
        node.trace.address == KEYLESS_DEPLOY_ADDRESS && !node.trace.kind.is_any_create()
    }) else {
        return;
    };
    let depth_offset = outer.traces().nodes()[parent_idx].trace.depth + 1;
    let base = outer.traces().nodes().len();

    let remapped: Vec<_> = sandbox
        .traces()
        .nodes()
        .iter()
        .cloned()
        .enumerate()
        .map(|(old_idx, mut node)| {
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
        })
        .collect();

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
        let gas_remaining = parent.trace.gas_limit;
        parent.trace.steps.push(intercepted_create_step(create_depth, contract, gas_remaining));
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
fn intercepted_create_step(depth: u64, contract: Address, gas_remaining: u64) -> CallTraceStep {
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
        gas_cost: 0,
        storage_change: None,
        status: None,
        immediate_bytes: None,
        decoded: None,
    }
}
