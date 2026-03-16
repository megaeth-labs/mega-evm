# Oracle Service

## Overview

The MegaETH Oracle Service provides a trust-minimized mechanism for bringing off-chain data on-chain through a sequencer-operated oracle contract.
Smart contracts can access external information (price feeds, randomness, timestamps, etc.) without relying on third-party oracle providers.

> **Trust Assumption**: Using the built-in oracle service requires trusting the sequencer to publish accurate oracle data on-chain.

## Contract Details

**Address**: `0x6342000000000000000000000000000000000001`

**Key Properties**:
- **Simple storage model** — Direct access to storage slots via `uint256` keys
- **Restricted writes** — Only `MEGA_SYSTEM_ADDRESS` can write oracle data
- **Public reads** — Anyone can read oracle data

## Interface

```solidity
interface IOracle {
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
}
```

## Gas Detention Impact

Reading oracle storage triggers [gas detention](../evm/gas-detention.md):

| Spec    | Trigger                          | Compute Gas Cap |
| ------- | -------------------------------- | --------------- |
| MiniRex–Rex2 | CALL to oracle contract     | 1M              |
| Rex3+   | SLOAD from oracle storage        | 20M             |

This means transactions that read oracle data have a limited compute gas budget after the read.
Design your contracts accordingly — perform oracle reads as late as possible.

## Oracle Services

The sequencer may operate multiple high-level oracle services using the central storage.
Each service uses unique storage slots to avoid collision.
The specific services provided are determined by the sequencer and are outside the scope of the EVM specification.
