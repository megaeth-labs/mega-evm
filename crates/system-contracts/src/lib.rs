//! System contracts for the `MegaETH` EVM.
//!
//! This crate provides bytecode constants for system contracts used by mega-evm.
//! When building from the repository with Foundry installed, the build script validates that
//! the pre-generated constants match the compiled Solidity source.
//! When building from a published crate (without Foundry), the pre-generated constants are used
//! directly.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
#![warn(unused_crate_dependencies)]
#![deny(unused_must_use)]

pub use alloy_primitives;

/// Oracle contract bytecode and interface.
///
/// Bytecode constants are pre-generated from the versioned artifacts in the `artifacts/`
/// directory.
/// Interface bindings are generated from the Solidity source.
pub mod oracle {
    include!("generated/oracle_artifacts.rs");

    alloy_sol_types::sol!("contracts/interfaces/IOracle.sol");
}

/// `KeylessDeploy` contract bytecode and interface.
///
/// Bytecode constants are pre-generated from the versioned artifacts in the `artifacts/`
/// directory.
/// Interface bindings are generated from the Solidity source.
pub mod keyless_deploy {
    include!("generated/keyless_deploy_artifacts.rs");

    alloy_sol_types::sol!("contracts/interfaces/IKeylessDeploy.sol");
}

/// `MegaAccessControl` contract bytecode and interface.
///
/// Bytecode constants are pre-generated from the versioned artifacts in the `artifacts/`
/// directory.
/// Interface bindings are generated from the Solidity source.
#[allow(missing_docs)]
pub mod access_control {
    include!("generated/access_control_artifacts.rs");

    alloy_sol_types::sol!(
        #[sol(all_derives)]
        "contracts/interfaces/IMegaAccessControl.sol"
    );
}

/// `MegaLimitControl` contract bytecode and interface.
///
/// Bytecode constants are pre-generated from the versioned artifacts in the `artifacts/`
/// directory.
/// Interface bindings are generated from the Solidity source.
pub mod limit_control {
    include!("generated/limit_control_artifacts.rs");

    alloy_sol_types::sol!("contracts/interfaces/IMegaLimitControl.sol");
}
