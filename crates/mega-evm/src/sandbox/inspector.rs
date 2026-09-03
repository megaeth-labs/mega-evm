//! Rewriting inspector channel for nested sandbox execution.
//!
//! A [`SandboxInspector`] is the intervention counterpart of [`super::SandboxObserver`].
//! Hook signatures match revm's [`Inspector`] on the sandbox EVM: `&mut` inputs and
//! override return values are forwarded, so `CALL`/`CREATE` can be short-circuited and
//! outcomes rewritten. [`InspectorBridge`] installs the handle as the sandbox EVM's
//! inspector. Both channels occupy the same type-erased slot: an observer is installed behind
//! a read-only adapter, so attaching either replaces the other.
//!
//! # Contract
//!
//! 1. With no hook attached, the sandbox path is unchanged.
//! 2. Attaching this channel without intervening leaves result, state, gas, and usage identical to
//!    the unattached path.
//! 3. Interventions take effect inside the sandbox as they would on a top-level EVM. Reported
//!    `gas_used` and usage are the post-intervention values; the parent frame records them as-is
//!    and does not check conservation. Malformed synthetic outcomes, such as a `memory_offset`
//!    outside the frame's memory, panic exactly as they would on a top-level EVM; the sandbox
//!    neither isolates nor amplifies that.
//! 4. The channel is node-local and non-consensus. An intervening node may diverge from the
//!    network; the caller accepts that risk.
//! 5. Later specs measure interventions and refuse some shapes. Integrators must not depend on this
//!    base being permissive.
//!
//! `sandbox_start` / `sandbox_end` are informational and cannot veto execution.

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

use crate::{ExternalEnvTypes, MegaContext};

use super::{
    observer::{SandboxEndOutcome, SandboxStartInfo},
    state::SandboxDb,
};

/// Object-safe rewriting inspector for nested sandbox execution.
///
/// Signatures match [`Inspector`]`<`[`MegaContext`]`<`[`SandboxDb`]`<'_>, ExtEnvs>,
/// `[`EthInterpreter`]`>`. All hooks have empty / `None` defaults so adding a hook is not a
/// breaking change. Method-level lifetimes on [`MegaContext`]`<`[`SandboxDb`]`<'_>, _>` keep the
/// trait object-safe.
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

/// The type-erased hook slot for nested sandbox execution.
///
/// Both channels share it: an observer is installed behind
/// [`super::observer::ReadOnlyHook`], an inspector directly. Attaching one replaces the other.
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
