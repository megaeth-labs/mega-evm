//! The `SequencerRegistry` system contract.
//!
//! It holds the two rotating roles of the chain — the system address and the sequencer — and
//! their change schedules. It runs its own bytecode: no method of it is intercepted, so a call
//! to it is an ordinary call. Its Solidity source is
//! `crates/system-contracts/contracts/SequencerRegistry.sol`.
//!
//! The contract is deployed by [`transact_deploy`](crate::system::transact_deploy) with the
//! bootstrap slots [`SequencerRegistryConfig`] names. Reading the rotated system address out of
//! its storage, and the pre-block call that applies a due change, belong to the pre-block
//! system calls.

use alloy_primitives::{address, Address};

use super::MEGA_SYSTEM_ADDRESS;
use crate::{HardforkParams, HardforkParamsError, MegaHardfork};

/// The address of the `SequencerRegistry` system contract.
pub const SEQUENCER_REGISTRY_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000006");

/// The code of the `SequencerRegistry` contract.
pub use mega_system_contracts::sequencer_registry::LATEST_CODE as SEQUENCER_REGISTRY_CODE;

/// The code hash of the `SequencerRegistry` contract.
pub use mega_system_contracts::sequencer_registry::LATEST_CODE_HASH as SEQUENCER_REGISTRY_CODE_HASH;

pub use mega_system_contracts::sequencer_registry::{storage_slots, ISequencerRegistry};

/// Delay the unknown-chain placeholder seeds, in blocks.
///
/// A zero delay would disable the reaction window the field exists to guarantee. Ten matches
/// the contract's own tests: long enough that a rotation cannot activate in the same block.
pub const PLACEHOLDER_MIN_ROTATION_DELAY: u64 = 10;

/// Bootstrap configuration for the `SequencerRegistry`, attached to Satin via [`HardforkParams`].
///
/// These values seed the registry's storage on a fresh deploy. After that the live roles are
/// whatever the contract holds; rotating them is a later pre-block system call. The contract
/// has no setter for `_minRotationDelay`, so a matching-code registry cannot be repaired later
/// through this helper: the delay must be seeded here, and it must not be zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerRegistryConfig {
    /// Seeded into `_currentSystemAddress` and `_initialSystemAddress`.
    pub initial_system_address: Address,
    /// Seeded into `_currentSequencer` and `_initialSequencer`.
    pub initial_sequencer: Address,
    /// Seeded into `_admin`.
    pub initial_admin: Address,
    /// Seeded into `_initialFromBlock`: the first block at which historical lookups are valid.
    pub initial_from_block: u64,
    /// Seeded into `_minRotationDelay`. Must be nonzero: a zero delay disables the reaction
    /// window the field exists to guarantee.
    pub min_rotation_delay: u64,
}

impl SequencerRegistryConfig {
    /// Placeholder roles for a chain that has none published: [`MEGA_SYSTEM_ADDRESS`] for every
    /// address, block zero, and [`PLACEHOLDER_MIN_ROTATION_DELAY`].
    ///
    /// The unknown-chain fallback schedule uses this so a local chain can start. A real network
    /// attaches the roles governance chose.
    pub const fn placeholder() -> Self {
        Self {
            initial_system_address: MEGA_SYSTEM_ADDRESS,
            initial_sequencer: MEGA_SYSTEM_ADDRESS,
            initial_admin: MEGA_SYSTEM_ADDRESS,
            initial_from_block: 0,
            min_rotation_delay: PLACEHOLDER_MIN_ROTATION_DELAY,
        }
    }
}

impl HardforkParams for SequencerRegistryConfig {
    const FORK: MegaHardfork = MegaHardfork::Satin;
    const NAME: &'static str = "SequencerRegistryConfig";

    fn validate(&self) -> Result<(), HardforkParamsError> {
        if self.initial_system_address.is_zero() {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.initial_system_address must not be zero".into(),
            });
        }
        if self.initial_sequencer.is_zero() {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.initial_sequencer must not be zero".into(),
            });
        }
        if self.initial_admin.is_zero() {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.initial_admin must not be zero".into(),
            });
        }
        if self.min_rotation_delay == 0 {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.min_rotation_delay must not be zero".into(),
            });
        }
        Ok(())
    }
}
