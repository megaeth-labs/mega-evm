# Gas Detention

## Purpose

MegaETH's parallel EVM needs to minimize conflicts between concurrent transactions.
**[Volatile data](../glossary.md#volatile-data)** — block environment fields, the [beneficiary's](../glossary.md#beneficiary) account, and oracle storage — is frequently read by many transactions and is a major source of conflicts.

Gas detention restricts computation after volatile data is accessed by **capping the remaining [compute gas](../glossary.md#compute-gas)**.
This forces transactions that touch volatile data to terminate quickly, reducing conflicts without banning the access outright.

Detained gas is effectively refunded — users only pay for actual computation performed.

## Volatile Data Categories

| Category                    | Trigger                                                | Cap           |
| --------------------------- | ------------------------------------------------------ | ------------- |
| Block env / Beneficiary     | NUMBER, TIMESTAMP, COINBASE, DIFFICULTY/PREVRANDAO, GASLIMIT, BASEFEE, BLOCKHASH, BLOBBASEFEE, BLOBHASH, or beneficiary access | 20M           |
| Oracle                      | SLOAD from oracle contract storage                      | 20M           |

Transactions from `MEGA_SYSTEM_ADDRESS` are exempt from oracle gas detention.

The **most restrictive cap wins** when multiple volatile sources are accessed.

{% hint style="info" %}
**Rex4 (unstable)**: `SELFDESTRUCT` targeting the beneficiary also triggers beneficiary gas detention.
Rex4 also changes detention from absolute caps to **relative caps** — the effective detained limit becomes `current_usage + cap` at the time of volatile access, so a transaction can always perform at least `cap` more gas of computation after the access.
See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.
{% endhint %}

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
