---
description: SequencerRegistry system contract — dual-role registry for system address and sequencer, with change scheduling and history.
spec: Rex5
---

# SequencerRegistry

> **Unstable** — Rex5 semantics described here are under active development and may change before network activation.

This page specifies the `SequencerRegistry` system contract.
It tracks two independent roles: the **system address** (Oracle/system-tx authority) and the **sequencer** (mini-block signing key).
Each role has its own change lifecycle.

## Motivation

MegaETH needs to decouple the Oracle/system-transaction sender from the mini-block signing key so that each can be changed independently without affecting the other.
The `SequencerRegistry` provides a canonical on-chain source of truth for both roles, enabling:

- On-chain verification of mini-block signatures via `currentSequencer()` and `sequencerAt()`.
- Dynamic Oracle authority via `currentSystemAddress()`, replacing the hardcoded `MEGA_SYSTEM_ADDRESS`.
- Independent change of each role without redeploying contracts.

## Specification

### Address

```text
0x6342000000000000000000000000000000000006
```

### Bytecode

Version 1.0.0

Since: [Rex5](../upgrades/rex5.md)

The contract is deployed via raw state patch with initial storage seeded at deploy time.
No constructor is executed.

### Storage Layout

The storage layout is consensus-critical.
Rust slot constants in `mega-system-contracts` must match this layout.

| Slot | Name                            | Type             |
| ---- | ------------------------------- | ---------------- |
| 0    | `_currentSystemAddress`         | `address`        |
| 1    | `_currentSequencer`             | `address`        |
| 2    | `_admin`                        | `address`        |
| 3    | `_pendingAdmin`                 | `address`        |
| 4    | `_initialSystemAddress`         | `address`        |
| 5    | `_initialSequencer`             | `address`        |
| 6    | `_initialFromBlock`             | `uint256`        |
| 7    | `_pendingSystemAddress`         | `address`        |
| 8    | `_systemAddressActivationBlock` | `uint256`        |
| 9    | `_pendingSequencer`             | `address`        |
| 10   | `_sequencerActivationBlock`     | `uint256`        |
| 11   | `_systemAddressHistory`         | `ChangeRecord[]` |
| 12   | `_sequencerHistory`             | `ChangeRecord[]` |

`ChangeRecord` is packed: `uint96 fromBlock` + `address addr` fit in one 32-byte slot.

Future versions of `SequencerRegistry` may only **append** new slots; reordering or inserting in the middle is forbidden once the contract is in use, because dynamic-array element keys are derived from `keccak256(slot)` and any slot change orphans the existing data.

### Interface

```solidity
interface ISequencerRegistry {
    // System address role
    function currentSystemAddress() external view returns (address);
    function systemAddressAt(uint256 blockNumber) external view returns (address);
    function scheduleNextSystemAddressChange(address newSystemAddress, uint256 activationBlock) external;

    // Sequencer role
    function currentSequencer() external view returns (address);
    function sequencerAt(uint256 blockNumber) external view returns (address);
    function scheduleNextSequencerChange(address newSequencer, uint256 activationBlock) external;

    // Shared
    function applyPendingChanges() external;
    function admin() external view returns (address);
    function pendingAdmin() external view returns (address);
    function transferAdmin(address newAdmin) external; // step 1: schedule
    function acceptAdmin() external;                   // step 2: complete
}
```

### Read Methods

`currentSystemAddress()` returns the value in `_currentSystemAddress`.
`currentSequencer()` returns the value in `_currentSequencer`.
Both are seeded at deploy time and updated only by `applyPendingChanges()`.

`systemAddressAt(blockNumber)` and `sequencerAt(blockNumber)` return the role address active at the given block.
They revert with `FutureBlock` if `blockNumber > block.number` and `BeforeInitialBlock` if `blockNumber < _initialFromBlock`.
Both roles share the same `_initialFromBlock`.

### Change Scheduling

