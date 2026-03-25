# Oracle Services

Oracle services are sequencer-operated data feeds built on top of the [Oracle system contract](../system-contracts/oracle.md).
They provide real-time off-chain data to smart contracts through the oracle contract's storage slots.

## How Oracle Services Work

Oracle services are **not** system contracts themselves — they are high-level services that use the Oracle contract (`0x6342...0001`) as a shared storage layer.
Each service is allocated a range of storage slots in the Oracle contract and provides data by writing to those slots via [system transactions](../system-contracts/system-tx.md).

The key design property is **lazy evaluation**: oracle data is only published on-chain when a transaction actually reads it.

```
1. Transaction executes and reads an oracle storage slot (SLOAD)
2. The sequencer detects the access and captures the current value
3. A system transaction calling Oracle.setSlots() is generated
4. The system transaction is included in the block BEFORE the user transaction
5. Validators see consistent state: oracle write → user read
```

This means:
- If no transaction reads oracle data in a block, no oracle system transaction is generated.
- Only the slots that were actually accessed are published, minimizing on-chain storage updates.

{% hint style="warning" %}
**Trust assumption**: Oracle services are operated by the sequencer.
Using oracle data requires trusting the sequencer to provide accurate values.
{% endhint %}

## Available Services

| Service | Storage Slots | Since | Description |
| ------- | ------------- | ----- | ----------- |
| [High-Precision Timestamp](timestamp.md) | 0–7 | [MiniRex](../evm/spec-system.md#mini_rex) | Microsecond-resolution timestamps |

## Reading Oracle Data

Oracle services store data in the Oracle contract's storage.
You can read it by calling the Oracle contract directly:

```solidity
interface IOracle {
    function getSlot(uint256 slot) external view returns (bytes32 value);
    function getSlots(uint256[] calldata slots) external view returns (bytes32[] memory values);
}

IOracle oracle = IOracle(0x6342000000000000000000000000000000000001);
bytes32 value = oracle.getSlot(slot);
```

Some services also provide a dedicated wrapper contract with a typed interface (e.g., the [High-Precision Timestamp](timestamp.md) contract at `0x6342...0002`).

{% hint style="info" %}
Reading oracle data triggers [gas detention](../glossary.md#gas-detention) because it accesses [volatile data](../glossary.md#volatile-data).
The compute gas cap after an oracle read is 20M (Rex3+).
Design your contracts accordingly — keep post-oracle-read computation lightweight.
{% endhint %}
