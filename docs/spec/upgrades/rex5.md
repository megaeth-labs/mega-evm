---
description: Rex5 network upgrade — SequencerRegistry, dynamic system address, and Oracle v2.0.0.
---

# Rex5 Network Upgrade

> **Unstable** — This spec is under active development.
> Its semantics may change before network activation.
> Activation timestamps have not been set.

This page is an informative summary of the Rex5 specification.
For the full normative definition, see the Rex5 spec in the mega-evm repository.

## Summary

Rex5 introduces the `SequencerRegistry` system contract and dynamic system address resolution.
It also upgrades the Oracle contract to v2.0.0 to read its authority from the registry.

## What Changed

### 1. SequencerRegistry System Contract

A new system contract at `0x6342000000000000000000000000000000000006`.
It records the current sequencer, pending rotations, and rotation history.

Key methods:
- `currentSequencer()` — returns the current sequencer (constant fallback on fresh deploy).
- `sequencerAt(blockNumber)` — returns the sequencer active at a given block.
- `scheduleNextSequencerChange(newSequencer, activationBlock)` — admin schedules a rotation.
- `applyPendingChange()` — permissionless; called by the execution layer as a pre-block system call.
- `admin()` / `transferAdmin(newAdmin)` — admin management.

The contract uses a constant-bootstrap pattern: initial sequencer and admin are compile-time constants in the bytecode.
No storage is written during deployment.

### 2. Dynamic System Address

The system address used for system transaction identification and Oracle gas detention exemption is no longer a hardcoded constant.
It is resolved per block from `SequencerRegistry._currentSequencer` after all pre-block changes are committed.

On fresh deploy (zero storage), the resolved address equals the bytecode constant `INITIAL_SEQUENCER`.
After a sequencer rotation, the resolved address reflects the new sequencer.

### 3. Oracle v2.0.0

The Oracle contract's `onlySystemAddress` modifier now reads the current sequencer from `SequencerRegistry.currentSequencer()` instead of using a constructor `immutable`.
This enables sequencer rotation without redeploying the Oracle.

All other Oracle functionality (`sendHint`, `multiCall`, `getSlot`, `setSlot`, etc.) is preserved from v1.1.0.

### 4. Pre-Block Sequencer Rotation

Pending sequencer rotations are applied during `pre_execution_changes` via a pre-block EVM system call to `SequencerRegistry.applyPendingChange()`.
This follows the same pattern as EIP-2935 and EIP-4788.
The system call is only issued when a pending rotation is due (Rust pre-check reads committed storage).

## Developer Impact

- Contracts that verify mini-block signatures can use `SequencerRegistry.currentSequencer()` to look up the signing authority.
- Contracts that need historical sequencer information can use `sequencerAt(blockNumber)`.
- The Oracle contract's write methods (`setSlot`, `emitLog`, etc.) now accept calls from the current sequencer as reported by `SequencerRegistry`, not from a fixed address.

## Safety and Compatibility

- Pre-REX5 behavior is unchanged. The legacy `MEGA_SYSTEM_ADDRESS` constant is used for all pre-REX5 specs.
- `SequencerRegistry` does not have an interceptor. It runs normal on-chain bytecode.
- The `_currentSequencer` storage slot is only updated during pre-block system calls, ensuring block-stability.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model
- `crates/mega-evm/src/system/sequencer_registry.rs` — Rust implementation
- `crates/system-contracts/contracts/SequencerRegistry.sol` — Solidity contract
- `crates/system-contracts/contracts/Oracle.sol` — Oracle v2.0.0
