//! Read-only observation channel for nested sandbox execution.
//!
//! A [`SandboxObserver`] sees every interpreter hook that revm's [`Inspector`]
//! would see inside a sandbox, plus a paired [`SandboxObserver::sandbox_start`] /
//! [`SandboxObserver::sandbox_end`] lifecycle. Observation cannot short-circuit
//! `CALL`/`CREATE`: those hooks have no return value, and [`ObserverBridge`]
//! always forwards `None` to the EVM.
//!
//! # Read-only contract
//!
//! Implementations must not mutate interpreter or context state through the
//! `&mut` hook arguments. Doing so is not structurally prevented for every
//! argument (the signatures match revm so generic inspectors can adapt) and
//! may cause consensus divergence; debug builds already trip conservation
//! asserts on many such mutations.
//!
//! # Lifecycle
//!
//! [`sandbox_start`](SandboxObserver::sandbox_start) and
//! [`sandbox_end`](SandboxObserver::sandbox_end) fire exactly once per sandbox
//! that begins execution, and only when an observer is attached. Reverted
//! inner frames still emit their events; whether sandbox state was applied to
//! the parent is reported by [`SandboxEndOutcome::state_applied`].

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
    /// Gas limit override actually supplied to the sandbox (after any cap).
    pub gas_limit_override: u64,
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
/// INVARIANT: hooks observe; they must not mutate execution state. [`ObserverBridge`]
/// structurally drops `call`/`create` override outcomes.
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
    fn call(&mut self, context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>, inputs: &mut CallInputs) {
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
    fn create(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, ExtEnvs>,
        inputs: &mut CreateInputs,
    ) {
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
    #[inline]
    fn sandbox_end(&mut self, outcome: &SandboxEndOutcome) {
        let _ = outcome;
    }
}

/// Adapts any HRTB-compatible revm [`Inspector`] into a [`SandboxObserver`].
///
/// `call`/`create` override outcomes from the inner inspector are discarded.
/// [`SandboxObserver::sandbox_start`] and [`SandboxObserver::sandbox_end`] keep
/// their default empty implementations.
pub struct InspectorSandboxObserver<I>(pub I);

impl<I> core::fmt::Debug for InspectorSandboxObserver<I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("InspectorSandboxObserver").finish_non_exhaustive()
    }
}

impl<I, E> SandboxObserver<E> for InspectorSandboxObserver<I>
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
        self.0.initialize_interp(interp, context);
    }

    #[inline]
    fn step(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.0.step(interp, context);
    }

    #[inline]
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
    ) {
        self.0.step_end(interp, context);
    }

    #[inline]
    fn log(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        log: Log,
    ) {
        self.0.log(interp, context, log);
    }

    #[inline]
    fn call(&mut self, context: &mut MegaContext<SandboxDb<'_>, E>, inputs: &mut CallInputs) {
        let _ = self.0.call(context, inputs);
    }

    #[inline]
    fn call_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CallInputs,
        outcome: &CallOutcome,
    ) {
        // Clone so a mutating inner inspector cannot write back into execution.
        let mut outcome = outcome.clone();
        self.0.call_end(context, inputs, &mut outcome);
    }

    #[inline]
    fn create(&mut self, context: &mut MegaContext<SandboxDb<'_>, E>, inputs: &mut CreateInputs) {
        let _ = self.0.create(context, inputs);
    }

    #[inline]
    fn create_end(
        &mut self,
        context: &mut MegaContext<SandboxDb<'_>, E>,
        inputs: &CreateInputs,
        outcome: &CreateOutcome,
    ) {
        // Clone so a mutating inner inspector cannot write back into execution.
        let mut outcome = outcome.clone();
        self.0.create_end(context, inputs, &mut outcome);
    }

    #[inline]
    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.0.selfdestruct(contract, target, value);
    }
}

/// Inspector that forwards sandbox frames to a [`SandboxObserver`].
///
/// `call` and `create` always return `None`, so the observer has no channel
/// that can override execution.
pub(crate) struct ObserverBridge<E: ExternalEnvTypes> {
    observer: Rc<RefCell<dyn SandboxObserver<E>>>,
}

impl<E: ExternalEnvTypes> core::fmt::Debug for ObserverBridge<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ObserverBridge").finish_non_exhaustive()
    }
}

impl<E: ExternalEnvTypes> ObserverBridge<E> {
    /// Wraps a shared observer handle as a revm inspector.
    pub(crate) fn new(observer: Rc<RefCell<dyn SandboxObserver<E>>>) -> Self {
        Self { observer }
    }
}

impl<E: ExternalEnvTypes> Inspector<MegaContext<SandboxDb<'_>, E>, EthInterpreter>
    for ObserverBridge<E>
{
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
    fn test_inspector_adapter_accepts_generic_inspector() {
        fn assert_observer<E: ExternalEnvTypes, T: SandboxObserver<E>>(_: T) {}
        assert_observer::<EmptyExternalEnv, _>(InspectorSandboxObserver(GenericInspector));
    }
}
