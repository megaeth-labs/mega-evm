---
description: High-Precision Timestamp system contract — sub-second timestamp oracle service backed by Oracle storage.
spec: Rex5
---

# High-Precision Timestamp

This page specifies the High-Precision Timestamp system contract.
It defines the address, interface, storage layout within the [Oracle](oracle.md) contract, and the per-transaction snapshot semantics.

## Motivation

EVM's `block.timestamp` has second-level granularity.
MegaETH produces blocks at sub-second intervals, so multiple blocks may share the same `block.timestamp`.
Contracts that depend on time ordering (e.g., auction deadlines, TWAP calculations) need a higher-resolution timestamp.

The High-Precision Timestamp contract provides a microsecond-resolution timestamp backed by Oracle storage, updated per transaction by the sequencer.

## Specification

### Address

The High-Precision Timestamp contract MUST exist at `HIGH_PRECISION_TIMESTAMP_ADDRESS`.

### Interface

The contract MUST expose the following methods:

```solidity
interface IHighPrecisionTimestamp {
    function timestamp() external view returns (uint256);
    function update(uint256 ts) external;
    function ORACLE_CONTRACT_ADDRESS() external view returns (address);
    function ALLOCATION_START() external view returns (uint256);
    function ALLOCATION_SIZE() external view returns (uint32);
}
```

The `timestamp()` method MUST return the value stored at Oracle slot `TIMESTAMP_BASE_SLOT`, interpreted as a `uint256` number of microseconds since Unix epoch.

The `update(uint256 ts)` method is not an on-chain write path.
It forwards to the [Oracle](oracle.md) `setSlot`, which authorizes the immediate caller against the current [system address](system-tx.md); because the immediate caller is the timestamp contract itself rather than the system address, an on-chain `update` call reverts.
The current timestamp value is instead maintained by the sequencer writing the per-transaction snapshot directly to Oracle storage (see [Service Semantics](#service-semantics)).

`ORACLE_CONTRACT_ADDRESS()` MUST return `ORACLE_CONTRACT_ADDRESS`.
`ALLOCATION_START()` MUST return `TIMESTAMP_BASE_SLOT`.
`ALLOCATION_SIZE()` MUST return `TIMESTAMP_MAX_SLOTS`.

### Storage Layout

The timestamp service allocation within [Oracle](oracle.md) storage MUST be:

| Slot Range | Meaning                                          |
| ---------- | ------------------------------------------------ |
| `0`        | Current high-precision timestamp in microseconds |
| `1`–`7`    | Reserved for future use                          |

### Service Semantics

For each user transaction that accesses timestamp-backed Oracle data, the sequencer MUST provide a per-transaction snapshot of the timestamp service.
That snapshot value MUST be written to Oracle storage via a [Mega System Transaction](system-tx.md) before the corresponding user transaction in the final block ordering.

If a transaction does not access timestamp-backed Oracle data, the protocol MUST NOT require a timestamp-service write for that transaction.

The timestamp service MUST satisfy the following guarantees:

- the value is expressed in microseconds,
- the value is capped above by `block.timestamp × 1,000,000`,
- successive transactions within a block observe non-decreasing timestamp values,
- and each transaction observes a stable per-transaction snapshot.

## Constants

| Constant                           | Value                                        | Description                                               |
| ---------------------------------- | -------------------------------------------- | --------------------------------------------------------- |
| `HIGH_PRECISION_TIMESTAMP_ADDRESS` | `0x6342000000000000000000000000000000000002` | Stable high-precision timestamp wrapper address           |
| `TIMESTAMP_BASE_SLOT`              | `0`                                          | Oracle storage base slot for the timestamp service        |
| `TIMESTAMP_MAX_SLOTS`              | `8`                                          | Number of Oracle slots reserved for the timestamp service |

## Rationale

**Why a separate contract instead of reading Oracle storage directly?**
The wrapper's address, interface, and storage mapping are part of MegaETH's verifiable behavior.
Nodes and contracts must agree on how the timestamp service is exposed, not only on the existence of underlying Oracle storage.

**Why microsecond resolution?**
Microseconds provide sufficient granularity for sub-second block intervals while fitting in a single `uint256` slot.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the High-Precision Timestamp contract.
