---
description: MegaETH Oracle system contract — address, storage layout, hint forwarding, and gas detention trigger.
spec: Rex5
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

A node MUST deploy the bytecode version corresponding to the active spec.
The current version (2.0.0) reads its authorized system address dynamically from [`SequencerRegistry.currentSystemAddress()`](sequencer-registry.md).
Earlier versions 1.0.0 and 1.1.0 instead took `MEGA_SYSTEM_ADDRESS` as a constructor `immutable`.

#### Version 1.0.0

Since: [MiniRex](../upgrades/minirex.md)

Code hash: `0xe9b044afb735a0f569faeb248088b4f267578f60722f87d06ec3867b250a2c34`

Deployed bytecode: `0x608060405234801561000f57...` (full bytecode: [`Oracle-1.0.0.json`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/artifacts/Oracle-1.0.0.json), `deployedBytecode` field).

To verify the code hash, from the repository root:

```bash
cast keccak $(jq -r .deployedBytecode crates/system-contracts/artifacts/Oracle-1.0.0.json)
```

#### Version 1.1.0

Since: [Rex2](../upgrades/rex2.md)

Code hash: `0x06df675a69e53ea2a3c948521e330b3801740fede324a1cef2044418f8e09242`

Deployed bytecode: `0x608060405234801561000f57...` (full bytecode: [`Oracle-1.1.0.json`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/artifacts/Oracle-1.1.0.json), `deployedBytecode` field).

To verify the code hash, from the repository root:

```bash
cast keccak $(jq -r .deployedBytecode crates/system-contracts/artifacts/Oracle-1.1.0.json)
```

#### Version 2.0.0

Since: [Rex5](../upgrades/rex5.md)

Code hash: `0x71a65239db8d0f1bb765fad36e34f57600420d103a4401ef7555bd50b229dc55`

Deployed bytecode: `0x608060405234801561000f57...` (full bytecode: [`Oracle-2.0.0.json`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/artifacts/Oracle-2.0.0.json), `deployedBytecode` field).

To verify the code hash, from the repository root:

```bash
cast keccak $(jq -r .deployedBytecode crates/system-contracts/artifacts/Oracle-2.0.0.json)
```

The authorization check MUST read the authorized address from [`SequencerRegistry.currentSystemAddress()`](sequencer-registry.md) instead of a constructor `immutable`.
This enables system address change without redeploying the Oracle contract.
All other functionality is preserved from v1.1.0.

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

The methods above MUST be callable only by [`MEGA_SYSTEM_ADDRESS`](system-tx.md).
Calls from any other sender MUST revert with `NotSystemAddress()`.

For `setSlots`, if the `slots` and `values` array lengths differ, the call MUST revert with `InvalidLength(uint256 slotsLength, uint256 valuesLength)`.

#### Authorization Check Ordering

For `setSlot`, `setSlots`, `emitLog`, and `emitLogs`, the function body — including all `SSTORE` operations and `LOG` emissions — MUST execute before the caller authorization check.
On an unauthorized call, the body MUST run to completion (consuming gas for all operations it performs — `SSTORE` writes, `LOG` emissions, and any loop iterations), and the call MUST then revert with `NotSystemAddress()`.
The revert MUST roll back all storage writes and log emissions performed by the body, leaving no observable state change at the transaction boundary if the surrounding transaction does not catch the revert.

Authorization for `setSlots` is checked after the array-length equality check.
If the lengths differ, the call MUST revert with `InvalidLength(uint256, uint256)` before any `SSTORE` runs and before the authorization check.

{% hint style="info" %}
**Design intent.** Running the body before the authorization check makes the would-be storage writes and log emissions visible to off-chain EVM inspectors and trace consumers, even when the call ultimately reverts.
This is intentional and is part of the observable behavior of the Oracle contract.
{% endhint %}

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

When a `CALL` or `STATICCALL` targets `ORACLE_CONTRACT_ADDRESS` with the `sendHint(bytes32,bytes)` selector, the node MUST forward the `topic` and `data` to the external oracle backend as a side effect, subject to the admission conditions below.
The call MUST then fall through — the Oracle contract's deployed `sendHint` function body executes as ordinary bytecode.

Because the Solidity implementation of `sendHint` is a no-op `view` function, the net observable behavior is the combination of:

- hint forwarding to the oracle backend (side effect), and
- normal bytecode execution of the no-op function body (which returns successfully with no output).

Calls to `ORACLE_CONTRACT_ADDRESS` that do not match the `sendHint` selector MUST fall through without any side effect.

