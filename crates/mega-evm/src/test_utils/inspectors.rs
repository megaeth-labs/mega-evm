#[cfg(not(feature = "std"))]
use alloc as std;
use std::{rc::Rc, vec::Vec};

use core::cell::RefCell;
use revm::{
    bytecode::OpCode,
    context::{ContextTr, JournalTr},
    interpreter::{
        interpreter_types::Jumps, CallInputs, CallOutcome, CreateInputs, Interpreter,
        InterpreterTypes,
    },
    Inspector,
};

/// A smart pointer wrapper around `Rc<RefCell<T>>` that provides shared mutable access.
///
/// This type is used throughout the trace tree to allow multiple references to the same
/// node while maintaining interior mutability. It automatically derefs to the inner
/// `Rc<RefCell<T>>`.
#[derive(Debug, derive_more::Deref)]
pub struct Pointer<T>(#[deref] Rc<RefCell<T>>);

impl<T> Clone for Pointer<T> {
    /// Clones the pointer (increments reference count), not the underlying data.
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> From<T> for Pointer<T> {
    /// Wraps a value in a new `Pointer`.
    fn from(value: T) -> Self {
        Self(Rc::new(RefCell::new(value)))
    }
}

impl<T: Clone> Pointer<T> {
    /// Clones the underlying data (not just the pointer).
    pub fn clone_inner(&self) -> T {
        self.0.borrow().clone()
    }
}

/// A path to a specific node in the trace tree.
///
/// Each element represents the child index at that level of the tree.
/// For example, `vec![0, 2, 1]` means: first child -> third child -> second child.
pub type NodeLocation = Vec<usize>;

/// A location of an item within a node's sections.
///
/// `(section_index, item_index)` where sections are interleaved with child calls.
pub type ItemLocation = (usize, usize);

/// A complete location representing a specific item in the trace tree.
///
/// Combines a node location (path through call tree) with an item location
/// (position within that node's items).
#[derive(Debug)]
pub struct TraceLocation {
    /// Path to the node containing the item.
    pub call: NodeLocation,
    /// Position of the item within the node.
    pub item: ItemLocation,
}

/// A node in a hierarchical trace tree structure.
///
/// This represents a call frame (either a message call or contract creation) and maintains
/// a tree structure where:
/// - Each node contains metadata about the call (via `META`)
/// - Items (via `ITEM`) are stored in sections that are interleaved with child calls
/// - The structure allows reconstructing the exact chronological order of events
///
/// # Structure
///
/// Items are organized into sections, where sections are separated by child calls:
/// ```text
/// [items before first child] -> [first child] -> [items after first child] -> [second child] -> ...
/// ```
///
/// This allows the tree to maintain both hierarchical structure and chronological ordering.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TraceNode<ITEM, META> {
    /// Metadata about this call (e.g., call inputs, creation inputs).
    pub meta: META,
    /// Reference to the parent node, if any.
    pub parent: Option<Pointer<TraceNode<ITEM, META>>>,
    /// Sections of items. There is always one more section than children.
    /// Sections are interleaved with children to maintain chronological order.
    pub sections: Vec<Vec<Pointer<ITEM>>>,
    /// Child nodes representing nested calls.
    pub childrens: Vec<Pointer<TraceNode<ITEM, META>>>,
}

impl<ITEM, META> TraceNode<ITEM, META> {
    /// Creates a new trace node with the given metadata and optional parent.
    ///
    /// If a parent is provided, this node is automatically added as a child of the parent.
    pub fn new(meta: META, parent: Option<Pointer<Self>>) -> Pointer<Self> {
        let this: Pointer<Self> =
            Self { meta, parent: parent.clone(), sections: vec![Vec::new()], childrens: vec![] }
                .into();
        if let Some(p) = parent {
            p.borrow_mut().add_child(this.clone());
        }
        this
    }

    /// Returns the last item added to this node across all sections.
    pub fn last_item(&self) -> Option<Pointer<ITEM>> {
        self.sections.iter().flatten().last().cloned()
    }

    /// Adds an item to the current section (after the last child).
    pub fn add_item(&mut self, item: ITEM) {
        self.sections[self.childrens.len()].push(item.into());
    }

    /// Adds a child node and creates a new section for subsequent items.
    fn add_child<T: Into<Pointer<Self>>>(&mut self, child: T) {
        self.childrens.push(child.into());
        self.sections.push(Vec::new());
    }
}

impl<ITEM, META> Pointer<TraceNode<ITEM, META>> {
    /// Returns a flattened view of all items in the tree in chronological order.
    pub fn flattened_items(&self) -> Vec<Pointer<ITEM>> {
        let mut items = Vec::new();
        for i in 0..self.borrow().childrens.len() {
            items.extend(self.borrow().sections[i].iter().cloned());
            items.extend(self.borrow().childrens[i].flattened_items());
        }
        items.extend(self.borrow().sections[self.borrow().childrens.len()].iter().cloned());
        items
    }

    /// Retrieves a node at the specified location in the tree.
    ///
    /// Returns `None` if the location is invalid (out of bounds).
    pub fn get_node(&self, location: NodeLocation) -> Option<Self> {
        if location.is_empty() {
            Some(self.clone())
        } else {
            self.borrow().childrens[location[0]].get_node(location[1..].to_vec())
        }
    }

    /// Retrieves an item at the specified location within this node.
    ///
    /// Returns `None` if the location is invalid (out of bounds).
    pub fn get_item(&self, location: ItemLocation) -> Option<Pointer<ITEM>> {
        self.borrow().sections.get(location.0).and_then(|section| section.get(location.1)).cloned()
    }

    /// Retrieves an item using a complete trace location (node path + item position).
    ///
    /// Returns `None` if either the node location or item location is invalid.
    pub fn get_item_nested(&self, location: TraceLocation) -> Option<Pointer<ITEM>> {
        self.get_node(location.call).and_then(|n| n.get_item(location.item))
    }

    /// Iterates over all items in the tree in chronological order.
    ///
    /// For each item, calls the provided function with:
    /// - The node location (path to the node containing the item)
    /// - The node itself
    /// - The item location within the node
    /// - The item
    pub fn iterate_with<F>(&self, mut f: F)
    where
        F: FnMut(NodeLocation, Self, ItemLocation, Pointer<ITEM>),
    {
        self.iterate_with_inner(&mut f, vec![]);
    }

    /// Internal helper for recursive iteration maintaining the current node location.
    fn iterate_with_inner<F>(&self, f: &mut F, node_location: NodeLocation)
    where
        F: FnMut(NodeLocation, Self, ItemLocation, Pointer<ITEM>),
    {
        let sections = &self.borrow().sections;
        for (section_idx, section) in sections.iter().enumerate() {
            for (item_idx, item) in section.iter().enumerate() {
                f(node_location.clone(), self.clone(), (section_idx, item_idx), item.clone());
            }
            if section_idx < sections.len() - 1 {
                let mut child_node_location = node_location.clone();
                child_node_location.push(section_idx);
                self.borrow().childrens[section_idx].iterate_with_inner(f, child_node_location);
            }
        }
    }
}

/// Represents gas information for a single opcode execution.
#[derive(Clone)]
pub struct OpcodeGasInfo {
    /// The opcode executed.
    pub opcode: OpCode,
    /// Gas remaining before the opcode execution.
    pub gas_before: u64,
    /// Gas remaining after the opcode execution.
    pub gas_after: u64,
    /// Call depth when this opcode was executed.
    pub depth: u64,
}

impl core::fmt::Debug for OpcodeGasInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}[depth={}] gas: {} -> {} (cost: {})",
            self.opcode.as_str(),
            self.depth,
            self.gas_before,
            self.gas_after,
            self.gas_cost()
        )
    }
}

