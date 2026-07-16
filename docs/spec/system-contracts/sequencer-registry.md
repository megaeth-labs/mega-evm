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

Deployed bytecode: `0x608060405234801561000f57...` (full bytecode: [`SequencerRegistry-1.0.0.json`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/artifacts/SequencerRegistry-1.0.0.json), `deployedBytecode` field).

To verify the code hash, from the repository root:

```bash
cast keccak $(jq -r .deployedBytecode crates/system-contracts/artifacts/SequencerRegistry-1.0.0.json)
```

A node MUST deploy the contract via raw state patch with initial storage seeded at deploy time.
A node MUST NOT execute a constructor during deployment.

<details>
<summary>Rex6 (unstable): Version 2.0.0 bytecode and in-place upgrade</summary>

#### Version 2.0.0

Since: [Rex6](../upgrades/rex6.md)

Code hash: `0xabd7e8f1c8f0f9ca0346df585b65d9791a7e9ba5c431cc4eda5d2c2f1f5e9c43`

Version 2.0.0 hardens sequencer rotation: `scheduleNextSequencerChange` gains a third parameter carrying the new sequencer key's EIP-712 possession proof, enforces a minimum scheduling-to-activation delay, and exposes the `minRotationDelay()` and `rotationDigest()` views.
This is a breaking ABI change for the sequencer-role scheduling entry point (hence the major version bump); every other method keeps its version 1.0.0 ABI and behavior, with `version()` reporting `2.0.0`.

At the Rex6 activation block, a node MUST upgrade an existing version 1.0.0 registry **in place**: swap the bytecode to version 2.0.0, write the new `_minRotationDelay` slot, and preserve every other storage slot (roles, pending changes, histories).
A pending sequencer change scheduled under version 1.0.0 MUST still activate normally under version 2.0.0.

Deployed bytecode: `0x608060405234801561000f57...` (full bytecode: [`SequencerRegistry-2.0.0.json`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/artifacts/SequencerRegistry-2.0.0.json), `deployedBytecode` field).

To verify the code hash:

```bash
cast keccak $(jq -r .deployedBytecode crates/system-contracts/artifacts/SequencerRegistry-2.0.0.json)
```

</details>

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

<details>
<summary>Rex6 (unstable): `_minRotationDelay` storage slot</summary>

Version 2.0.0 appends one slot:

| Slot | Name                | Type      |
| ---- | ------------------- | --------- |
| 13   | `_minRotationDelay` | `uint256` |

Slot 13 exists only in version 2.0.0 and later; slots 0–12 are identical across versions.

</details>

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

<details>
<summary>Rex6 (unstable): rotation-hardening interface additions</summary>

Version 2.0.0 replaces the sequencer scheduling entry point and adds two views and two errors:

```solidity
// Replaces the version 1.0.0 signature
function scheduleNextSequencerChange(
    address newSequencer,
    uint256 activationBlock,
    bytes calldata newSequencerSignature
) external;

// New views
function minRotationDelay() external view returns (uint256);
function rotationDigest(address newSequencer, uint256 activationBlock) external view returns (bytes32);

// New errors
error RotationDelayTooShort();
error InvalidRotationProof();
```

Every other method keeps its version 1.0.0 signature, and `version()` reports `2.0.0`.

</details>

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

<details>
<summary>Rex6 (unstable): sequencer rotation hardening (possession proof + minimum delay)</summary>

From version 2.0.0, the sequencer role additionally enforces the rules below; the system-address role keeps the two-parameter `scheduleNextSystemAddressChange(newSystemAddress, activationBlock)` signature and only the rules above.

Scheduling a sequencer change with an address whose private key nobody holds creates an unrecoverable liveness failure: at the activation block, mini-block production requires the new key's signature, and the admin transaction that could fix the registry itself requires block production.
Version 2.0.0 closes this hole at the contract entry point with two independent guards on `scheduleNextSequencerChange`.

**Minimum rotation delay.**
The activation block MUST satisfy `activationBlock >= block.number + minRotationDelay()`; otherwise the call MUST revert with `RotationDelayTooShort()`.
This guarantees a reaction window between scheduling and activation in which a bad registration can be detected and cancelled.
The delay value is read from the `_minRotationDelay` storage slot, which is seeded by the node at deploy/upgrade time and has no on-chain setter; changing it requires a bytecode upgrade.

