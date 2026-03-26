# High-Precision Timestamp

The High-Precision Timestamp oracle service provides **microsecond-resolution timestamps** to smart contracts, far exceeding the second-precision `block.timestamp` available in standard EVM.

**Available since**: [MiniRex](../hardfork-spec.md#mini_rex)

## Why Sub-Second Timestamps?

Standard EVM provides `block.timestamp` with second precision.
On Ethereum (12-second blocks), this is adequate for most use cases.
On MegaETH, with sub-millisecond block times, second-precision timestamps collapse many blocks into the same value, making them useless for time-sensitive logic.

The high-precision timestamp gives contracts access to the actual wall-clock time (in microseconds) at the moment of transaction execution.

### Use Cases

- High-frequency trading mechanisms
- Precise time-based rate limiting
- Accurate latency measurements
- Sub-second auction mechanics
- TWAP calculations requiring fine granularity

## How to Read the Timestamp

### Via the Wrapper Contract (Recommended)

A dedicated wrapper contract is deployed at `0x6342000000000000000000000000000000000002`.
It provides a typed Solidity interface:

```solidity
interface IHighPrecisionTimestamp {
    /// @notice Returns the current timestamp in microseconds since Unix epoch.
    function timestamp() external view returns (uint256);

    /// @notice Updates the timestamp for a given index.
    /// @dev Can only be called by MEGA_SYSTEM_ADDRESS.
    function update(uint256 index) external;

    /// @notice Returns the Oracle contract address used as the storage backend.
    function oracle() external view returns (address);

    /// @notice Returns the base storage slot in the Oracle contract.
    function baseSlot() external view returns (uint256);

    /// @notice Returns the number of allocated storage slots.
    function maxSlots() external view returns (uint32);
}
```

**Example usage:**

```solidity
IHighPrecisionTimestamp hpt = IHighPrecisionTimestamp(
    0x6342000000000000000000000000000000000002
);

// Returns microseconds since Unix epoch (e.g., 1700000000123456)
uint256 timestampUs = hpt.timestamp();

// Convert to seconds for comparison with block.timestamp
uint256 timestampSec = timestampUs / 1_000_000;
```

### Via Direct Oracle Storage

The timestamp is stored at **slot 0** of the [Oracle contract](../system-contracts/oracle.md) (`0x6342...0001`):

```solidity
IOracle oracle = IOracle(0x6342000000000000000000000000000000000001);

// Slot 0 = high-precision timestamp in microseconds
uint256 timestampUs = uint256(oracle.getSlot(0));
```

## Storage Layout

| Slot | Contents | Format |
| ---- | -------- | ------ |
| 0 | Timestamp | `uint256`, microseconds since Unix epoch |
| 1–7 | Reserved | Reserved for future use |

The service is allocated slots 0–7 in the Oracle contract, but currently only slot 0 is used.

## Timestamp Guarantees

- **Precision**: Microsecond (1/1,000,000 second).
- **Freshness**: Updated per transaction — each transaction sees a fresh timestamp reflecting the actual wall-clock time at execution.
- **Upper bound**: The timestamp is capped at `block.timestamp × 1,000,000`.
  It will never exceed the block timestamp converted to microseconds.
  This ensures the oracle timestamp is consistent with the block-level timestamp.
- **Lower bound**: The timestamp reflects the sequencer's wall-clock time at transaction execution.
  There is no protocol-enforced lower bound beyond monotonicity.
  In practice, the oracle timestamp is very close to real time because the sequencer captures it immediately before execution.
- **Monotonicity**: Within a block, successive transactions see non-decreasing timestamps.

{% hint style="info" %}
The timestamp is provided by the sequencer and requires trusting the sequencer's clock.
The `block.timestamp × 1,000,000` cap provides a coarse upper-bound sanity check.
{% endhint %}

## Gas Detention Impact

Reading the high-precision timestamp triggers [gas detention](../glossary.md#gas-detention) because the Oracle contract's storage is [volatile data](../glossary.md#volatile-data).
After reading the timestamp, your transaction's remaining compute gas is capped at 20M (Rex3+).

If your contract only needs to read the timestamp and perform minimal computation afterward, this is a non-issue.
If you need significant computation after reading the timestamp, consider reading it as late as possible in your transaction's execution flow.