Each role has independent `schedule*Change(newAddress, activationBlock)`.
`activationBlock` must be strictly greater than `block.number` and fit in `uint96`.
At most one pending change per role exists at a time; a new schedule overwrites the previous one.
To cancel, pass `activationBlock = type(uint256).max` and `newAddress = address(0)`.

### Pre-Block Apply

`applyPendingChanges()` is permissionless and applies both roles atomically.
It is called by the execution layer as a pre-block system call when a Rust-side pre-check confirms any role change is due.
For each role, if pending and due, it updates the current address, appends to the change history, and clears pending state.

### Two-Step Admin Transfer

Admin handoff is a two-step process to prevent permanent loss of admin authority through a single mistyped, phished, or clipboard-substituted address.

1. The current admin calls `transferAdmin(newAdmin)`.
   This sets `_pendingAdmin = newAdmin` and emits `AdminTransferStarted(currentAdmin, newAdmin)`.
   The current admin remains in effect — `admin()` and all admin-only operations are unaffected until step 2.
   Passing `address(0)` cancels any previously pending transfer.
   Re-calling `transferAdmin` overwrites `_pendingAdmin`.
2. The pending admin calls `acceptAdmin()`.
   This is the only way `_admin` is ever updated.
   It sets `_admin = msg.sender`, clears `_pendingAdmin`, and emits `AdminTransferred(oldAdmin, newAdmin)`.
   Any caller other than the current `_pendingAdmin` reverts with `NotPendingAdmin`.

The acceptance step proves the new admin's keys are live and controlled.
Until acceptance, the old admin retains full authority and can re-target or cancel the pending transfer.

### Interception

`SequencerRegistry` does NOT use call interception.
All methods run as normal on-chain bytecode.

### Deploy-Time Seeding

At first deploy, the execution layer writes 6 flat storage slots:
`_currentSystemAddress`, `_currentSequencer`, `_admin`, `_initialSystemAddress`, `_initialSequencer`, `_initialFromBlock`.
The sequencer and admin addresses come from `SequencerRegistryConfig` on the chain's hardfork configuration.

**The initial system address is fixed.**
Both `_currentSystemAddress` and `_initialSystemAddress` are seeded with the legacy `MEGA_SYSTEM_ADDRESS` constant.
The genesis value is not configurable on `SequencerRegistryConfig`.
Pre-Rex5 components (payload executor, txpool, replay) all assume the system tx sender equals `MEGA_SYSTEM_ADDRESS`, so a configurable initial value would silently break those invariants at chain bootstrap.
After Rex5 activation, the system address can be rotated via `scheduleNextSystemAddressChange` + `applyPendingChanges`.

**Validation constraints:** Both configurable address fields — `rex5_initial_sequencer` and `rex5_initial_admin` — must be non-zero.
The chain configuration MUST reject either zero address before the first block using this registry is executed.
A zero `rex5_initial_admin` would permanently lock all admin-only registry operations.
A zero `rex5_initial_sequencer` produces an invalid initial sequencer state.

The `rex5_` prefix on these field names is deliberate: the values only take effect when Rex5 activates and seed the registry at that moment; pre-Rex5 blocks ignore them entirely.

## Constants

| Name                         | Value                                        | Description      |
| ---------------------------- | -------------------------------------------- | ---------------- |
| `SEQUENCER_REGISTRY_ADDRESS` | `0x6342000000000000000000000000000000000006` | Contract address |

## Rationale

**Why two roles instead of one?**
The Oracle/system-tx sender and the mini-block signing key are different operational concerns.
Coupling them means changing one silently revokes the other, which would break Oracle authority on the first sequencer key change.

**Why a pre-block system call for role changes?**
Applying a role change as a regular transaction would change role addresses mid-block, breaking block-stability.

**Why deploy-time storage seeding instead of constant bootstrap?**
The `_initialFromBlock` depends on the Rex5 activation block number, which is not known at compile time.
Seeding all initial values at deploy time keeps the bootstrap mechanism uniform.

## Spec History

- [Rex5](../upgrades/rex5.md) introduced the `SequencerRegistry` contract with dual roles.