**Possession proof.**
`newSequencerSignature` MUST be a 65-byte `r || s || v` signature by the `newSequencer` key over the EIP-712 digest returned by `rotationDigest(newSequencer, activationBlock)`; otherwise the call MUST revert with `InvalidRotationProof()`.
The digest is computed per [EIP-712](https://eips.ethereum.org/EIPS/eip-712) with:

- Domain: `name = "MegaETH SequencerRegistry"`, `version = "1"`, `chainId = block.chainid`, `verifyingContract = address(this)`.
  The domain version is independent of the contract's semantic version and only changes if the signing scheme itself changes.
- Domain type hash: `keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")` = `0x8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f`.
- Struct type hash: `keccak256("SequencerRotation(address newSequencer,uint256 activationBlock)")` = `0xaebf84f20bcc14afc14200ed16cfb9314ef205d4d1d8fbcc0a256c39a3e79aa9`.
- Digest: `keccak256(0x1901 || domainSeparator || keccak256(abi.encode(structTypeHash, newSequencer, activationBlock)))`.

The domain separator MUST be computed at call time (not cached at construction), because the contract is installed via raw state patch and no constructor runs.

Signature validation MUST reject, with `InvalidRotationProof()`:

- a signature whose length is not exactly 65 bytes;
- a malleable signature with `s > secp256k1n / 2`;
- a `v` value outside `{27, 28}`;
- a failed `ecrecover` (recovered address zero);
- a recovered address different from `newSequencer`.

**Validation order.**
Checks MUST run cheapest-first: `InvalidActivationBlock`, the cancellation special case, `ActivationBlockTooLarge`, `ZeroAddress`, `RotationDelayTooShort`, and the possession proof last, so `ecrecover` only runs on otherwise-valid inputs.

**Cancellation is exempt.**
The cancellation case (`newSequencer = address(0)`, `activationBlock = type(uint256).max`) MUST NOT require a signature, so a rotation to a key that turns out to be lost can always be cancelled.

**Replay protection.**
The signature commits to `(chainId, verifyingContract, newSequencer, activationBlock)` and needs no nonce:

- Only the admin can call `scheduleNextSequencerChange`, so a third party cannot replay a captured signature.
- A signature cannot be reused for a different `newSequencer` or `activationBlock` — the digest would differ.
- Replaying the identical `(newSequencer, activationBlock)` pair merely reinstates the same intended rotation, which is harmless by construction.
- A stale signature naturally expires: once `block.number + minRotationDelay()` exceeds its `activationBlock`, no schedule call can accept it.

</details>

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

<details>
<summary>Rex6 (unstable): `_minRotationDelay` seeding</summary>

From Rex6, the node MUST also seed the `_minRotationDelay` slot from `SequencerRegistryRex6Config.rex6_min_rotation_delay` on the chain's hardfork configuration:

- On a fresh deploy at Rex6 (registry never existed), the node MUST write all 6 bootstrap slots plus `_minRotationDelay` and install the version 2.0.0 bytecode directly.
- On the in-place version 1.0.0 → 2.0.0 upgrade at the Rex6 activation block, the node MUST write only `_minRotationDelay` and MUST leave every other slot untouched.

`rex6_min_rotation_delay` MUST be non-zero; the chain configuration MUST reject a zero value at load time, because a zero delay disables the reaction window the field exists to guarantee.

</details>

## Constants

| Name                         | Value                                        | Description      |
| ---------------------------- | -------------------------------------------- | ---------------- |
| `SEQUENCER_REGISTRY_ADDRESS` | `0x6342000000000000000000000000000000000006` | Contract address |

<details>
<summary>Rex6 (unstable): rotation-proof constants</summary>

| Name                          | Value                                                                | Description                                  |
| ----------------------------- | -------------------------------------------------------------------- | -------------------------------------------- |
| `SEQUENCER_ROTATION_TYPEHASH` | `0xaebf84f20bcc14afc14200ed16cfb9314ef205d4d1d8fbcc0a256c39a3e79aa9` | EIP-712 struct type hash for rotation proofs |
| EIP-712 domain name           | `MegaETH SequencerRegistry`                                          | Rotation-proof signing domain                |
| EIP-712 domain version        | `1`                                                                  | Independent of the contract semantic version |

</details>

## Rationale

**Why two roles instead of one?**
The Oracle/system-tx sender and the mini-block signing key are different operational concerns.
Coupling them means changing one silently revokes the other, which would break Oracle authority on the first sequencer key change.

**Why a pre-block system call for role changes?**
Applying a role change as a regular transaction would change role addresses mid-block, breaking block-stability.

**Why deploy-time storage seeding instead of constant bootstrap?**
The `_initialFromBlock` depends on the Rex5 activation block number, which is not known at compile time.
Seeding all initial values at deploy time keeps the bootstrap mechanism uniform.

<details>
<summary>Rex6 (unstable): rotation-hardening rationale</summary>

**Why require a possession proof at scheduling time?**
A sequencer rotation to a typo'd or otherwise unheld address deadlocks the chain at the activation block, and no on-chain recovery exists once block production halts.
Requiring the new key to sign the exact rotation it authorizes moves the failure to the scheduling transaction, where it reverts harmlessly.

**Why is `_minRotationDelay` config-seeded instead of a Solidity constant?**
A constant baked into the bytecode would force every network — including devnets and e2e test harnesses — to wait the same real wall-clock delay for a live rotation test.
A seeded storage value keeps one canonical version 2.0.0 code hash across all networks while letting each network choose its own delay, exactly like the existing role seeding.

**Why no nonce in the rotation proof?**
Scheduling is admin-gated, the signature binds the exact `(chainId, contract, newSequencer, activationBlock)` tuple, replaying the identical tuple is idempotent, and the minimum-delay check naturally expires stale signatures.
A nonce would add a storage slot and a tooling burden without excluding any additional attack.

</details>

## Spec History

- [Rex5](../upgrades/rex5.md) introduced the `SequencerRegistry` contract with dual roles.
- [Rex6](../upgrades/rex6.md) (**unstable**) — upgrades the contract to version 2.0.0: sequencer rotation requires the new key's EIP-712 possession proof and a minimum scheduling-to-activation delay (`_minRotationDelay`, slot 13).
