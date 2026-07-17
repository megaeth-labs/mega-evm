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

A node MUST deploy the bytecode version corresponding to the active spec.

#### Version 1.0.0

Since: [Rex5](../upgrades/rex5.md)

Code hash: `0x63cd411a379be1c198613ef1d15c3058e7b0db4a5d07d4bcf07014af90040315`

Deployed bytecode: `0x608060405234801561000f57...` ([full bytecode](../static/bytecode/SequencerRegistry-1.0.0.txt)).

To verify the code hash, from the repository root:

```bash
cast keccak $(cat docs/spec/static/bytecode/SequencerRegistry-1.0.0.txt)
```

A node MUST deploy the contract via raw state patch with initial storage seeded at deploy time.
A node MUST NOT execute a constructor during deployment.

### Storage Layout

The storage layout is consensus-critical.
Rust slot constants in `mega-system-contracts` MUST match this layout.

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

Each `ChangeRecord` MUST be packed as `uint96 fromBlock` followed by `address addr` in one 32-byte slot.

Future versions of `SequencerRegistry` MUST only append new storage slots after slot 12.
Future versions MUST NOT reorder existing slots or insert new slots before slot 13 once the contract is in use, because dynamic-array element keys are derived from `keccak256(slot)` and any slot change orphans the existing data.

### Interface

```solidity
interface ISequencerRegistry {
    // Packed history entry
    struct ChangeRecord {
        uint96 fromBlock;
        address addr;
    }

    // System address role
    function currentSystemAddress() external view returns (address);
    function systemAddressAt(uint256 blockNumber) external view returns (address);
    function scheduleNextSystemAddressChange(address newSystemAddress, uint256 activationBlock) external;
    event SystemAddressChangeScheduled(
        address indexed oldSystemAddress,
        address indexed newSystemAddress,
        uint256 activationBlock
    );

    // Sequencer role
    function currentSequencer() external view returns (address);
    function sequencerAt(uint256 blockNumber) external view returns (address);
    function scheduleNextSequencerChange(address newSequencer, uint256 activationBlock) external;
    event SequencerChangeScheduled(
        address indexed oldSequencer,
        address indexed newSequencer,
        uint256 activationBlock
    );

    // Shared
    function applyPendingChanges() external;
    function admin() external view returns (address);
    function pendingAdmin() external view returns (address);
    function transferAdmin(address newAdmin) external; // step 1: schedule
    function acceptAdmin() external;                   // step 2: complete
    event AdminTransferStarted(address indexed currentAdmin, address indexed newPendingAdmin);
    event AdminTransferred(address indexed oldAdmin, address indexed newAdmin);

    // Errors
    error FutureBlock();
    error BeforeInitialBlock();
    error NotAdmin();
    error NotPendingAdmin();
    error ZeroAddress();
    error InvalidActivationBlock();
    error ActivationBlockTooLarge();
}
```

### Read Methods

`currentSystemAddress()` MUST return the value in `_currentSystemAddress`.
`currentSequencer()` MUST return the value in `_currentSequencer`.
Both values MUST be seeded at deploy time and MUST be updated only by `applyPendingChanges()`.

`systemAddressAt(blockNumber)` and `sequencerAt(blockNumber)` MUST return the role address active at the given block.
They MUST revert with `FutureBlock` if `blockNumber > block.number` and `BeforeInitialBlock` if `blockNumber < _initialFromBlock`.
Both roles MUST share the same `_initialFromBlock`.

### Change Scheduling

Each role MUST have an independent `schedule*Change(newAddress, activationBlock)` method.
To cancel a pending change, the caller MUST pass `activationBlock = type(uint256).max` and `newAddress = address(0)`; the call MUST be handled as a special cancellation case before address and `uint96` validation.
For all other values, `activationBlock` MUST be strictly greater than `block.number`; otherwise the call MUST revert with `InvalidActivationBlock()`.
`activationBlock` MUST also fit in `uint96`; if it exceeds `type(uint96).max`, the call MUST revert with `ActivationBlockTooLarge()`.
At most one pending change per role MUST exist at a time; a new schedule MUST overwrite the previous one.

### Pre-Block Apply

`applyPendingChanges()` MUST be permissionless and MUST apply both roles atomically.
The execution layer MUST call `applyPendingChanges()` as a pre-block system call when a pre-check confirms any role change is due.
For each role, if a change is pending and due, `applyPendingChanges()` MUST update the current address, append to the change history, and clear pending state.

The system call MUST be issued with `gas_limit = max(block.gas_limit, 30_000_000)` instead of revm's upstream-fixed 30M default, matching the EIP-2935 / EIP-4788 pre-block calls.
This gas floor is necessary because the slot-rotation cost scales with REX dynamic storage gas (SALT bucket capacity), and a fixed 30M is no longer guaranteed to be sufficient on activation blocks.

### Two-Step Admin Transfer

Admin handoff is a two-step process to prevent permanent loss of admin authority through a single mistyped, phished, or clipboard-substituted address.

1. The current admin calls `transferAdmin(newAdmin)`.
   The call MUST set `_pendingAdmin = newAdmin` and emit `AdminTransferStarted(currentAdmin, newAdmin)`.
   The current admin MUST remain in effect; `admin()` and all admin-only operations MUST be unaffected until step 2.
   Passing `address(0)` MUST cancel any previously pending transfer.
   Re-calling `transferAdmin` MUST overwrite `_pendingAdmin`.
2. The pending admin calls `acceptAdmin()`.
   This MUST be the only way `_admin` is updated.
   It MUST set `_admin = msg.sender`, clear `_pendingAdmin`, and emit `AdminTransferred(oldAdmin, newAdmin)`.
   Any caller other than the current `_pendingAdmin` MUST revert with `NotPendingAdmin`.

The acceptance step proves the new admin's keys are live and controlled.
Until acceptance, the old admin MUST retain full authority and MAY re-target or cancel the pending transfer.

### Interception

`SequencerRegistry` does NOT use call interception.
All methods MUST run as normal on-chain bytecode.

### Deploy-Time Seeding

A node MUST write 6 flat storage slots at first deploy:
`_currentSystemAddress`, `_currentSequencer`, `_admin`, `_initialSystemAddress`, `_initialSequencer`, `_initialFromBlock`.
The sequencer and admin addresses MUST come from `SequencerRegistryConfig` on the chain's hardfork configuration.

**The initial system address is fixed.**
Both `_currentSystemAddress` and `_initialSystemAddress` MUST be seeded with the legacy `MEGA_SYSTEM_ADDRESS` constant.
The genesis value MUST NOT be configurable on `SequencerRegistryConfig`.
Until the registry is active, the payload executor, txpool, and replay paths all assume the system-transaction sender equals `MEGA_SYSTEM_ADDRESS`, so the initial value is seeded at the activation boundary rather than being freely configurable, to avoid silently breaking those invariants at chain bootstrap.
Once the registry is active, the system address MAY be rotated via `scheduleNextSystemAddressChange` and `applyPendingChanges`.

**Validation constraints:** Both configurable address fields — `rex5_initial_sequencer` and `rex5_initial_admin` — MUST be non-zero.
The chain configuration MUST reject either zero address before the first block using this registry is executed.
A zero `rex5_initial_admin` would permanently lock all admin-only registry operations.
A zero `rex5_initial_sequencer` produces an invalid initial sequencer state.

The `rex5_` prefix on these field names is deliberate: the values seed the registry at the activation boundary of the spec that introduces it, and blocks before that boundary ignore them entirely.

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
