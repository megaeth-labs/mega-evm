# Oracle Service

## Overview

The **MegaETH Oracle Service** provides a trust-minimized mechanism for bringing off-chain data on-chain through a sequencer-operated oracle contract. This service enables smart contracts to access external information (such as price feeds, randomness, timestamps, or other real-world data) without relying on third-party oracle providers.

> **⚠️ Trust Assumption**: Using the built-in oracle service requires trusting the sequencer to publish accurate oracle data on-chain. Users should understand this trust model before building applications that depend on oracle data.

## Architecture

### Oracle Contract

The oracle contract is a system contract deployed at a predefined address that provides a simple, gas-efficient interface for reading and writing arbitrary storage slots.
The oracle contract serves as a central generic storage of all different oracle services operated by the sequencer.

**Contract Address**: `0x6342000000000000000000000000000000000001`

**Key Properties**:

- **Simple Storage Model**: Direct access to storage slots via `uint256` keys
- **Restricted Writes**: Only [`MEGA_SYSTEM_ADDRESS`](./MEGA_SYSTEM_TRANSACTION.md) can write oracle data
- **Public Reads**: Anyone can read oracle data without restrictions

### Contract Interface

```solidity
interface IOracle {
    /// @notice Reads a value from a specific storage slot
    /// @param slot The storage slot to read from
    /// @return value The bytes32 value stored at the slot
    function getSlot(uint256 slot) external view returns (bytes32 value);

    /// @notice Writes a value to a specific storage slot
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS
    /// @param slot The storage slot to write to
    /// @param value The bytes32 value to store
    function setSlot(uint256 slot, bytes32 value) external;

    /// @notice Reads values from multiple storage slots
    /// @param slots Array of storage slots to read from
    /// @return values Array of bytes32 values at corresponding slots
    function getSlots(uint256[] calldata slots)
        external view returns (bytes32[] memory values);

    /// @notice Writes values to multiple storage slots
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS
    /// @param slots Array of storage slots to write to
    /// @param values Array of bytes32 values to store
    function setSlots(
        uint256[] calldata slots,
        bytes32[] calldata values
    ) external;
}
```

## Oracle Services

The sequencer may operate multiple high-level oracle services using the central on-chain storage provided by the oracle contract.
Each service must use unique storage slots in the oracle contract to avoid storage collision. However, this avoidance of collision is fully guaranteed by the sequencer itself.

The actual services provided by the sequencer may vary and are out of scope of the EVM specification.
