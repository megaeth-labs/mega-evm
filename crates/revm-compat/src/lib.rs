//! Revm compatibility layer for mega-evm
//!
//! This crate provides a compatibility layer that re-exports revm and related
//! APIs while preserving their original namespace structure. This allows existing
//! code written for revm to work with mega-evm without modifications.
//!
//! The re-exports are organized by their originating crate:
//! - [`revm`] - Core revm library types and traits
//! - [`op_revm`] - Optimism-specific revm extensions
//! - [`alloy_evm`] - Alloy EVM utilities
//! - [`alloy_op_evm`] - Alloy Optimism EVM extensions

/// Core revm library re-exports
///
/// This module contains the fundamental types and traits from the revm library,
/// organized by functional area:
pub mod revm {
    // reexport dependencies
    #[doc(inline)]
    pub use revm::bytecode;
    #[doc(inline)]
    pub use revm::context;
    #[doc(inline)]
    pub use revm::context_interface;
    #[doc(inline)]
    pub use revm::database;
    #[doc(inline)]
    pub use revm::database_interface;
    #[doc(inline)]
    pub use revm::handler;
    #[doc(inline)]
    pub use revm::inspector;
    #[doc(inline)]
    pub use revm::interpreter;
    #[doc(inline)]
    pub use revm::precompile;
    #[doc(inline)]
    pub use revm::primitives;
    #[doc(inline)]
    pub use revm::state;

    // Export items.

    pub use revm::{
        context::{
            journal::{Journal, JournalEntry},
            Context,
        },
        database_interface::{Database, DatabaseCommit, DatabaseRef},
        handler::{
            ExecuteCommitEvm, ExecuteEvm, MainBuilder, MainContext, MainnetEvm,
            SystemCallCommitEvm, SystemCallEvm,
        },
        inspector::{InspectCommitEvm, InspectEvm, Inspector},
        // precompile::install_crypto,
    };
}

/// Optimism-specific revm extensions
///
/// This module re-exports Optimism-specific types and configurations
/// from the op-revm crate.
pub mod op_revm {
    pub use op_revm::{
        DefaultOp,
        OpContext,
        OpEvm,
        OpHaltReason,
        OpSpecId, // Optimism specification identifier
        OpTransaction,
        OpTransactionError,
    };

    pub mod precompiles {
        pub use op_revm::precompiles::{fjord, granite, isthmus};
    }
}

/// Alloy EVM utilities
///
/// This module contains EVM-related utilities from the alloy-evm crate,
/// particularly for block execution and transaction handling.
pub mod alloy_evm {
    /// Core EVM factory and environment types
    pub use alloy_evm::{
        Database,
        EvmEnv,            // EVM environment configuration
        EvmFactory,        // Factory for creating EVM instances
        FromRecoveredTx,   // Trait for creating from recovered transactions
        FromTxWithEncoded, // Trait for creating from encoded transactions
    };

    /// Block execution functionality
    pub mod block {
        pub use alloy_evm::block::{
            BlockExecutionError,  // Block execution error type
            BlockExecutionResult, // Result of block execution
            BlockExecutor,        // Block executor trait
            BlockExecutorFactory, // Factory for creating block executors
        };
    }

    pub mod precompiles {
        pub use alloy_evm::precompiles::PrecompilesMap;
    }
}

/// Alloy Optimism EVM extensions
///
/// This module provides Optimism-specific extensions from the alloy-op-evm crate,
/// particularly for receipt building.
pub mod alloy_op_evm {
    pub use alloy_op_evm::{
        OpBlockExecutionCtx, // Optimism block execution context
        OpBlockExecutorFactory,
        OpEvm,
        OpEvmFactory,
    };

    pub mod block {
        pub use alloy_op_evm::block::{
            OpAlloyReceiptBuilder,  // Optimism receipt builder
            OpBlockExecutorFactory, // Optimism block executor factory
        };
    }
}
