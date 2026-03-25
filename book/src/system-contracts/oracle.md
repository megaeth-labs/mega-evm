# Oracle Contract

## Overview

The Oracle contract is the centralized storage backend for MegaEVM's [oracle services](../oracle-services/README.md).
It provides a simple key-value store where the sequencer writes off-chain data (timestamps, price feeds, etc.) via [system transactions](system-tx.md), and oracle service wrapper contracts read from it.

{% hint style="info" %}
**For contract developers**: You typically do not interact with the Oracle contract directly.
Use the higher-level [oracle services](../oracle-services/README.md) instead — they provide typed interfaces and dedicated wrapper contracts (e.g., [High-Precision Timestamp](../oracle-services/timestamp.md) at `0x6342...0002`).
{% endhint %}

{% hint style="warning" %}
**Trust Assumption**: Oracle data is published by the sequencer.
Using oracle services requires trusting the sequencer to provide accurate values.
{% endhint %}

## Contract Details

**Address**: `0x6342000000000000000000000000000000000001`

**Key Properties**:
- **Simple storage model** — Direct access to storage slots via `uint256` keys
- **Restricted writes** — Only `MEGA_SYSTEM_ADDRESS` can write oracle data
- **Public reads** — Anyone can read oracle data
- **Versioned bytecode** — Pre-[Rex2](../evm/spec-system.md#rex2) deploys Oracle v1.0.0, and Rex2+ deploys Oracle v1.1.0 with `sendHint`

## Interface

```solidity
interface IOracle {
    /// @notice Executes multiple oracle calls in one transaction
    function multiCall(bytes[] calldata data)
        external returns (bytes[] memory results);

    /// @notice Reads a value from a specific storage slot
    function getSlot(uint256 slot) external view returns (bytes32 value);

    /// @notice Writes a value to a specific storage slot
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS
    function setSlot(uint256 slot, bytes32 value) external;

    /// @notice Reads values from multiple storage slots
    function getSlots(uint256[] calldata slots)
        external view returns (bytes32[] memory values);

    /// @notice Writes values to multiple storage slots
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS
    function setSlots(
        uint256[] calldata slots,
        bytes32[] calldata values
    ) external;

    /// @notice Sends a hint to the oracle backend
    /// @dev Available from Rex2 onward.
    /// Declared `view` because it does not mutate on-chain state;
    /// the hint is processed by the sequencer outside the EVM.
    function sendHint(bytes32 topic, bytes calldata data) external view;

    /// @notice Emits an oracle log
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS
    function emitLog(bytes32 topic, bytes calldata data) external;

    /// @notice Emits multiple oracle logs
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS
    function emitLogs(bytes32 topic, bytes[] calldata dataVector) external;
}
```

## EVM-Level Behaviors

**Forced-cold SLOAD**: All SLOAD operations on the oracle contract use cold access gas cost (2,100 gas) regardless of EIP-2929 warm/cold tracking state.
This ensures deterministic gas costs during replay.

**`sendHint` interception**: `sendHint` is intercepted at the EVM level before call frame execution — the hint is forwarded to the oracle backend via the external oracle environment.
The call still proceeds to on-chain execution after interception.

## Gas Detention Impact

Oracle access triggers [gas detention](../glossary.md#gas-detention).
An SLOAD from the oracle contract's storage caps remaining [compute gas](../glossary.md#compute-gas) at 20M.
This means transactions that read oracle data have a limited compute gas budget after the read.
Design your contracts accordingly — perform oracle reads as late as possible.

For the history of oracle detention triggers and cap values across specs, see the [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex3](../upgrades/rex3.md) upgrade pages.

## Oracle Services

The sequencer operates high-level oracle services using the central storage.
Each service is allocated a range of storage slots to avoid collision.
See [Oracle Services](../oracle-services/README.md) for available services, including the [High-Precision Timestamp](../oracle-services/timestamp.md).
