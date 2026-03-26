# Oracle Services

Oracle services are sequencer-operated data feeds built on top of the [Oracle system contract](../system-contracts/oracle.md).
They provide real-time off-chain data to smart contracts through the oracle contract's storage slots.

## How Oracle Services Work

Oracle services are **not** system contracts themselves — they are high-level services that use the Oracle contract (`0x6342...0001`) as a shared storage layer.
Each service is allocated a range of storage slots in the Oracle contract.

### Real-Time Data Feeding

The sequencer maintains an **oracle service manager** that coordinates all oracle services.
For each user transaction, the sequencer feeds real-time data through the following lifecycle:

1. **Open window** — Before executing a transaction, the sequencer opens an oracle window.
   Each oracle service takes a **snapshot** of its current data at this moment.
   For example, the timestamp oracle captures the current wall-clock time in microseconds (capped at `block.timestamp × 1,000,000`).
   Oracle data remains stable within the window — the same transaction always sees the same values.

2. **Transaction executes** — When the EVM encounters an SLOAD on the oracle contract's address, it calls into the oracle service manager instead of reading on-chain storage.
   The manager resolves the slot from the snapshot and **records that the slot was accessed**.

3. **Close window** — After the transaction finishes, the sequencer closes the window and collects all accessed slots with their snapshot values.

4. **Generate system transaction** — If any oracle slots were accessed, the sequencer generates a signed [system transaction](../system-contracts/system-tx.md) that calls `Oracle.setSlots()` to write those values to on-chain storage.

5. **Block ordering** — The system transaction is placed **before** the user transaction in the final block.
   This way, validators and replayers see the oracle write first, then the user read — producing consistent state without needing the oracle service manager.

### Lazy Evaluation

The key design property is that oracle data is only published on-chain when a transaction actually reads it:

- If no transaction reads oracle data in a block, no oracle system transaction is generated.
- Only the slots that were actually accessed are published, minimizing on-chain storage updates.
- Each transaction gets its own oracle window with a fresh snapshot, so successive transactions can see updated values (e.g., increasing timestamps).

{% hint style="info" %}
**Trust assumption**: Oracle services are operated by the sequencer.
Using oracle data requires trusting the sequencer to provide accurate values.
{% endhint %}

## Available Services

| Service | Storage Slots | Since | Description |
| ------- | ------------- | ----- | ----------- |
| [High-Precision Timestamp](timestamp.md) | 0–7 | [MiniRex](../hardfork-spec.md#mini_rex) | Microsecond-resolution timestamps |

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

{% hint style="danger" %}
Reading oracle data triggers [gas detention](../glossary.md#gas-detention) because it accesses [volatile data](../glossary.md#volatile-data).
The compute gas cap after an oracle read is 20M (Rex3+).
Design your contracts accordingly — keep post-oracle-read computation lightweight.
{% endhint %}
