---
description: Rex5 network upgrade — SequencerRegistry with dual roles, dynamic system address, and Oracle v2.0.0.
---

# Rex5 Network Upgrade

> **Unstable** — This spec is under active development.
> Its semantics may change before network activation.
> Activation timestamps have not been set.

This page is an informative summary of the Rex5 specification.
For the full normative definition, see the Rex5 spec in the mega-evm repository.

## Summary

Rex5 introduces the `SequencerRegistry` system contract, which tracks two independent roles: the **system address** (Oracle/system-tx authority) and the **sequencer** (mini-block signing key).
It also upgrades the Oracle contract to v2.0.0 to read its authority from the registry.

## What Changed

### 1. SequencerRegistry System Contract

A new system contract at `0x6342000000000000000000000000000000000006`.
It tracks two independent roles, each with its own rotation lifecycle.

Key methods:

- `currentSystemAddress()` — returns the current system address (Oracle/system-tx sender).
- `currentSequencer()` — returns the current sequencer (mini-block signing key).
- `systemAddressAt(blockNumber)` / `sequencerAt(blockNumber)` — historical role lookups.
- `scheduleNextSystemAddressChange(...)` / `scheduleNextSequencerChange(...)` — admin schedules a rotation for either role.
- `applyPendingChanges()` — permissionless; applies both roles atomically as a pre-block system call.
- `admin()` / `transferAdmin(newAdmin)` — admin management.

Initial storage is seeded at deploy time from the chain's `SequencerRegistryConfig`.
No constructor is executed.

### 2. Dynamic System Address

The system address used for system transaction identification and Oracle gas detention exemption is no longer a hardcoded constant.
It is resolved per block from `SequencerRegistry._currentSystemAddress` after all pre-block changes are committed.

Rotating the sequencer does NOT affect the system address, and vice versa.

### 3. Oracle v2.0.0

The Oracle contract's `onlySystemAddress` modifier now reads from `SequencerRegistry.currentSystemAddress()` instead of using a constructor `immutable`.
This enables system address rotation without redeploying the Oracle.

All other Oracle functionality (`sendHint`, `multiCall`, `getSlot`, `setSlot`, etc.) is preserved from v1.1.0.

### 4. Pre-Block Role Rotation

Pending role rotations are applied during `pre_execution_changes` via a single pre-block EVM system call to `SequencerRegistry.applyPendingChanges()`.
This follows the same pattern as EIP-2935 and EIP-4788.
The system call is only issued when a Rust-side pre-check confirms any role rotation is due.

## Developer Impact

- Contracts that verify mini-block signatures can use `SequencerRegistry.currentSequencer()` to look up the signing authority.
- Contracts that need historical information can use `systemAddressAt(blockNumber)` or `sequencerAt(blockNumber)`.
- The Oracle contract's write methods (`setSlot`, `emitLog`, etc.) now accept calls from the current system address as reported by `SequencerRegistry`, not from a fixed address.

## Safety and Compatibility

- Pre-REX5 behavior is unchanged. The legacy `MEGA_SYSTEM_ADDRESS` constant is used for all pre-REX5 specs.
- `SequencerRegistry` does not have an interceptor. It runs normal on-chain bytecode.
- Both `_currentSystemAddress` and `_currentSequencer` are only updated during pre-block system calls, ensuring block-stability.
- Rotating one role does not affect the other.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model
- `crates/mega-evm/src/system/sequencer_registry.rs` — Rust implementation
- `crates/system-contracts/contracts/SequencerRegistry.sol` — Solidity contract
- `crates/system-contracts/contracts/Oracle.sol` — Oracle v2.0.0
