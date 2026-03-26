# Oracle Contract

## Overview

The Oracle contract is the centralized storage backend for MegaEVM's [oracle services](../oracle-services/overview.md).
It provides a simple key-value store where the sequencer writes off-chain data (timestamps, price feeds, etc.) via [system transactions](system-tx.md), and oracle service wrapper contracts read from it.

{% hint style="success" %}
**For contract developers**: You typically do not interact with the Oracle contract directly.
Use the higher-level [oracle services](../oracle-services/overview.md) instead — they provide typed interfaces and dedicated wrapper contracts (e.g., [High-Precision Timestamp](../oracle-services/timestamp.md) at `0x6342...0002`).
{% endhint %}

{% hint style="info" %}
**Trust Assumption**: Oracle data is published by the sequencer.
Using oracle services requires trusting the sequencer to provide accurate values.
{% endhint %}

## Contract Details

**Address**: `0x6342000000000000000000000000000000000001`

**Source**: [`Oracle.sol`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/contracts/Oracle.sol)

**Key Properties**:
- **Simple storage model** — Direct access to storage slots via `uint256` keys
- **Restricted writes** — Only `MEGA_SYSTEM_ADDRESS` can write oracle data
- **Public reads** — Anyone can read oracle data
- **Versioned bytecode** — Pre-[Rex2](../hardfork-spec.md#rex2) deploys Oracle v1.0.0, and Rex2+ deploys Oracle v1.1.0 with `sendHint`

## Interface

### Public — Read Methods

```solidity
interface IOracle {
    /// @notice Reads a value from a specific storage slot
    function getSlot(uint256 slot) external view returns (bytes32 value);

    /// @notice Reads values from multiple storage slots
    function getSlots(uint256[] calldata slots)
        external view returns (bytes32[] memory values);
}
```

### Internal — Used by Oracle Services

The following methods are used internally by [oracle service](../oracle-services/overview.md) wrapper contracts to communicate with the sequencer.
They are not intended for direct use by application contracts.

```solidity
interface IOracle {
    /// @notice Sends a hint to the sequencer's oracle backend.
    /// @dev Available from Rex2 onward. Used by oracle service wrappers
    /// to request data from the sequencer at runtime (e.g., requesting
    /// a fresh timestamp before reading it). Does not mutate on-chain state.
    function sendHint(bytes32 topic, bytes calldata data) external view;

    /// @notice Executes multiple oracle calls in one transaction
    function multiCall(bytes[] calldata data)
        external returns (bytes[] memory results);
}
```

### Sequencer-Only — Write Methods

These methods can only be called by `MEGA_SYSTEM_ADDRESS` via [system transactions](system-tx.md).
Calls from any other address will revert.

```solidity
interface IOracle {
    /// @notice Writes a value to a specific storage slot
    function setSlot(uint256 slot, bytes32 value) external;

    /// @notice Writes values to multiple storage slots
    function setSlots(
        uint256[] calldata slots,
        bytes32[] calldata values
    ) external;

    /// @notice Emits an oracle log
    function emitLog(bytes32 topic, bytes calldata data) external;

    /// @notice Emits multiple oracle logs
    function emitLogs(bytes32 topic, bytes[] calldata dataVector) external;
}
```

## EVM-Level Behaviors

**Forced-cold SLOAD**: All SLOAD operations on the oracle contract use cold access gas cost (2,100 gas) regardless of EIP-2929 warm/cold tracking state.
This ensures deterministic gas costs during replay.

**`sendHint` interception**: When an oracle service wrapper calls `sendHint`, the EVM intercepts the call and forwards the hint to the sequencer's oracle backend before the call frame executes.
The sequencer uses this to prepare data (e.g., capture the current timestamp) so it is available when the transaction subsequently reads oracle storage.
The call then proceeds to on-chain execution normally (the Solidity function body runs as a no-op `view` function).

## Gas Detention Impact

Oracle access triggers [gas detention](../glossary.md#gas-detention).
An SLOAD from the oracle contract's storage caps remaining [compute gas](../glossary.md#compute-gas) at 20M.
This means transactions that read oracle data have a limited compute gas budget after the read.
Design your contracts accordingly — perform oracle reads as late as possible.

For the history of oracle detention triggers and cap values across specs, see the [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex3](../upgrades/rex3.md) upgrade pages.

## Oracle Services

The sequencer operates high-level oracle services using the central storage.
Each service is allocated a range of storage slots to avoid collision.
See [Oracle Services](../oracle-services/overview.md) for available services, including the [High-Precision Timestamp](../oracle-services/timestamp.md).
