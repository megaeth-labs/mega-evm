//! Hook channel into nested sandbox execution.
//!
//! Keyless sandbox execution is otherwise invisible to a parent inspector. A
//! [`SandboxInspector`] attached via [`crate::MegaContext::set_keyless_sandbox_hook`]
//! sees every hook that revm's [`Inspector`] would see on the sandbox EVM, plus a paired
//! [`sandbox_start`](SandboxInspector::sandbox_start) /
//! [`sandbox_end`](SandboxInspector::sandbox_end) lifecycle. Hook signatures match revm's:
//! `&mut` inputs and override return values are forwarded, so a hook can rewrite inputs,
//! short-circuit `CALL`/`CREATE`, and rewrite outcomes exactly as it could on a top-level
//! EVM. A hook that only observes returns `None` from `call`/`create` (the default bodies)
//! and leaves interpreter and context state alone; that is what read-only means here, as it
//! does for the outer EVM's inspector. [`InspectorBridge`] installs the handle as the sandbox
//! EVM's inspector.
//!
//! # Contract
//!
//! 1. With no hook attached, the sandbox path is unchanged.
//! 2. Attaching a hook without intervening leaves result, state, gas, and usage identical to the
//!    unattached path.
//! 3. Interventions take effect inside the sandbox as they would on a top-level EVM. Reported
//!    `gas_used` and usage are the post-intervention values; the parent frame records them as-is
//!    and does not check conservation. Malformed synthetic outcomes, such as a `memory_offset`
//!    outside the frame's memory, panic exactly as they would on a top-level EVM; the sandbox
//!    neither isolates nor amplifies that.
//! 4. The hook is node-local and non-consensus. An intervening node may diverge from the network;
//!    the caller accepts that risk.
//! 5. Later specs measure interventions and refuse some shapes. Integrators must not depend on this
//!    base being permissive.
//!
//! # Lifecycle
//!
//! `sandbox_start` and `sandbox_end` fire exactly once per sandbox attempt, and only when a
//! hook is attached. An attempt begins once the keyless payload has been decoded, its signer
//! recovered, the deploy address derived and found free, and the gas budget admitted; a call
//! that is rejected before that point emits no events. From then on the pair is guaranteed: a
//! sandbox transaction that revm's validation rejects without ever constructing a sandbox EVM
//! still delivers `sandbox_end` with [`SandboxRejectKind::Rejected`]. Reverted inner frames
//! still emit their events; whether sandbox state was applied to the parent is reported by
//! [`SandboxEndOutcome::state_applied`]. Both lifecycle hooks are informational and cannot
//! veto execution.
//!
//! # External-environment invariance
//!
//! Attaching a hook must not change sandbox env semantics at any spec. Pre-REX4 sandboxes
//! always run with [`crate::EmptyExternalEnv`]; REX4+ sandboxes always share the parent env.
//! A hook therefore implements [`SandboxInspector`] for both the parent env type and
//! [`crate::EmptyExternalEnv`]; the setter stores two type-erased handles so hooks fire on
//! both paths, and a sandbox's lifecycle events go to the same slot as its opcode-level
//! hooks (the [`crate::EmptyExternalEnv`] impl pre-REX4, the parent-env impl from REX4 on).
//! A type that implements revm's [`Inspector`] for every sandbox context satisfies both
//! bounds through the blanket impl.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::cell::RefCell;
use std::rc::Rc;

use alloy_primitives::{Address, Bytes, Log, U256};
use revm::{
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        Interpreter,
    },
    Inspector,
};

use crate::{ExternalEnvTypes, MegaContext, MegaSpecId};

use super::state::SandboxDb;

/// Context available when a sandbox is about to execute.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStartInfo {
    /// Spec used by the parent context that started the sandbox.
    pub spec: MegaSpecId,
    /// Recovered signer of the keyless deployment transaction.
    pub signer: Address,
    /// Deterministic deploy address derived from the signer.
    pub deploy_address: Address,
    /// Caller-supplied gas limit override decoded from the payload.
    ///
    /// The ABI value is a `U256` and is saturating-converted to `u64`, so a
    /// payload larger than [`u64::MAX`] is reported as [`u64::MAX`].
    pub gas_limit_override: u64,
    /// Gas limit actually granted to the sandbox after outer-gas capping
    /// (REX5+ caps to the outer frame's remaining gas; pre-REX5 equals
    /// [`Self::gas_limit_override`]).
    pub effective_gas_limit: u64,
    /// Gas limit carried by the signed keyless transaction itself.
    pub tx_gas_limit: u64,
    /// The intercepted `KeylessDeploy` call frame, as an inspector on the outer EVM saw it.
    pub outer_call: OuterCallInfo,
}

