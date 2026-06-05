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

Deployed bytecode:

```
0x608060405234801561000f575f5ffd5b50600436106100cf575f3560e01c806354fd4d501161007d578063e3093f5011610058578063e3093f50146101d5578063e8129030146101e8578063f851a44014610206575f5ffd5b806354fd4d501461016657806375829def146101a5578063de7a36af146101b8575f5ffd5b80632b20d6e5116100ad5780632b20d6e5146101385780633411f3c31461014b57806344d744591461015e575f5ffd5b80630e18b681146100d3578063129c8240146100dd578063267822471461011a575b5f5ffd5b6100db610224565b005b6100f06100eb366004610d94565b6102f7565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b60035473ffffffffffffffffffffffffffffffffffffffff166100f0565b6100db610146366004610dd3565b61041d565b6100db610159366004610dd3565b6106cf565b6100db610973565b604080518082018252600581527f312e302e30000000000000000000000000000000000000000000000000000000602082015290516101119190610dfb565b6100db6101b3366004610e4e565b610985565b5f5473ffffffffffffffffffffffffffffffffffffffff166100f0565b6100f06101e3366004610d94565b610a35565b60015473ffffffffffffffffffffffffffffffffffffffff166100f0565b60025473ffffffffffffffffffffffffffffffffffffffff166100f0565b60035473ffffffffffffffffffffffffffffffffffffffff16338114610276576040517f058d9a1b00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6002805473ffffffffffffffffffffffffffffffffffffffff8381167fffffffffffffffffffffffff00000000000000000000000000000000000000008084168217909455600380549094169093556040519116919082907ff8ccb027dfcd135e000e9d45e6cc2d662578a8825d4c45b5e32e0adf67e79ec6905f90a35050565b5f43821115610332576040517fdbe289ad00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b60065482101561036e576040517fd2c461b000000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600c54805b80156103fc575f600c610387600184610e9b565b8154811061039757610397610eb4565b5f91825260209091200180549091506bffffffffffffffffffffffff1685106103e957546c01000000000000000000000000900473ffffffffffffffffffffffffffffffffffffffff16949350505050565b50806103f481610ee1565b915050610373565b505060055473ffffffffffffffffffffffffffffffffffffffff1692915050565b610425610b5b565b43811161045e576040517fc2fdad0400000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff810361058d5773ffffffffffffffffffffffffffffffffffffffff8216156104d3576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600980547fffffffffffffffffffffffff00000000000000000000000000000000000000001690555f600a81905560015473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff167f7ac0520c61d87c77ffc308e23c801f5a351140175ec3f6356a3a890aa0cfdbf97fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff60405161058191815260200190565b60405180910390a35050565b6bffffffffffffffffffffffff8111156105d3576040517f9d7378d300000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b73ffffffffffffffffffffffffffffffffffffffff8216610620576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6009805473ffffffffffffffffffffffffffffffffffffffff84167fffffffffffffffffffffffff00000000000000000000000000000000000000009091168117909155600a82905561068860015473ffffffffffffffffffffffffffffffffffffffff1690565b73ffffffffffffffffffffffffffffffffffffffff167f7ac0520c61d87c77ffc308e23c801f5a351140175ec3f6356a3a890aa0cfdbf98360405161058191815260200190565b6106d7610b5b565b438111610710576040517fc2fdad0400000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff81036108325773ffffffffffffffffffffffffffffffffffffffff821615610785576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600780547fffffffffffffffffffffffff00000000000000000000000000000000000000001690555f6008819055805473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff167fd839b7efbcae2bd482a7aa1c098265fdb6b43c10e3e5138a8fa2b395a02eb9157fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff60405161058191815260200190565b6bffffffffffffffffffffffff811115610878576040517f9d7378d300000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b73ffffffffffffffffffffffffffffffffffffffff82166108c5576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6007805473ffffffffffffffffffffffffffffffffffffffff84167fffffffffffffffffffffffff00000000000000000000000000000000000000009091168117909155600882905561092c5f5473ffffffffffffffffffffffffffffffffffffffff1690565b73ffffffffffffffffffffffffffffffffffffffff167fd839b7efbcae2bd482a7aa1c098265fdb6b43c10e3e5138a8fa2b395a02eb9158360405161058191815260200190565b61097b610bac565b610983610ca1565b565b61098d610b5b565b6003805473ffffffffffffffffffffffffffffffffffffffff83167fffffffffffffffffffffffff000000000000000000000000000000000000000090911681179091556109f060025473ffffffffffffffffffffffffffffffffffffffff1690565b73ffffffffffffffffffffffffffffffffffffffff167fe5cd1c804f1c9cc6d7009e4c0fb532f0e2d8863524c3323a6b3790c3f80bf25c60405160405180910390a350565b5f43821115610a70576040517fdbe289ad00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600654821015610aac576040517fd2c461b000000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600b54805b8015610b3a575f600b610ac5600184610e9b565b81548110610ad557610ad5610eb4565b5f91825260209091200180549091506bffffffffffffffffffffffff168510610b2757546c01000000000000000000000000900473ffffffffffffffffffffffffffffffffffffffff16949350505050565b5080610b3281610ee1565b915050610ab1565b505060045473ffffffffffffffffffffffffffffffffffffffff1692915050565b60025473ffffffffffffffffffffffffffffffffffffffff163314610983576040517f7bfa4b9f00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b60075473ffffffffffffffffffffffffffffffffffffffff1680610bcd5750565b60085443811115610bdc575050565b5f805473ffffffffffffffffffffffffffffffffffffffff9384167fffffffffffffffffffffffff000000000000000000000000000000000000000091821681178355604080518082019091526bffffffffffffffffffffffff948516815260208101918252600b8054600181018255908552905191519095166c01000000000000000000000000029316929092177f0175b7a638427703f0dbe7bb9bbf987a2551717b34e79f33b5b1008d1fa01db990930192909255600780549091169055600855565b60095473ffffffffffffffffffffffffffffffffffffffff1680610cc25750565b600a5443811115610cd1575050565b6001805473ffffffffffffffffffffffffffffffffffffffff9384167fffffffffffffffffffffffff000000000000000000000000000000000000000091821681178355604080518082019091526bffffffffffffffffffffffff948516815260208101918252600c805494850181555f908152905191519095166c01000000000000000000000000029316929092177fdf6966c971051c3d54ec59162606531493a51404a002842f56009d7e5cf4a8c790910155600980549091169055600a55565b5f60208284031215610da4575f5ffd5b5035919050565b803573ffffffffffffffffffffffffffffffffffffffff81168114610dce575f5ffd5b919050565b5f5f60408385031215610de4575f5ffd5b610ded83610dab565b946020939093013593505050565b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011684010191505092915050565b5f60208284031215610e5e575f5ffd5b610e6782610dab565b9392505050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b81810381811115610eae57610eae610e6e565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b5f81610eef57610eef610e6e565b507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff019056fea164736f6c634300081e000a
```

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

