---
description: SequencerRegistry system contract — dual-role registry for system address and sequencer, with change scheduling and history.
spec: Rex5
---

# SequencerRegistry

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
| 3    | `_initialSystemAddress`         | `address`        |
| 4    | `_initialSequencer`             | `address`        |
| 5    | `_initialFromBlock`             | `uint256`        |
| 6    | `_pendingSystemAddress`         | `address`        |
| 7    | `_systemAddressActivationBlock` | `uint256`        |
| 8    | `_pendingSequencer`             | `address`        |
| 9    | `_sequencerActivationBlock`     | `uint256`        |
| 10   | `_systemAddressHistory`         | `ChangeRecord[]` |
| 11   | `_sequencerHistory`             | `ChangeRecord[]` |

`ChangeRecord` is packed: `uint96 fromBlock` + `address addr` fit in one 32-byte slot.

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
    function transferAdmin(address newAdmin) external;
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

### Interception

`SequencerRegistry` does NOT use call interception.
All methods run as normal on-chain bytecode.

### Deploy-Time Seeding

At first deploy, the execution layer writes 6 flat storage slots:
`_currentSystemAddress`, `_currentSequencer`, `_admin`, `_initialSystemAddress`, `_initialSequencer`, `_initialFromBlock`.
The values come from `SequencerRegistryConfig` on the chain's hardfork configuration.

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