/// The intercepted `KeylessDeploy` call that started a sandbox, described with the fields an
/// inspector on the outer EVM records for that frame.
///
/// A tracer that records the outer EVM and the sandbox separately uses this to pair a
/// sandbox with the outer frame it belongs under: the fields match what revm's `call` hook
/// received for the intercepted frame.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OuterCallInfo {
    /// Call depth of the intercepted frame; `0` for a transaction's top-level call.
    pub depth: usize,
    /// `msg.sender` of the intercepted call.
    pub caller: Address,
    /// Gas limit of the intercepted call frame, as handed to it by the outer EVM.
    pub gas_limit: u64,
    /// Gas remaining in the intercepted frame when the sandbox started: after the dispatch
    /// overhead and any REX5+ materialization charge, before the sandbox reservation was
    /// debited.
    pub gas_remaining: u64,
    /// `msg.value` of the intercepted call.
    pub value: U256,
    /// Full calldata of the intercepted call.
    pub data: Bytes,
}

/// Terminal outcome of one sandbox execution, delivered exactly once.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxEndOutcome {
    /// Sandbox completed and its state was applied to the parent journal.
    Applied {
        /// How the sandbox EVM completed.
        completion: SandboxCompletionKind,
        /// Gas consumed by the sandbox EVM.
        gas_used: u64,
    },
    /// Sandbox ran but its state was not applied.
    NotApplied {
        /// Why the sandbox state was discarded.
        reason: SandboxRejectKind,
    },
}

/// Completion kind for a sandbox whose state was applied to the parent.
///
/// The kind describes what the sandbox EVM did, on every spec alike. What the outer
/// caller is told differs by spec only for [`Self::EmptyCode`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxCompletionKind {
    /// Inner CREATE succeeded with non-empty runtime bytecode.
    Deployed,
    /// Inner CREATE ran to a successful exit but left no code at the deploy address:
    /// it returned empty runtime bytecode, or (REX6+) the account self-destructed in the
    /// same transaction.
    ///
    /// The outer caller sees `EmptyCodeDeployed` errorData on every spec. REX5+ forwards
    /// the constructor's logs into the parent receipt; pre-REX5 drops them.
    EmptyCode,
    /// Sandbox EVM execution reverted or halted after producing mergeable state.
    ExecutionFailed,
}

/// Reason a completed or aborted sandbox did not apply state to the parent.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxRejectKind {
    /// Validate-reject or internal error before a mergeable frame was produced, or a
    /// top-level create result that carries no address. Only a hook's `create` override
    /// produces the latter; it is charged like [`Self::AddressMismatch`] and nothing is
    /// applied.
    Rejected,
    /// REX5+ post-execution resource accounting rejected the sandbox.
    PostAccountingHalt,
    /// Applying sandbox state to the parent journal failed.
    ApplyFailed,
    /// Deployed address did not match the derived address.
    AddressMismatch,
}

impl SandboxEndOutcome {
    /// Returns `true` when sandbox state was applied to the parent journal.
    pub fn state_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

/// Object-safe hook into nested sandbox execution.
///
/// Signatures match [`Inspector`]`<`[`MegaContext`]`<`[`SandboxDb`]`<'_>, ExtEnvs>,
/// `[`EthInterpreter`]`>`. All hooks have empty / `None` defaults so adding a hook is not a
/// breaking change, and a type that overrides nothing but the hooks it reads from observes
/// without intervening. Method-level lifetimes on [`MegaContext`]`<`[`SandboxDb`]`<'_>, _>`
/// keep the trait object-safe.
///
/// Types that already implement [`Inspector`] for every sandbox context lifetime
/// receive a blanket [`SandboxInspector`] impl. Local types that are not inspectors
/// may still implement this trait by hand.
pub trait SandboxInspector<ExtEnvs: ExternalEnvTypes> {
    /// Called before the sandbox interpreter is initialized.
    #[inline]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
    ) {
        let _ = interp;
        let _ = context;
    }

    /// Called before each opcode executes.
    #[inline]
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
    ) {
        let _ = interp;
        let _ = context;
    }

    /// Called after each opcode executes.
    #[inline]
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
    ) {
        let _ = interp;
        let _ = context;
    }

    /// Called when a log is emitted inside the sandbox.
    #[inline]
    fn log(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        log: Log,
    ) {
        let _ = interp;
        let _ = context;
        let _ = log;
    }

    /// Called when a call frame is about to start.
    ///
    /// Returning `Some` overrides the call, matching [`Inspector::call`].
    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        let _ = context;
        let _ = inputs;
        None
    }

    /// Called when a call frame has concluded. Mutations of `outcome` are kept.
    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let _ = context;
        let _ = inputs;
        let _ = outcome;
    }

    /// Called when a create frame is about to start.
    ///
    /// Returning `Some` overrides the create, matching [`Inspector::create`].
    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let _ = context;
        let _ = inputs;
        None
    }

    /// Called when a create frame has concluded. Mutations of `outcome` are kept.
    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let _ = context;
        let _ = inputs;
        let _ = outcome;
    }

    /// Called when a contract self-destructs inside the sandbox.
    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        let _ = contract;
        let _ = target;
        let _ = value;
    }

    /// Called once immediately before sandbox execution starts. Cannot veto.
    #[inline]
    fn sandbox_start(&mut self, info: &SandboxStartInfo) {
        let _ = info;
    }

    /// Called once with the terminal sandbox outcome. Paired with [`Self::sandbox_start`].
    /// Cannot veto.
    #[inline]
    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        let _ = outcome;
    }
}

