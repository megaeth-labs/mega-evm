//! Flat storage slot constants for `SequencerRegistry`.

use alloy_primitives::U256;

/// Storage slot for `_currentSystemAddress`.
pub const CURRENT_SYSTEM_ADDRESS: U256 = U256::ZERO;

/// Storage slot for `_currentSequencer`.
pub const CURRENT_SEQUENCER: U256 = U256::from_limbs([1, 0, 0, 0]);

/// Storage slot for `_admin`.
pub const ADMIN: U256 = U256::from_limbs([2, 0, 0, 0]);

/// Storage slot for `_pendingAdmin`.
pub const PENDING_ADMIN: U256 = U256::from_limbs([3, 0, 0, 0]);

/// Storage slot for `_initialSystemAddress`.
pub const INITIAL_SYSTEM_ADDRESS: U256 = U256::from_limbs([4, 0, 0, 0]);

/// Storage slot for `_initialSequencer`.
pub const INITIAL_SEQUENCER: U256 = U256::from_limbs([5, 0, 0, 0]);

/// Storage slot for `_initialFromBlock`.
pub const INITIAL_FROM_BLOCK: U256 = U256::from_limbs([6, 0, 0, 0]);

/// Storage slot for `_pendingSystemAddress`.
pub const PENDING_SYSTEM_ADDRESS: U256 = U256::from_limbs([7, 0, 0, 0]);

/// Storage slot for `_systemAddressActivationBlock`.
pub const SYSTEM_ADDRESS_ACTIVATION_BLOCK: U256 = U256::from_limbs([8, 0, 0, 0]);

/// Storage slot for `_pendingSequencer`.
pub const PENDING_SEQUENCER: U256 = U256::from_limbs([9, 0, 0, 0]);

/// Storage slot for `_sequencerActivationBlock`.
pub const SEQUENCER_ACTIVATION_BLOCK: U256 = U256::from_limbs([10, 0, 0, 0]);

// Slots 11 (`_systemAddressHistory`) and 12 (`_sequencerHistory`) are Solidity
// dynamic arrays. By the standard layout, the slot itself stores `array.length` and
// element `i` lives at `keccak256(bytes32(slot)) + i`. Concretely:
//
//   `_systemAddressHistory[0]` is at
//     `keccak256(bytes32(uint256(11)))`
//     = `0x0175b7a638427703f0dbe7bb9bbf987a2551717b34e79f33b5b1008d1fa01db9`
//   `_sequencerHistory[0]` is at
//     `keccak256(bytes32(uint256(12)))`
//     = `0xdf6966c971051c3d54ec59162606531493a51404a002842f56009d7e5cf4a8c7`
//
// Rust never reads or writes these arrays directly — `systemAddressAt` and
// `sequencerAt` are on-chain getters that iterate them — so there is no flat slot
// constant.

/// Storage slot for `_minRotationDelay` (v2.0.0+). Appended after the two dynamic
/// arrays at slots 11/12, so 13 is the first free flat slot.
pub const MIN_ROTATION_DELAY: U256 = U256::from_limbs([13, 0, 0, 0]);
