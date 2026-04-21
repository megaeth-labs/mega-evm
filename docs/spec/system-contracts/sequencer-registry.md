---
description: SequencerRegistry system contract — sequencer address lookup, rotation scheduling, and history.
spec: Rex5
---

# SequencerRegistry

This page specifies the `SequencerRegistry` system contract.
It records the current sequencer, pending rotations, and rotation history.
On-chain contracts use it to verify mini-block signatures by looking up the sequencer active at a given block.

## Motivation

MegaETH's mini-block signing feature requires on-chain verification of sequencer signatures.
The `SequencerRegistry` provides a canonical source of truth for which address is the active sequencer at any block number.
It also enables sequencer rotation without redeploying the Oracle contract.

## Specification

### Address

```text
0x6342000000000000000000000000000000000006
```

### Bytecode

Version 1.0.0

Since: [Rex5](../upgrades/rex5.md)

Code hash: `0x1ab52fdc824e7014abade0233ffb0017234c8ffd4075a51ca600f5a8fc5e7a47`

The contract uses a constant-bootstrap pattern.
`INITIAL_SEQUENCER` and `INITIAL_ADMIN` are compile-time constants in the bytecode.
No storage is written during deployment.
`address(0)` in storage means "use the constant default".

### Interface

```solidity
interface ISequencerRegistry {
    function currentSequencer() external view returns (address);
    function sequencerAt(uint256 blockNumber) external view returns (address);
    function scheduleNextSequencerChange(address newSequencer, uint256 activationBlock) external;
    function applyPendingChange() external;
    function admin() external view returns (address);
    function transferAdmin(address newAdmin) external;
}
```

Errors:

```solidity
error FutureBlock();
error NotAdmin();
error ZeroAddress();
error InvalidActivationBlock();
error ActivationBlockTooLarge();
```

Events:

```solidity
event SequencerChangeScheduled(address indexed oldSequencer, address indexed newSequencer, uint256 activationBlock);
event AdminTransferred(address indexed oldAdmin, address indexed newAdmin);
```

### Read Methods

`currentSequencer()` returns `_currentSequencer` if non-zero, otherwise `INITIAL_SEQUENCER`.

`admin()` returns `_admin` if non-zero, otherwise `INITIAL_ADMIN`.

`sequencerAt(blockNumber)` reverts with `FutureBlock` if `blockNumber > block.number`.
Otherwise it searches the rotation history in reverse.
If no rotation covers the queried block, it returns `INITIAL_SEQUENCER`.

### Rotation Scheduling

`scheduleNextSequencerChange(newSequencer, activationBlock)` is restricted to the current admin.
`activationBlock` must be strictly greater than `block.number`.
At most one pending schedule exists at a time; a new schedule overwrites the previous one.
To cancel, pass `activationBlock = type(uint256).max` and `newSequencer = address(0)`.

### Pre-Block Apply

`applyPendingChange()` is permissionless.
It is called by the execution layer as a pre-block system call (same pattern as EIP-2935 and EIP-4788).
It is only invoked when a Rust-side pre-check confirms a rotation is due.

When pending and due:

1. Writes `_pendingSequencer` to `_currentSequencer`.
2. Appends a `RotationRecord` to the history array.
3. Clears `_pendingSequencer` and `_activationBlock`.

When not pending or not due: no-op.

### Interception

`SequencerRegistry` does NOT use call interception.
All methods run as normal on-chain bytecode.

### Value Transfer

All methods are non-payable.

## Constants

| Name                         | Value                                        | Description                               |
| ---------------------------- | -------------------------------------------- | ----------------------------------------- |
| `SEQUENCER_REGISTRY_ADDRESS` | `0x6342000000000000000000000000000000000006` | Contract address                          |
| `REX5_INITIAL_SEQUENCER`     | `0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d` | Initial sequencer (compile-time constant) |
| `REX5_REGISTRY_ADMIN`        | `0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d` | Initial admin (compile-time constant)     |

## Rationale

**Why constant bootstrap instead of constructor parameters?**
mega-evm deploys system contracts by replacing deployed bytecode without running constructors.
Compile-time constants avoid the need for deploy-time storage initialization, keeping the deployment pattern consistent with all other system contracts.

**Why a pre-block system call for rotation?**
Applying rotation as a regular transaction would change `_currentSequencer` mid-block, breaking block-stability of `SYSTEM_ADDRESS`.
A pre-block system call ensures the rotation is applied before any user transaction executes.

**Why is the system call gated by a Rust pre-check?**
Reading two storage slots (pending sequencer + activation block) is cheaper than an EVM call.
On blocks without a pending rotation (the common case), the EVM call is skipped entirely.

## Spec History

- [Rex5](../upgrades/rex5.md) introduced the `SequencerRegistry` contract.
