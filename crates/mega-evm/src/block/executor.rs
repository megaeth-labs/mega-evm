//! Block executor of the Satin engine.
//!
//! Block execution lands here with the block executor: alloy-evm's `BlockExecutor` over a
//! [`MegaEvm`](crate::MegaEvm), the Karst block rules, the pre-block system calls and
//! system contract deployments, and the block-level limits.

/// Executes the transactions of one block on a Satin EVM.
///
/// A skeleton for now: it owns the EVM and hands it back.
#[derive(Debug)]
pub struct MegaBlockExecutor<E> {
    evm: E,
}

impl<E> MegaBlockExecutor<E> {
    /// Creates an executor that runs the block on `evm`.
    pub const fn new(evm: E) -> Self {
        Self { evm }
    }

    /// The EVM the block runs on.
    pub const fn evm(&self) -> &E {
        &self.evm
    }

    /// The EVM the block runs on, mutably.
    pub const fn evm_mut(&mut self) -> &mut E {
        &mut self.evm
    }

    /// Consumes the executor and returns its EVM.
    pub fn into_evm(self) -> E {
        self.evm
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaSpecId};
    use alloy_evm::Evm;
    use revm::context::BlockEnv;

    #[test]
    fn test_executor_hands_back_its_evm() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN)
            .with_block(BlockEnv { gas_limit: 30_000_000, ..Default::default() });
        let mut executor = MegaBlockExecutor::new(MegaEvm::new(ctx));

        assert_eq!(executor.evm().block().gas_limit, 30_000_000);
        executor.evm_mut().set_inspector_enabled(true);
        assert!(executor.into_evm().is_inspecting());
    }
}