`currentSystemAddress()` returns the value in `_currentSystemAddress`.
`currentSequencer()` returns the value in `_currentSequencer`.
Both are seeded at deploy time and updated only by `applyPendingChanges()`.

`systemAddressAt(blockNumber)` and `sequencerAt(blockNumber)` return the role address active at the given block.
They revert with `FutureBlock` if `blockNumber > block.number` and `BeforeInitialBlock` if `blockNumber < _initialFromBlock`.
Both roles share the same `_initialFromBlock`.

### Change Scheduling

Each role has independent `schedule*Change(newAddress, activationBlock)`.
To cancel a pending change, pass `activationBlock = type(uint256).max` and `newAddress = address(0)`; the call is handled as a special cancellation case before any other validation.
For all other values, `activationBlock` MUST be strictly greater than `block.number`; otherwise the call MUST revert with `InvalidActivationBlock()`.
`activationBlock` MUST also fit in `uint96`; if it exceeds `type(uint96).max`, the call MUST revert with `ActivationBlockTooLarge()`.
At most one pending change per role exists at a time; a new schedule overwrites the previous one.

### Pre-Block Apply

`applyPendingChanges()` is permissionless and applies both roles atomically.
It is called by the execution layer as a pre-block system call when a Rust-side pre-check confirms any role change is due.
For each role, if pending and due, it updates the current address, appends to the change history, and clears pending state.

The system call is issued with `gas_limit = max(block.gas_limit, 30_000_000)` instead of revm's upstream-fixed 30M default, matching the EIP-2935 / EIP-4788 pre-block calls.
This is required because the slot-rotation cost scales with REX dynamic storage gas (SALT bucket capacity), and a fixed 30M is no longer guaranteed to be sufficient on activation blocks.

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
Until the registry is active, the payload executor, txpool, and replay paths all assume the system-transaction sender equals `MEGA_SYSTEM_ADDRESS`, so the initial value is seeded at the activation boundary rather than being freely configurable, to avoid silently breaking those invariants at chain bootstrap.
Once the registry is active, the system address can be rotated via `scheduleNextSystemAddressChange` and `applyPendingChanges`.

**Validation constraints:** Both configurable address fields — `rex5_initial_sequencer` and `rex5_initial_admin` — must be non-zero.
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
