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

Deployed bytecode:

```
0x608060405234801561000f575f5ffd5b50600436106100e5575f3560e01c806354fd4d5011610088578063de7a36af11610063578063de7a36af146101f7578063e3093f5014610214578063e812903014610227578063f851a44014610245575f5ffd5b806354fd4d501461019d57806375829def146101dc578063c7455c42146101ef575f5ffd5b80632b659fd8116100c35780632b659fd81461014e5780633411f3c31461016157806344d744591461017457806348a2cf6d1461017c575f5ffd5b80630e18b681146100e9578063129c8240146100f35780632678224714610130575b5f5ffd5b6100f1610263565b005b610106610101366004611274565b610336565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b60035473ffffffffffffffffffffffffffffffffffffffff16610106565b6100f161015c3660046112b3565b61045c565b6100f161016f366004611336565b6107e1565b6100f1610a91565b61018f61018a366004611336565b610aa3565b604051908152602001610127565b604080518082018252600581527f322e302e3000000000000000000000000000000000000000000000000000000060208201529051610127919061135e565b6100f16101ea3660046113b1565b610c5b565b600d5461018f565b5f5473ffffffffffffffffffffffffffffffffffffffff16610106565b610106610222366004611274565b610d0b565b60015473ffffffffffffffffffffffffffffffffffffffff16610106565b60025473ffffffffffffffffffffffffffffffffffffffff16610106565b60035473ffffffffffffffffffffffffffffffffffffffff163381146102b5576040517f058d9a1b00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6002805473ffffffffffffffffffffffffffffffffffffffff8381167fffffffffffffffffffffffff00000000000000000000000000000000000000008084168217909455600380549094169093556040519116919082907ff8ccb027dfcd135e000e9d45e6cc2d662578a8825d4c45b5e32e0adf67e79ec6905f90a35050565b5f43821115610371576040517fdbe289ad00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6006548210156103ad576040517fd2c461b000000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600c54805b801561043b575f600c6103c66001846113fe565b815481106103d6576103d6611411565b5f91825260209091200180549091506bffffffffffffffffffffffff16851061042857546c01000000000000000000000000900473ffffffffffffffffffffffffffffffffffffffff16949350505050565b50806104338161143e565b9150506103b2565b505060055473ffffffffffffffffffffffffffffffffffffffff1692915050565b610464610e31565b43831161049d576040517fc2fdad0400000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff83036105cd5773ffffffffffffffffffffffffffffffffffffffff841615610512576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600980547fffffffffffffffffffffffff00000000000000000000000000000000000000001690555f600a81905560015473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff167f7ac0520c61d87c77ffc308e23c801f5a351140175ec3f6356a3a890aa0cfdbf97fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff6040516105c091815260200190565b60405180910390a36107db565b6bffffffffffffffffffffffff831115610613576040517f9d7378d300000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b73ffffffffffffffffffffffffffffffffffffffff8416610660576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600d5461066d9043611472565b8310156106a6576040517f11ede46e00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b5f6106bb6106b48686610aa3565b8484610e82565b90508473ffffffffffffffffffffffffffffffffffffffff168173ffffffffffffffffffffffffffffffffffffffff1614610722576040517fe415bbae00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6009805473ffffffffffffffffffffffffffffffffffffffff87167fffffffffffffffffffffffff00000000000000000000000000000000000000009091168117909155600a85905561078a60015473ffffffffffffffffffffffffffffffffffffffff1690565b73ffffffffffffffffffffffffffffffffffffffff167f7ac0520c61d87c77ffc308e23c801f5a351140175ec3f6356a3a890aa0cfdbf9866040516107d191815260200190565b60405180910390a3505b50505050565b6107e9610e31565b438111610822576040517fc2fdad0400000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff81036109505773ffffffffffffffffffffffffffffffffffffffff821615610897576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600780547fffffffffffffffffffffffff00000000000000000000000000000000000000001690555f6008819055805473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff167fd839b7efbcae2bd482a7aa1c098265fdb6b43c10e3e5138a8fa2b395a02eb9157fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff60405161094491815260200190565b60405180910390a35050565b6bffffffffffffffffffffffff811115610996576040517f9d7378d300000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b73ffffffffffffffffffffffffffffffffffffffff82166109e3576040517fd92e233d00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b6007805473ffffffffffffffffffffffffffffffffffffffff84167fffffffffffffffffffffffff000000000000000000000000000000000000000090911681179091556008829055610a4a5f5473ffffffffffffffffffffffffffffffffffffffff1690565b73ffffffffffffffffffffffffffffffffffffffff167fd839b7efbcae2bd482a7aa1c098265fdb6b43c10e3e5138a8fa2b395a02eb9158360405161094491815260200190565b610a9961108c565b610aa1611181565b565b604080518082018252601981527f4d6567614554482053657175656e63657252656769737472790000000000000060209182015281518083018352600181527f31000000000000000000000000000000000000000000000000000000000000009082015281517f8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f818301527f85da835bd1cf371efdb3f9056e5a3a6245af1400fa0ce2e878a198765a4eb9c2818401527fc89efdaa54c0f20c7adf612882df0950f5a951637e0307cdcb4c672f298b8bc660608201524660808201523060a0808301919091528351808303909101815260c0820184528051908301207faebf84f20bcc14afc14200ed16cfb9314ef205d4d1d8fbcc0a256c39a3e79aa960e083015273ffffffffffffffffffffffffffffffffffffffff861661010083015261012080830186905284518084039091018152610140830185528051908401207f190100000000000000000000000000000000000000000000000000000000000061016084015261016283019190915261018280830191909152835180830390910181526101a290910190925281519101205b92915050565b610c63610e31565b6003805473ffffffffffffffffffffffffffffffffffffffff83167fffffffffffffffffffffffff00000000000000000000000000000000000000009091168117909155610cc660025473ffffffffffffffffffffffffffffffffffffffff1690565b73ffffffffffffffffffffffffffffffffffffffff167fe5cd1c804f1c9cc6d7009e4c0fb532f0e2d8863524c3323a6b3790c3f80bf25c60405160405180910390a350565b5f43821115610d46576040517fdbe289ad00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600654821015610d82576040517fd2c461b000000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b600b54805b8015610e10575f600b610d9b6001846113fe565b81548110610dab57610dab611411565b5f91825260209091200180549091506bffffffffffffffffffffffff168510610dfd57546c01000000000000000000000000900473ffffffffffffffffffffffffffffffffffffffff16949350505050565b5080610e088161143e565b915050610d87565b505060045473ffffffffffffffffffffffffffffffffffffffff1692915050565b60025473ffffffffffffffffffffffffffffffffffffffff163314610aa1576040517f7bfa4b9f00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b5f60418214610ebd576040517fe415bbae00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b5f610ecb6020828587611485565b610ed4916114ac565b90505f610ee5604060208688611485565b610eee916114ac565b90505f85856040818110610f0457610f04611411565b919091013560f81c9150507f7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0821115610f69576040517fe415bbae00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b8060ff16601b14158015610f8157508060ff16601c14155b15610fb8576040517fe415bbae00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b604080515f808252602082018084528a905260ff841692820192909252606081018590526080810184905260019060a0016020604051602081039080840390855afa158015611009573d5f5f3e3d5ffd5b50506040517fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0015191505073ffffffffffffffffffffffffffffffffffffffff8116611081576040517fe415bbae00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b979650505050505050565b60075473ffffffffffffffffffffffffffffffffffffffff16806110ad5750565b600854438111156110bc575050565b5f805473ffffffffffffffffffffffffffffffffffffffff9384167fffffffffffffffffffffffff000000000000000000000000000000000000000091821681178355604080518082019091526bffffffffffffffffffffffff948516815260208101918252600b8054600181018255908552905191519095166c01000000000000000000000000029316929092177f0175b7a638427703f0dbe7bb9bbf987a2551717b34e79f33b5b1008d1fa01db990930192909255600780549091169055600855565b60095473ffffffffffffffffffffffffffffffffffffffff16806111a25750565b600a54438111156111b1575050565b6001805473ffffffffffffffffffffffffffffffffffffffff9384167fffffffffffffffffffffffff000000000000000000000000000000000000000091821681178355604080518082019091526bffffffffffffffffffffffff948516815260208101918252600c805494850181555f908152905191519095166c01000000000000000000000000029316929092177fdf6966c971051c3d54ec59162606531493a51404a002842f56009d7e5cf4a8c790910155600980549091169055600a55565b5f60208284031215611284575f5ffd5b5035919050565b803573ffffffffffffffffffffffffffffffffffffffff811681146112ae575f5ffd5b919050565b5f5f5f5f606085870312156112c6575f5ffd5b6112cf8561128b565b935060208501359250604085013567ffffffffffffffff8111156112f1575f5ffd5b8501601f81018713611301575f5ffd5b803567ffffffffffffffff811115611317575f5ffd5b876020828401011115611328575f5ffd5b949793965060200194505050565b5f5f60408385031215611347575f5ffd5b6113508361128b565b946020939093013593505050565b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011684010191505092915050565b5f602082840312156113c1575f5ffd5b6113ca8261128b565b9392505050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b81810381811115610c5557610c556113d1565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b5f8161144c5761144c6113d1565b507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff0190565b80820180821115610c5557610c556113d1565b5f5f85851115611493575f5ffd5b8386111561149f575f5ffd5b5050820193919092039150565b80356020831015610c55577fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff602084900360031b1b169291505056fea164736f6c634300081e000a
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
