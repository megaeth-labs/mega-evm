//! Read-only observation channel for nested sandbox execution.
//!
//! Keyless sandbox execution has two hook channels. This module is the
//! **read-only default**: attach via [`crate::MegaContext::set_keyless_sandbox_observer`].
//! The rewriting channel is [`crate::sandbox::SandboxInspector`], attached explicitly
//! via [`crate::MegaContext::set_keyless_sandbox_inspector`]. The two occupy the same
//! slot; attaching one replaces the other.
//!
//! A [`SandboxObserver`] sees every interpreter hook that revm's [`Inspector`]
//! would see inside a sandbox, plus a paired [`SandboxObserver::sandbox_start`] /
//! [`SandboxObserver::sandbox_end`] lifecycle. Observation cannot short-circuit
//! `CALL`/`CREATE`: those hooks have no return value, and the sandbox installs
//! the observer behind `ReadOnlyHook`, which always answers `None` to the EVM.
//!
//! # Read-only contract
//!
//! Two layers close intervention:
//!
//! - **Structural.** [`SandboxObserver::call`] / [`SandboxObserver::create`] take shared references
//!   to [`CallInputs`] / [`CreateInputs`]. The blanket impl for revm [`Inspector`]s clones a
//!   temporary copy when forwarding; mutations of that copy are discarded, as are `Some` override
//!   outcomes. Combined with `ReadOnlyHook` always returning `None`, rewriting inputs and
//!   short-circuiting the frame are both closed.
//! - **Contractual.** [`SandboxObserver::step`] / [`SandboxObserver::step_end`] take `&mut
//!   Interpreter` and hooks take `&mut MegaContext` because those types are not cheaply cloneable.
//!   Implementations must not mutate them. Debug builds already trip conservation asserts on many
//!   such mutations.
//!
//! Attaching a compliant (read-only) observer does not change sandbox execution
//! results. No such guarantee is made for an observer that mutates interpreter
//! or context state.
//!
//! # Lifecycle
//!
//! [`sandbox_start`](SandboxObserver::sandbox_start) and
//! [`sandbox_end`](SandboxObserver::sandbox_end) fire exactly once per sandbox
//! attempt, and only when an observer is attached. A validate-reject path that
//! never constructs a sandbox EVM still delivers the pair. Reverted inner
//! frames still emit their events; whether sandbox state was applied to the
//! parent is reported by [`SandboxEndOutcome::state_applied`].
//!
//! # External-environment invariance
//!
//! Attaching an observer must not change sandbox env semantics at any spec.
//! Pre-REX4 sandboxes always run with [`crate::EmptyExternalEnv`]; REX4+
//! sandboxes always share the parent env. An observer therefore implements
//! [`SandboxObserver`] for both the parent env type and
//! [`crate::EmptyExternalEnv`]; [`crate::MegaContext::set_keyless_sandbox_observer`]
//! stores two type-erased handles so opcode-level hooks fire on both paths. A
//! type that implements revm's [`Inspector`] for every sandbox context satisfies
//! both bounds through the blanket impl.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::cell::RefCell;
use std::rc::Rc;

use alloy_primitives::{Address, Log, U256};
use revm::{
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        Interpreter,
    },
    Inspector,
};

use crate::{ExternalEnvTypes, MegaContext, MegaSpecId};

use super::{
    inspector::{SandboxHookHandle, SandboxInspector},
    state::SandboxDb,
};

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
}

