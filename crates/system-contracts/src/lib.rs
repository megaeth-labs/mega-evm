//! System contracts for the `MegaETH` EVM.
//!
//! This crate provides bytecode constants for system contracts used by mega-evm.
//! Repository builds check the constants against the versioned JSON artifacts.
//! Published crates embed the same constants and omit the JSON artifacts.
//! The required CI lint job checks the contract inventory, attests the latest artifacts against
//! the Solidity source with the pinned Osaka EVM target, and checks that a changed Solidity
//! version fails attestation.
//! The literal test table pins the versioned code hashes and latest aliases.
//! The generated headers keep the historical `updated automatically by build.rs` wording to
//! preserve the frozen file bytes.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
// `not(test)`: dev-dependencies (e.g. `serde_json`, used only by integration tests) are linked
// into test builds but not referenced by the library crate, which would otherwise trip this lint.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
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

/// `SequencerRegistry` contract bytecode and interface.
///
/// Bytecode constants are pre-generated from the versioned artifacts in the `artifacts/`
/// directory.
/// Interface bindings are generated from the Solidity source.
#[allow(missing_docs)]
pub mod sequencer_registry {
    include!("generated/sequencer_registry_artifacts.rs");

    alloy_sol_types::sol!(
        #[sol(all_derives)]
        "contracts/interfaces/ISequencerRegistry.sol"
    );

    pub mod storage_slots;
}
