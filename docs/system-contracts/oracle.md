---
description: MegaETH Oracle system contract — address, storage layout, hint forwarding, and gas detention trigger.
spec: Rex3
---

# Oracle

This page specifies the Oracle system contract.
It defines the address, interface, restricted write behavior, storage access semantics, and hint forwarding.

## Motivation

MegaETH needs a canonical protocol-level storage backend for externally sourced data such as timestamps and other oracle-fed values.
That storage must be readable by contracts, writable by protocol-controlled maintenance transactions, and stable across specs.

## Specification

### Address

The Oracle system contract MUST exist at `ORACLE_CONTRACT_ADDRESS`.

### Bytecode

The Oracle constructor takes `MEGA_SYSTEM_ADDRESS` as an immutable parameter.
A node MUST deploy the bytecode version corresponding to the active spec.

Source: [`Oracle.sol`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/contracts/Oracle.sol)

| Version | Code Hash |
| ------- | --------- |
| `1.1.0` | `0x06df675a69e53ea2a3c948521e330b3801740fede324a1cef2044418f8e09242` |

### Public Read Interface

The Oracle contract MUST expose the following externally callable read methods:

```solidity
interface IOracle {
    function getSlot(uint256 slot) external view returns (bytes32 value);
    function getSlots(uint256[] calldata slots) external view returns (bytes32[] memory values);
}
```

`getSlot` MUST return the storage value at the specified slot.
`getSlots` MUST return the storage values at the specified slots in the same order as the input array.

### Restricted Write Interface

The Oracle contract MUST expose the following write and log-emission methods:

```solidity
interface IOracle {
    function setSlot(uint256 slot, bytes32 value) external;
    function setSlots(uint256[] calldata slots, bytes32[] calldata values) external;
    function emitLog(bytes32 topic, bytes calldata data) external;
    function emitLogs(bytes32 topic, bytes[] calldata dataVector) external;
}
```

The methods above MUST be callable only by `MEGA_SYSTEM_ADDRESS`.
Calls from any other sender MUST revert with `NotSystemAddress()`.

For `setSlots`, if the `slots` and `values` array lengths differ, the call MUST revert with `InvalidLength(uint256 slotsLength, uint256 valuesLength)`.

### Auxiliary Interface

The Oracle contract MUST expose the following auxiliary methods:

```solidity
interface IOracle {
    function multiCall(bytes[] calldata data) external returns (bytes[] memory results);
    function sendHint(bytes32 topic, bytes calldata data) external view;
}
```

`multiCall` MUST execute each payload by `DELEGATECALL` into the Oracle contract and MUST return the results in order.
If any delegated call fails, `multiCall` MUST revert and MUST bubble up the revert data if present.

`sendHint` MUST be externally callable and MUST be a no-op at the Solidity bytecode level.

### Storage Access Semantics

**Reads.**
`getSlot` and `getSlots` read Oracle storage via `SLOAD`.
The node MAY serve Oracle reads from an external data source that provides realtime, per-transaction values.
When an `SLOAD` targets `ORACLE_CONTRACT_ADDRESS`, the node MUST first consult the external data source.
If it provides a value for the requested slot, that value MUST be returned.
Otherwise, the node MUST return the on-chain storage value.

**Writes.**
`setSlot` and `setSlots` write Oracle storage via `SSTORE`.
These methods are restricted to `MEGA_SYSTEM_ADDRESS` (see [Restricted Write Interface](#restricted-write-interface)).

**On-chain persistence.**
When the external data source provides a value for a read, the sequencer MUST persist that value on-chain by inserting a [Mega System Transaction](system-tx.md) that calls `setSlot` or `setSlots`.
This system transaction MUST be ordered before the user transaction that triggered the read, so that full nodes replaying the block observe the same storage state.

### Hint Forwarding

`sendHint` is the only function in Oracle system contract that participates in [call interception](interception.md).
All other Oracle functions (`getSlot`, `getSlots`, `setSlot`, `setSlots`, `emitLog`, `emitLogs`, `multiCall`) execute via ordinary contract bytecode only.

When a `CALL` or `STATICCALL` targets `ORACLE_CONTRACT_ADDRESS` and the input matches the `sendHint(bytes32,bytes)` selector, the node MUST forward the decoded `topic` and `data` to the external oracle backend as a side effect.
The call MUST then fall through — the Oracle contract's deployed `sendHint` function body executes as ordinary bytecode.

Because the Solidity implementation of `sendHint` is a no-op `view` function, the net observable behavior is the combination of:

- hint forwarding to the oracle backend (side effect), and
- normal bytecode execution of the no-op function body (which returns successfully with no output).

Calls to `ORACLE_CONTRACT_ADDRESS` that do not match the `sendHint` selector MUST fall through without any side effect.

If a transaction calls `sendHint` and subsequently reads an Oracle slot, the hint MUST be delivered to the oracle backend before the read is served.

### Gas and Detention Semantics

The following gas and detention rules MUST apply:

- `SLOAD` against Oracle storage MUST use the cold access gas cost.
- Oracle storage reads MUST participate in [gas detention](../evm/gas-detention.md).
- `CALL` or `STATICCALL` to the Oracle contract address alone MUST NOT trigger oracle detention unless Oracle storage is actually read.
- `DELEGATECALL` to the Oracle contract MUST NOT trigger oracle detention solely by targeting the Oracle address.

### Versioning

Pre-[Rex2](../upgrades/rex2.md), the deployed Oracle bytecode does not include `sendHint`.
From [Rex2](../upgrades/rex2.md) onward, the stable Oracle bytecode includes `sendHint`.

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `ORACLE_CONTRACT_ADDRESS` | `0x6342000000000000000000000000000000000001` | Stable Oracle system-contract address |

## Rationale

**Why centralize oracle-backed data in one contract?**
Oracle-backed protocol data needs a single canonical storage location so all contracts and all nodes observe the same values under the same addressing scheme.

**Why restrict writes to `MEGA_SYSTEM_ADDRESS`?**
Externally sourced oracle values are part of protocol-maintained state.
Allowing arbitrary writes would destroy the meaning of oracle-backed data and make the values untrustworthy as protocol inputs.

**Why use a per-transaction external data source instead of pre-populating all oracle data?**
Traditional oracle designs require all data to be written on-chain before any transaction can read it, even if most transactions never access oracle data.
The external data source enables a realtime lazy oracle: values are only fetched and persisted when a transaction actually reads them.
This avoids unnecessary system transactions for data that no one consumes, reduces block overhead, and allows oracle data to be as fresh as the moment of access rather than the moment of block construction.
The sequencer's frontrunning system transaction ensures that the lazily served value is still persisted on-chain for full nodes and verifiers that replay the block.

**Why intercept `sendHint` during call interception?**
Hint forwarding depends on external backend behavior that cannot be expressed by on-chain bytecode alone.
The no-op Solidity body provides a stable interface, while the [call interception](interception.md) mechanism supplies the protocol-level side effect.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the Oracle contract.
- [Rex2](../upgrades/rex2.md) added the `sendHint` entry point to the deployed Oracle bytecode.
- [Rex3](../upgrades/rex3.md) changed oracle detention to SLOAD-based triggering and raised the oracle detention cap to 20M.