/// Terminal outcome of one sandbox execution, delivered exactly once.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxEndOutcome {
    /// Sandbox completed and its state was applied to the parent journal.
    Applied {
        /// Wire-shape completion reported to the outer caller.
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
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxCompletionKind {
    /// Inner CREATE succeeded with non-empty runtime bytecode.
    Deployed,
    /// Inner CREATE succeeded but returned empty runtime bytecode.
    EmptyCode,
    /// Sandbox EVM execution failed (revert or halt) after producing mergeable state.
    ExecutionFailed,
}

/// Reason a completed or aborted sandbox did not apply state to the parent.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxRejectKind {
    /// Validate-reject or internal error before a mergeable frame was produced.
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

/// Object-safe observer for nested sandbox execution.
///
/// All hooks have empty default implementations so adding a hook is not a
/// breaking change. Method-level lifetimes on [`MegaContext`]`<`[`SandboxDb`]`<'_>, _>`
/// keep the trait object-safe.
///
/// A compliant (read-only) observer does not change sandbox execution results.
/// [`call`](Self::call) / [`create`](Self::create) cannot rewrite inputs or
/// override the frame; [`step`](Self::step) and context arguments remain
/// contractually read-only. `ReadOnlyHook` never answers a `call`/`create`
/// override on the observer's behalf.
///
/// Types that already implement [`Inspector`] for every sandbox context
/// lifetime receive a blanket impl that forwards each hook on a temporary
/// copy of the inputs. Local types that are not inspectors implement this
/// trait by hand.
pub trait SandboxObserver<ExtEnvs: ExternalEnvTypes> {
    /// Called once after the sandbox interpreter is created, before the first opcode.
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

    /// Called when a call frame is about to start. Cannot override the call.
    #[inline]
    fn call(&mut self, context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>, inputs: &CallInputs) {
        let _ = context;
        let _ = inputs;
    }

    /// Called when a call frame has concluded.
    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &CallInputs,
        outcome: &CallOutcome,
    ) {
        let _ = context;
        let _ = inputs;
        let _ = outcome;
    }

    /// Called when a create frame is about to start. Cannot override the create.
    #[inline]
    fn create(&mut self, context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>, inputs: &CreateInputs) {
        let _ = context;
        let _ = inputs;
    }

    /// Called when a create frame has concluded.
    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &CreateInputs,
        outcome: &CreateOutcome,
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

    /// Called once immediately before sandbox execution starts.
    #[inline]
    fn sandbox_start(&mut self, info: &SandboxStartInfo) {
        let _ = info;
    }

    /// Called once with the terminal sandbox outcome. Paired with [`Self::sandbox_start`].
    ///
    /// When an internal database error aborts sandbox execution, the opcode-level
    /// event stream may truncate on an unclosed frame (the upstream inspect path
    /// propagates the error with `?`). [`sandbox_end`](Self::sandbox_end) is still
    /// delivered and is the terminus of the event stream.
    #[inline]
    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        let _ = outcome;
    }
}

impl<I, E> SandboxObserver<E> for I
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
    fn call(&mut self, context: &mut MegaContext<SandboxDb<'_>, E>, inputs: &CallInputs) {
        // The inspector works on a copy: its mutations and any override are discarded.
        let mut inputs = inputs.clone();
        let _ = Inspector::call(self, context, &mut inputs);
    }

    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &CallOutcome,
    ) {
        // Clone so a mutating inspector cannot write back into execution.
        let mut outcome = outcome.clone();
        Inspector::call_end(self, context, inputs, &mut outcome);
    }

    #[inline]
    fn create(&mut self, context: &mut MegaContext<SandboxDb<'_>, E>, inputs: &CreateInputs) {
        let mut inputs = inputs.clone();
        let _ = Inspector::create(self, context, &mut inputs);
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CreateInputs,
        outcome: &CreateOutcome,
    ) {
        let mut outcome = outcome.clone();
        Inspector::create_end(self, context, inputs, &mut outcome);
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        Inspector::selfdestruct(self, contract, target, value);
    }
}

/// Installs a [`SandboxObserver`] on the sandbox's hook slot without a way to intervene.
///
/// Every hook forwards a shared reference to the observer, `call` / `create` always answer
/// `None`, and `call_end` / `create_end` hand their outcomes over read-only, so the observer
/// channel is structurally unable to rewrite execution even though it shares the slot with
/// [`SandboxInspector`].
pub(crate) struct ReadOnlyHook<E: ExternalEnvTypes> {
    observer: Rc<RefCell<dyn SandboxObserver<E>>>,
}

