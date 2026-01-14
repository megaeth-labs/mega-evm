//! System contracts for the `MegaETH` EVM.
//!
//! This crate provides bytecode constants for system contracts used by mega-evm.
//! The bytecode is validated at compile time against the Solidity source.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
#![warn(unused_crate_dependencies)]
#![deny(unused_must_use)]

pub use alloy_primitives;

/// Oracle contract bytecode and interface.
///
/// Bytecode constants are generated at build time from the versioned artifacts
/// in the `artifacts/` directory. Interface bindings are generated from the
/// Solidity source.
pub mod oracle {
    include!(concat!(env!("OUT_DIR"), "/oracle_artifacts.rs"));

    alloy_sol_types::sol!("contracts/interfaces/IOracle.sol");
}

/// KeylessDeploy contract bytecode and interface.
///
/// Bytecode constants are generated at build time from the versioned artifacts
/// in the `artifacts/` directory. Interface bindings are generated from the
/// Solidity source.
pub mod keyless_deploy {
    include!(concat!(env!("OUT_DIR"), "/keyless_deploy_artifacts.rs"));

    alloy_sol_types::sol!("contracts/interfaces/IKeylessDeploy.sol");
}
