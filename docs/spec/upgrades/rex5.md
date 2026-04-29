---
description: Rex5 network upgrade — SequencerRegistry with dual roles, dynamic system address, Oracle v2.0.0, caller-account update deduplication, and CALLCODE new-account storage gas metering fix.
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

Rex5 also corrects a resource-accounting bug where the caller-account update was overcounted whenever a contract performed multiple value-transferring sub-calls or contract creations from the same call frame.

Rex5 additionally corrects a `CALLCODE` storage-gas metering bug: new-account storage gas is now charged against the caller's storage context rather than the code-source address.

## What Changed

### 1. SequencerRegistry System Contract

A new system contract at `0x6342000000000000000000000000000000000006`.
It tracks two independent roles, each with its own change lifecycle.

Key methods:

- `currentSystemAddress()` — returns the current system address (Oracle/system-tx sender).
- `currentSequencer()` — returns the current sequencer (mini-block signing key).
- `systemAddressAt(blockNumber)` / `sequencerAt(blockNumber)` — historical role lookups.
- `scheduleNextSystemAddressChange(...)` / `scheduleNextSequencerChange(...)` — admin schedules a change for either role.
- `applyPendingChanges()` — permissionless; applies both roles atomically as a pre-block system call.
- `admin()` / `transferAdmin(newAdmin)` — admin management.

Initial storage is seeded at deploy time from the chain's `SequencerRegistryConfig`.
No constructor is executed.

### 2. Dynamic System Address

The system address used for system transaction identification and Oracle gas detention exemption is no longer a hardcoded constant.
It is resolved per block from `SequencerRegistry._currentSystemAddress` after all pre-block changes are committed.

Changing the sequencer does NOT affect the system address, and vice versa.

### 3. Oracle v2.0.0

The Oracle contract's `onlySystemAddress` modifier now reads from `SequencerRegistry.currentSystemAddress()` instead of using a constructor `immutable`.
This enables system address change without redeploying the Oracle.

All other Oracle functionality (`sendHint`, `multiCall`, `getSlot`, `setSlot`, etc.) is preserved from v1.1.0.

From Rex5, in-place Oracle bytecode upgrades no longer mark the Oracle account as newly created, so any Oracle storage accumulated before the upgrade is preserved across the transition.
This differs from pre-Rex5 upgrades, which cleared existing Oracle storage.

### 4. Pre-Block Role Change

Pending role changes are applied during `pre_execution_changes` via a single pre-block EVM system call to `SequencerRegistry.applyPendingChanges()`.
This follows the same pattern as EIP-2935 and EIP-4788.
The system call is only issued when a Rust-side pre-check confirms any role change is due.

### 5. CALLCODE New-Account Storage Gas Metering

**Previous behavior (Rex4 and earlier):**
The storage-gas wrapper for `CALLCODE` charged new-account storage gas against the stack `to` address — i.e. the code-source address.
For `CALLCODE`, however, execution happens in the caller's account context: the stack `to` only selects which code to load, while the storage / account context remains the caller's.
Charging new-account storage gas against the code-source address can therefore charge spuriously when the code-source is empty.

**New behavior (Rex5):**
The new-account emptiness check and the `new_account_storage_gas(...)` charge are performed against the current frame's storage account (`interpreter.input.target_address()` — the caller / executing contract).
The stack `to` continues to be used solely as the code-source for the underlying `CALLCODE` instruction.
Pre-Rex5 specs preserve the (frozen) prior behavior for backward compatibility.
`CALL` semantics are unchanged: the stack `to` is still the value recipient and is the correct address for new-account metering.

### 6. Caller-Account Update Deduplication (Data Size and KV Updates)

**Previous behavior (Rex4 and earlier):**
When a call frame performed a value-transferring `CALL` / `CALLCODE` or a `CREATE` / `CREATE2`, the implementation charged the _caller_ account update to the child frame's discardable budget.
However, the parent frame's `target_updated` flag was never marked after the first charge.
As a result, every subsequent value-transferring sub-call or create from the same parent frame re-charged the caller account, overcounting both data-size bytes and KV-update counts for the caller.

**New behavior (Rex5):**
After charging the first caller-account update within a parent call frame, the frame's `target_updated` flag is marked.
All subsequent value-transferring sub-calls and creates from the same parent frame no longer re-charge the caller account.
Each distinct callee or created account is still counted independently.
The discardable-on-revert mechanic is unchanged: charges recorded inside a child frame that reverts are still dropped.

## Developer Impact

- Contracts that verify mini-block signatures can use `SequencerRegistry.currentSequencer()` to look up the signing authority.
- Contracts that need historical information can use `systemAddressAt(blockNumber)` or `sequencerAt(blockNumber)`.
- The Oracle contract's write methods (`setSlot`, `emitLog`, etc.) now accept calls from the current system address as reported by `SequencerRegistry`, not from a fixed address.
- Transactions that perform multiple value-transferring sub-calls or creates from the same contract now report lower data-size and KV-update usage than they did under Rex4.
  This only affects usage tracking; it does not change execution semantics, state transitions, or the base transaction gas model.
- Value-transferring `CALLCODE` no longer charges new-account storage gas based on the code-source address.
  Contracts that previously paid spurious new-account storage gas via `CALLCODE` to an empty address see lower gas usage under Rex5.

## Safety and Compatibility

- Pre-REX5 behavior is unchanged. The legacy `MEGA_SYSTEM_ADDRESS` constant is used for all pre-REX5 specs.
- `SequencerRegistry` does not have an interceptor. It runs normal on-chain bytecode.
- Both `_currentSystemAddress` and `_currentSequencer` are only updated during pre-block system calls, ensuring block-stability.
- Changing one role does not affect the other.
- Rex4 and earlier retain the original caller-account overcounting behavior unchanged.
- Rex4 and earlier retain the original `CALLCODE` new-account storage gas metering behavior unchanged.
- Rex5 is the current unstable spec under active development; its semantics may still change before network activation.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model
- `crates/mega-evm/src/system/sequencer_registry.rs` — Rust implementation
- `crates/system-contracts/contracts/SequencerRegistry.sol` — Solidity contract
- `crates/system-contracts/contracts/Oracle.sol` — Oracle v2.0.0