impl<I, E> SandboxInspector<E> for I
where
    E: ExternalEnvTypes,
    I: for<'a> Inspector<MegaContext<SandboxDb<'a>, E>, EthInterpreter>,
{
    #[inline]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        Inspector::initialize_interp(self, interp, context);
    }

    #[inline]
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        Inspector::step(self, interp, context);
    }

    #[inline]
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        Inspector::step_end(self, interp, context);
    }

    #[inline]
    fn log(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        log: Log,
    ) {
        Inspector::log(self, interp, context, log);
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        Inspector::call(self, context, inputs)
    }

    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        Inspector::call_end(self, context, inputs, outcome);
    }

    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        Inspector::create(self, context, inputs)
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        Inspector::create_end(self, context, inputs, outcome);
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        Inspector::selfdestruct(self, contract, target, value);
    }
}

/// Inspector that forwards sandbox frames to a [`SandboxInspector`].
///
/// `call` / `create` return the inner override; `call_end` / `create_end` pass
/// `&mut outcome` through so mutations are visible to the EVM.
pub(crate) struct InspectorBridge<E: ExternalEnvTypes> {
    inspector: Rc<RefCell<dyn SandboxInspector<E>>>,
}

impl<E: ExternalEnvTypes> core::fmt::Debug for InspectorBridge<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InspectorBridge").finish_non_exhaustive()
    }
}

impl<E: ExternalEnvTypes> InspectorBridge<E> {
    /// Wraps a shared inspector handle as a revm inspector.
    pub(crate) fn new(inspector: Rc<RefCell<dyn SandboxInspector<E>>>) -> Self {
        Self { inspector }
    }
}

impl<E: ExternalEnvTypes> Inspector<MegaContext<SandboxDb<'_>, E>, EthInterpreter>
    for InspectorBridge<E>
{
    #[inline]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.inspector.borrow_mut().initialize_interp(interp, context);
    }

    #[inline]
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.inspector.borrow_mut().step(interp, context);
    }

    #[inline]
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.inspector.borrow_mut().step_end(interp, context);
    }

    #[inline]
    fn log(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        log: Log,
    ) {
        self.inspector.borrow_mut().log(interp, context, log);
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.inspector.borrow_mut().call(context, inputs)
    }

    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.inspector.borrow_mut().call_end(context, inputs, outcome);
    }

    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.inspector.borrow_mut().create(context, inputs)
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.inspector.borrow_mut().create_end(context, inputs, outcome);
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.inspector.borrow_mut().selfdestruct(contract, target, value);
    }
}

/// The type-erased hook slot for nested sandbox execution. Setting a hook replaces the
/// previous one.
pub(crate) type SandboxHookHandle<E> = Rc<RefCell<dyn SandboxInspector<E>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmptyExternalEnv, TestExternalEnvs};
    use std::boxed::Box;

    struct LocalInspector;

    impl<E: ExternalEnvTypes> SandboxInspector<E> for LocalInspector {}

    struct GenericInspector;

    impl<CTX> Inspector<CTX> for GenericInspector {}

    #[test]
    fn test_sandbox_end_outcome_reports_whether_state_was_applied() {
        let applied =
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, gas_used: 1 };
        assert!(applied.state_applied());

        let not_applied = SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::Rejected };
        assert!(!not_applied.state_applied());
    }

    #[test]
    fn test_sandbox_inspector_is_object_safe() {
        let _boxed: Box<dyn SandboxInspector<EmptyExternalEnv>> = Box::new(LocalInspector);
        let _rc: Rc<RefCell<dyn SandboxInspector<EmptyExternalEnv>>> =
            Rc::new(RefCell::new(LocalInspector));
    }

    #[test]
    fn test_local_sandbox_inspector_coexists_with_blanket() {
        fn assert_inspector<E: ExternalEnvTypes, T: SandboxInspector<E>>(_: T) {}
        assert_inspector::<EmptyExternalEnv, _>(LocalInspector);
        assert_inspector::<EmptyExternalEnv, _>(GenericInspector);
    }

    #[test]
    fn test_blanket_inspector_satisfies_empty_and_parent_env_bounds() {
        fn assert_dual<
            T: SandboxInspector<EmptyExternalEnv> + SandboxInspector<TestExternalEnvs>,
        >(
            _: T,
        ) {
        }
        assert_dual(GenericInspector);
        assert_dual(LocalInspector);
    }

    #[test]
    fn test_inspector_bridge_debug_names_the_bridge() {
        use std::format;
        let inspector: Rc<RefCell<dyn SandboxInspector<EmptyExternalEnv>>> =
            Rc::new(RefCell::new(LocalInspector));
        let rendered = format!("{:?}", InspectorBridge::new(inspector));
        assert!(rendered.starts_with("InspectorBridge"), "{rendered}");
    }
}