impl OpcodeGasInfo {
    /// Calculates the gas cost of this opcode execution.
    pub fn gas_cost(&self) -> u64 {
        self.gas_before.saturating_sub(self.gas_after)
    }
}

/// Metadata for a trace node representing either a call or create operation.
#[derive(Debug, Clone)]
pub enum MsgCallMeta {
    /// A message call (CALL, CALLCODE, DELEGATECALL, STATICCALL).
    Call(CallInputs),
    /// A contract creation (CREATE, CREATE2).
    Create(CreateInputs),
}

/// An inspector that collects a hierarchical view of gas left before and after each opcode.
#[derive(Debug, Default)]
pub struct GasInspector {
    /// Stack of currently active call frames.
    pub stack: Vec<Pointer<TraceNode<OpcodeGasInfo, MsgCallMeta>>>,
    /// Root of the trace tree containing all opcode executions.
    pub trace: Option<Pointer<TraceNode<OpcodeGasInfo, MsgCallMeta>>>,
}

impl GasInspector {
    /// Creates a new enabled `GasInspector`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears all recorded gas information.
    pub fn clear(&mut self) {
        self.trace = None;
        self.stack = vec![];
    }

    /// Returns a flattened view of all opcode records in chronological order.
    /// This traverses the call tree and collects all opcodes in execution order.
    pub fn records(&self) -> Vec<OpcodeGasInfo> {
        self.trace
            .as_ref()
            .map(|t| t.flattened_items().into_iter().map(|p| p.clone_inner()).collect())
            .unwrap_or_default()
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for GasInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        // Create a new trace node for this call
        let parent = self.stack.last().cloned();
        let msg_call = TraceNode::new(MsgCallMeta::Call(inputs.clone()), parent);
        self.stack.push(msg_call.clone());
        // Set as root trace if this is the first call
        if self.trace.is_none() {
            self.trace = Some(msg_call);
        }
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        // Pop the call from the stack when it completes
        self.stack.pop();
    }

    fn create(
        &mut self,
        _context: &mut CTX,
        inputs: &mut CreateInputs,
    ) -> Option<revm::interpreter::CreateOutcome> {
        // Create a new trace node for this contract creation
        let parent = self.stack.last().cloned();
        let msg_call = TraceNode::new(MsgCallMeta::Create(inputs.clone()), parent);
        self.stack.push(msg_call.clone());
        // Set as root trace if this is the first operation
        if self.trace.is_none() {
            self.trace = Some(msg_call);
        }
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut revm::interpreter::CreateOutcome,
    ) {
        // Pop the create from the stack when it completes
        self.stack.pop();
    }

    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        // Record gas state before opcode execution
        let gas_before = interp.gas.remaining();
        let opcode = interp.bytecode.opcode();
        let depth = context.journal().depth();
        let step = OpcodeGasInfo {
            opcode: OpCode::new(opcode).unwrap(),
            gas_before,
            gas_after: gas_before, // Will be updated in step_end
            depth: depth as u64,
        };
        if let Some(c) = self.stack.last_mut() {
            c.borrow_mut().add_item(step)
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        // Update the gas_after field with the actual gas remaining after execution
        let current = self
            .stack
            .last()
            .as_ref()
            .and_then(|c| c.borrow_mut().last_item())
            .expect("current is not None");
        current.borrow_mut().gas_after = interp.gas.remaining();
    }
}