impl<E: ExternalEnvTypes + 'static> ReadOnlyHook<E> {
    /// Wraps a shared observer handle as a type-erased hook handle.
    pub(crate) fn handle(observer: Rc<RefCell<dyn SandboxObserver<E>>>) -> SandboxHookHandle<E> {
        Rc::new(RefCell::new(Self { observer }))
    }
}

impl<E: ExternalEnvTypes> core::fmt::Debug for ReadOnlyHook<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadOnlyHook").finish_non_exhaustive()
    }
}

impl<E: ExternalEnvTypes> SandboxInspector<E> for ReadOnlyHook<E> {
    #[inline]
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.observer.borrow_mut().initialize_interp(interp, context);
    }

    #[inline]
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.observer.borrow_mut().step(interp, context);
    }

    #[inline]
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.observer.borrow_mut().step_end(interp, context);
    }

    #[inline]
    fn log(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        log: Log,
    ) {
        self.observer.borrow_mut().log(interp, context, log);
    }

    #[inline]
    fn call(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.observer.borrow_mut().call(context, inputs);
        None
    }

    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.observer.borrow_mut().call_end(context, inputs, outcome);
    }

    #[inline]
    fn create(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.observer.borrow_mut().create(context, inputs);
        None
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.observer.borrow_mut().create_end(context, inputs, outcome);
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.observer.borrow_mut().selfdestruct(contract, target, value);
    }

    #[inline]
    fn sandbox_start(&mut self, info: &SandboxStartInfo) {
        self.observer.borrow_mut().sandbox_start(info);
    }

    #[inline]
    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        self.observer.borrow_mut().sandbox_end(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmptyExternalEnv;
    use std::boxed::Box;

    struct NopObserver;

    impl<E: ExternalEnvTypes> SandboxObserver<E> for NopObserver {}

    struct GenericInspector;

    impl<CTX> Inspector<CTX> for GenericInspector {}

    #[test]
    fn test_sandbox_observer_is_object_safe() {
        let _boxed: Box<dyn SandboxObserver<EmptyExternalEnv>> = Box::new(NopObserver);
        let _rc: Rc<RefCell<dyn SandboxObserver<EmptyExternalEnv>>> =
            Rc::new(RefCell::new(NopObserver));
    }

    #[test]
    fn test_sandbox_end_outcome_reports_whether_state_was_applied() {
        let applied =
            SandboxEndOutcome::Applied { completion: SandboxCompletionKind::Deployed, gas_used: 1 };
        assert!(applied.state_applied());

        let not_applied = SandboxEndOutcome::NotApplied { reason: SandboxRejectKind::Rejected };
        assert!(!not_applied.state_applied());
    }

    #[test]
    fn test_generic_inspector_is_a_sandbox_observer() {
        fn assert_observer<E: ExternalEnvTypes, T: SandboxObserver<E>>(_: T) {}
        assert_observer::<EmptyExternalEnv, _>(GenericInspector);
        assert_observer::<EmptyExternalEnv, _>(NopObserver);
    }

    #[test]
    fn test_blanket_observer_satisfies_empty_and_parent_env_bounds() {
        use crate::TestExternalEnvs;
        fn assert_dual<T: SandboxObserver<EmptyExternalEnv> + SandboxObserver<TestExternalEnvs>>(
            _: T,
        ) {
        }
        assert_dual(GenericInspector);
        assert_dual(NopObserver);
    }

    #[test]
    fn test_read_only_hook_is_a_sandbox_inspector_handle() {
        let observer: Rc<RefCell<dyn SandboxObserver<EmptyExternalEnv>>> =
            Rc::new(RefCell::new(NopObserver));
        let _handle: SandboxHookHandle<EmptyExternalEnv> = ReadOnlyHook::handle(observer);
    }
}
