# Gas Detention

## Purpose

MegaETH's parallel EVM needs to minimize conflicts between concurrent transactions.
**[Volatile data](../glossary.md#volatile-data)** — block environment fields, the [beneficiary's](../glossary.md#beneficiary) account, and oracle storage — is frequently read by many transactions and is a major source of conflicts.

Gas detention restricts computation after volatile data is accessed by **capping the remaining [compute gas](../glossary.md#compute-gas)**.
This forces transactions that touch volatile data to terminate quickly, reducing conflicts without banning the access outright.

Detained gas is effectively refunded — users only pay for actual computation performed.

## Volatile Data Categories

### Block Environment and Beneficiary — Cap: 20M

The following opcodes trigger block environment gas detention:

NUMBER, TIMESTAMP, COINBASE, DIFFICULTY/PREVRANDAO, GASLIMIT, BASEFEE, BLOCKHASH, BLOBBASEFEE, BLOBHASH.

Any operation that accesses the [beneficiary](../glossary.md#beneficiary) (block coinbase) account also triggers gas detention:

- `BALANCE` on the beneficiary address
- `SELFBALANCE` when the current contract is the beneficiary
- `EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH` on the beneficiary address
- Transaction sender is the beneficiary
- Transaction recipient (CALL target) is the beneficiary
- Accessing the beneficiary account via `DELEGATECALL`

### Oracle — Cap: 20M

SLOAD from the [oracle contract](../system-contracts/oracle.md) storage triggers oracle gas detention.
The trigger is SLOAD-based (not CALL-based): simply calling the oracle contract without reading its storage does not activate detention.
DELEGATECALL to the oracle contract does **not** trigger detention, because SLOAD in a DELEGATECALL context reads the caller's storage, not the oracle contract's storage.

{% hint style="success" %}
The 20M cap is in [compute gas](../glossary.md#compute-gas), which is identical to standard Ethereum gas.
For reference, a typical Uniswap V3 swap costs ~150K gas and a complex multi-hop aggregation ~500K gas on Ethereum mainnet.
20M compute gas is ample headroom for most contract interactions after a volatile data read.
{% endhint %}

Transactions from [`MEGA_SYSTEM_ADDRESS`](../system-contracts/system-tx.md) are exempt from oracle gas detention.

The **most restrictive cap wins** when multiple volatile sources are accessed.

<details>

<summary>Rex4 (unstable): Gas Detention Changes</summary>

`SELFDESTRUCT` targeting the beneficiary also triggers beneficiary gas detention.
Rex4 also changes detention from absolute caps to **relative caps** — the effective detained limit becomes `current_usage + cap` at the time of volatile access, so a transaction can always perform at least `cap` more gas of computation after the access.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

## How It Works

1. A transaction accesses volatile data (e.g., reads `TIMESTAMP`)
2. The access is recorded and the detained limit is set as an absolute cap on total compute gas
3. After each volatile opcode, remaining compute gas is capped at the detained limit
4. If `compute_gas_used` already exceeds the cap at the moment of access, execution halts immediately with `VolatileDataAccessOutOfGas`; otherwise remaining compute gas is capped to `cap - compute_gas_used`
5. Excess gas beyond the effective limit is "detained" and refunded at transaction end

## Example

```
Transaction starts with 200M compute gas budget

1. Normal computation uses 5M gas
2. Transaction reads TIMESTAMP → triggers block env detention (20M absolute cap)
3. Compute gas is now capped at 20M total
4. Transaction can perform at most 15M more gas of computation (20M cap - 5M used)
5. At transaction end, the detained 180M gas is refunded
```

## History

Gas detention caps and triggers have evolved across specs.
For the full history, see the [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex3](../upgrades/rex3.md) upgrade pages.