When the call's gas limit is greater than zero and the calldata's leading four bytes match the `sendHint` selector, the node MUST charge the full byte length of the call's calldata — the entire call input — against the transaction's data-size resource lane before attempting to decode it.
A call whose gas limit is zero MUST NOT be charged and MUST fall through to the on-chain Oracle bytecode for canonical handling.
This charge MUST apply regardless of whether the calldata subsequently decodes successfully.
If decoding the calldata fails, the node MUST NOT invoke the hint callback; the byte charge still applies.

The node MUST forward a hint to the off-chain backend only when the call's gas limit is greater than zero, the leading four bytes of the calldata match the `sendHint` selector, the calldata decodes as a valid `sendHint(bytes32 topic, bytes data)` invocation, AND recording the calldata byte length keeps the transaction within its data-size limit.
If the gas limit is zero, or the selector does not match, or decoding fails, the node MUST NOT invoke the hint callback.
A zero-gas-limit, selector-mismatch, or decode-failure call falls through to the on-chain Oracle bytecode for canonical handling.
A data-size overflow halts the transaction with the canonical data-size `OutOfGas` failure.

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

**Upgrade storage semantics.**
When the Oracle account's bytecode is upgraded, the fate of existing Oracle storage is consensus-critical.

A bytecode upgrade MUST NOT mark the Oracle account as newly created.
Existing Oracle storage MUST be preserved across the upgrade.

A historical exception applies to the Rex2 upgrade only: that upgrade marked the Oracle account as newly created, clearing any storage accumulated under the previous bytecode version, and canonical mainnet state reflects that the Oracle storage present at the Rex2 activation boundary was cleared.

## Constants

| Constant                  | Value                                        | Description                           |
| ------------------------- | -------------------------------------------- | ------------------------------------- |
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

**Why run the restricted-write body before the authorization check?**
The authorization check is intentionally placed after the function body so that off-chain EVM inspectors observe the would-be storage writes and log emissions even when the call reverts.
This preserves trace visibility for simulators that exercise the Oracle's write paths from non-system callers, at the cost of allowing unauthorized callers to perform unbounded body work that is rolled back on revert.
The trade-off and its consequences are spelled out in [Security Considerations](#security-considerations).

## Security Considerations

**Block gas consumption by unauthorized callers.**
Because the body of `setSlot`, `setSlots`, `emitLog`, and `emitLogs` runs before the authorization check, an unauthorized caller MAY supply arbitrarily large input arrays to `setSlots` or `emitLogs` and cause the body to iterate and `SSTORE` (or emit `LOG`) up to the caller's gas allowance before the call reverts.
The unauthorized caller pays the full gas cost of this work, but the gas consumed counts against the block's gas budget.
This is not differentiable from any other unbounded-loop gas-burn pattern reachable from ordinary EVM bytecode, so it does not imply a contract change.
A node implementation that gates the `SSTORE` or `LOG` operations on authorization first would produce gas accounting that disagrees with the canonical Oracle bytecode and MUST NOT be used.

**Execution trace semantics for trace consumers.**
A trace produced by `debug_traceTransaction` or any equivalent inspector for an Oracle write call MAY contain `SSTORE` and `LOG` operations even when the surrounding call reverts.
A consumer that infers permanent state changes from raw trace operations alone MUST NOT treat such operations as committed without first checking the final transaction status: if the transaction reverts, all `SSTORE`s and `LOG`s in its trace MUST be discarded.
A consumer that fails to filter by transaction status MAY misattribute Oracle slot updates to unauthorized callers and corrupt downstream indexing or replay state.

**Invariants preserved.**
The Oracle's restricted-write methods preserve the invariant that, at the transaction boundary, no Oracle storage slot is modified and no `Log` event is emitted by an unauthorized caller.
Per-frame trace visibility of the would-be writes is informative only and MUST NOT be interpreted as a state change.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the Oracle contract.
- [Rex2](../upgrades/rex2.md) added the `sendHint` entry point to the deployed Oracle bytecode.
- [Rex3](../upgrades/rex3.md) changed oracle detention to SLOAD-based triggering and raised the oracle detention cap to 20M.
- [Rex5](../upgrades/rex5.md) replaced the constructor `immutable` authority with a dynamic read from `SequencerRegistry.currentSystemAddress()` (Oracle v2.0.0), preserved existing Oracle storage across in-place bytecode upgrades, and gated hint forwarding on a positive gas limit while metering the full hint calldata against the transaction data-size lane before decoding.
