//! Trait view of the keyless sandbox hook setters.
//!
//! [`crate::MegaContext`], [`crate::MegaEvm`], and [`crate::MegaBlockExecutor`] each expose the
//! same setters as inherent methods. Callers that only hold the EVM behind a generic (for
//! example a node's `ConfigureEvm::Evm` projection) cannot name those inherent methods, so
//! [`KeylessSandboxHooks`] restates them as a trait bound.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::cell::RefCell;
use std::rc::Rc;

use alloy_evm::Database;
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;

use super::{SandboxInspector, SandboxObserver};
use crate::{EmptyExternalEnv, ExternalEnvTypes, MegaBlockExecutor, MegaContext, MegaEvm};

/// Attaches or detaches the keyless sandbox hook on a host that runs sandboxes.
///
/// Both channels share one slot: attaching either replaces the other. See
/// [`SandboxObserver`] for the read-only channel and [`SandboxInspector`] for the rewriting
/// channel.
pub trait KeylessSandboxHooks {
    /// External environment types the host's parent context is typed against.
    type ExtEnvs: ExternalEnvTypes;

    /// Attaches a read-only observer on every spec.
    fn set_keyless_sandbox_observer<O>(&mut self, observer: Option<Rc<RefCell<O>>>)
    where
        O: SandboxObserver<Self::ExtEnvs> + SandboxObserver<EmptyExternalEnv> + 'static;

    /// Attaches a read-only observer that only implements the parent env type.
    ///
    /// Pre-REX4 sandboxes emit only `sandbox_start` / `sandbox_end` through this handle.
    fn set_keyless_sandbox_observer_for_parent_env(
        &mut self,
        observer: Option<Rc<RefCell<dyn SandboxObserver<Self::ExtEnvs>>>>,
    );

    /// Attaches a rewriting inspector on every spec.
    fn set_keyless_sandbox_inspector<I>(&mut self, inspector: Option<Rc<RefCell<I>>>)
    where
        I: SandboxInspector<Self::ExtEnvs> + SandboxInspector<EmptyExternalEnv> + 'static;

    /// Attaches a rewriting inspector that only implements the parent env type.
    ///
    /// Pre-REX4 sandboxes emit only `sandbox_start` / `sandbox_end` through this handle.
    fn set_keyless_sandbox_inspector_for_parent_env(
        &mut self,
        inspector: Option<Rc<RefCell<dyn SandboxInspector<Self::ExtEnvs>>>>,
    );

