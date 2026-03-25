# Oracle Service

## Overview

The MegaETH Oracle Service provides a trust-minimized mechanism for bringing off-chain data on-chain through a sequencer-operated oracle contract.
Smart contracts can access external information (price feeds, randomness, timestamps, etc.) without relying on third-party oracle providers.

{% hint style="warning" %}
**Trust Assumption**: Using the built-in oracle service requires trusting the sequencer to publish accurate oracle data on-chain.
{% endhint %}

## Contract Details

**Address**: `0x6342000000000000000000000000000000000001`

**Key Properties**:
- **Simple storage model** — Direct access to storage slots via `uint256` keys
- **Restricted writes** — Only `MEGA_SYSTEM_ADDRESS` can write oracle data
- **Public reads** — Anyone can read oracle data
- **Versioned bytecode** — Pre-Rex2 deploys Oracle v1.0.0, and Rex2+ deploys Oracle v1.1.0 with `sendHint`

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
    /// @dev Available from Rex2 onward
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

**`sendHint` interception**: `sendHint` is intercepted at the EVM level before frame execution — the hint is forwarded to the oracle backend via the external oracle environment.
The call still proceeds to on-chain execution after interception.

## Gas Detention Impact

Oracle access can trigger [gas detention](../evm/gas-detention.md), but the trigger changes by spec:

| Spec         | Trigger                          | Compute Gas Cap |
| ------------ | -------------------------------- | --------------- |
| MiniRex      | CALL to oracle contract                | 1M              |
| Rex–Rex2     | CALL or STATICCALL to oracle contract  | 1M              |
| Rex3+        | SLOAD from oracle storage        | 20M             |

This means transactions that read oracle data have a limited compute gas budget after the read.
Design your contracts accordingly — perform oracle reads as late as possible.

## Oracle Services

The sequencer may operate multiple high-level oracle services using the central storage.
Each service uses unique storage slots to avoid collision.
The specific services provided are determined by the sequencer and are outside the scope of the EVM specification.