    /// Detaches any sandbox hook from both env-type slots.
    fn clear_keyless_sandbox_hook(&mut self);
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> KeylessSandboxHooks for MegaContext<DB, ExtEnvs> {
    type ExtEnvs = ExtEnvs;

    fn set_keyless_sandbox_observer<O>(&mut self, observer: Option<Rc<RefCell<O>>>)
    where
        O: SandboxObserver<ExtEnvs> + SandboxObserver<EmptyExternalEnv> + 'static,
    {
        MegaContext::set_keyless_sandbox_observer(self, observer);
    }

    fn set_keyless_sandbox_observer_for_parent_env(
        &mut self,
        observer: Option<Rc<RefCell<dyn SandboxObserver<ExtEnvs>>>>,
    ) {
        MegaContext::set_keyless_sandbox_observer_for_parent_env(self, observer);
    }

    fn set_keyless_sandbox_inspector<I>(&mut self, inspector: Option<Rc<RefCell<I>>>)
    where
        I: SandboxInspector<ExtEnvs> + SandboxInspector<EmptyExternalEnv> + 'static,
    {
        MegaContext::set_keyless_sandbox_inspector(self, inspector);
    }

    fn set_keyless_sandbox_inspector_for_parent_env(
        &mut self,
        inspector: Option<Rc<RefCell<dyn SandboxInspector<ExtEnvs>>>>,
    ) {
        MegaContext::set_keyless_sandbox_inspector_for_parent_env(self, inspector);
    }

    fn clear_keyless_sandbox_hook(&mut self) {
        MegaContext::clear_keyless_sandbox_hook(self);
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> KeylessSandboxHooks
    for MegaEvm<DB, INSP, ExtEnvs>
{
    type ExtEnvs = ExtEnvs;

    fn set_keyless_sandbox_observer<O>(&mut self, observer: Option<Rc<RefCell<O>>>)
    where
        O: SandboxObserver<ExtEnvs> + SandboxObserver<EmptyExternalEnv> + 'static,
    {
        MegaEvm::set_keyless_sandbox_observer(self, observer);
    }

    fn set_keyless_sandbox_observer_for_parent_env(
        &mut self,
        observer: Option<Rc<RefCell<dyn SandboxObserver<ExtEnvs>>>>,
    ) {
        MegaEvm::set_keyless_sandbox_observer_for_parent_env(self, observer);
    }

    fn set_keyless_sandbox_inspector<I>(&mut self, inspector: Option<Rc<RefCell<I>>>)
    where
        I: SandboxInspector<ExtEnvs> + SandboxInspector<EmptyExternalEnv> + 'static,
    {
        MegaEvm::set_keyless_sandbox_inspector(self, inspector);
    }

    fn set_keyless_sandbox_inspector_for_parent_env(
        &mut self,
        inspector: Option<Rc<RefCell<dyn SandboxInspector<ExtEnvs>>>>,
    ) {
        MegaEvm::set_keyless_sandbox_inspector_for_parent_env(self, inspector);
    }

    fn clear_keyless_sandbox_hook(&mut self) {
        MegaEvm::clear_keyless_sandbox_hook(self);
    }
}

impl<H, E: KeylessSandboxHooks, R: OpReceiptBuilder> KeylessSandboxHooks
    for MegaBlockExecutor<H, E, R>
{
    type ExtEnvs = E::ExtEnvs;

    fn set_keyless_sandbox_observer<O>(&mut self, observer: Option<Rc<RefCell<O>>>)
    where
        O: SandboxObserver<E::ExtEnvs> + SandboxObserver<EmptyExternalEnv> + 'static,
    {
        self.evm.set_keyless_sandbox_observer(observer);
    }

    fn set_keyless_sandbox_observer_for_parent_env(
        &mut self,
        observer: Option<Rc<RefCell<dyn SandboxObserver<E::ExtEnvs>>>>,
    ) {
        self.evm.set_keyless_sandbox_observer_for_parent_env(observer);
    }

    fn set_keyless_sandbox_inspector<I>(&mut self, inspector: Option<Rc<RefCell<I>>>)
    where
        I: SandboxInspector<E::ExtEnvs> + SandboxInspector<EmptyExternalEnv> + 'static,
    {
        self.evm.set_keyless_sandbox_inspector(inspector);
    }

    fn set_keyless_sandbox_inspector_for_parent_env(
        &mut self,
        inspector: Option<Rc<RefCell<dyn SandboxInspector<E::ExtEnvs>>>>,
    ) {
        self.evm.set_keyless_sandbox_inspector_for_parent_env(inspector);
    }

    fn clear_keyless_sandbox_hook(&mut self) {
        self.evm.clear_keyless_sandbox_hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, MegaSpecId};

    struct NopObserver;

    impl<E: ExternalEnvTypes> SandboxObserver<E> for NopObserver {}

    fn attach_through_trait<T: KeylessSandboxHooks>(host: &mut T) {
        host.set_keyless_sandbox_observer(Some(Rc::new(RefCell::new(NopObserver))));
        host.clear_keyless_sandbox_hook();
    }

    #[test]
    fn test_hooks_trait_reaches_context_and_evm() {
        let mut db = MemoryDatabase::default();
        let mut ctx = MegaContext::new(&mut db, MegaSpecId::REX6);
        attach_through_trait(&mut ctx);

        let mut evm = MegaEvm::new(ctx);
        attach_through_trait(&mut evm);
        evm.set_keyless_sandbox_observer(Some(Rc::new(RefCell::new(NopObserver))));
        assert!(evm.ctx.keyless_sandbox_hook.is_some());
        KeylessSandboxHooks::clear_keyless_sandbox_hook(&mut evm);
        assert!(evm.ctx.keyless_sandbox_hook.is_none());
    }
}
