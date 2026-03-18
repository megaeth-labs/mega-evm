# Gas Detention

## Purpose

MegaETH's parallel EVM needs to minimize conflicts between concurrent transactions.
**Volatile data** — block environment fields, the beneficiary's account, and oracle storage — is frequently read by many transactions and is a major source of conflicts.

Gas detention restricts computation after volatile data is accessed by **capping the remaining compute gas**.
This forces transactions that touch volatile data to terminate quickly, reducing conflicts without banning the access outright.

Detained gas is effectively refunded — users only pay for actual computation performed.

## Volatile Data Categories

| Category                    | Trigger                                                | Cap (Rex3+)   |
| --------------------------- | ------------------------------------------------------ | ------------- |
| Block env / Beneficiary     | NUMBER, TIMESTAMP, COINBASE, etc. or beneficiary access | 20M           |
| Oracle                      | SLOAD from oracle contract storage                      | 20M           |

The **most restrictive cap wins** when multiple volatile sources are accessed.

### Rex4: Relative Gas Detention Cap

Starting from Rex4, gas detention uses **relative caps** instead of absolute caps.
The effective detained limit is `current_usage + cap` at the time of volatile access.
This means a transaction that has already consumed compute gas before accessing volatile data is not retroactively penalized — it can still use the full cap amount of compute gas after the access.

## How It Works

1. A transaction accesses volatile data (e.g., reads `TIMESTAMP`)
2. The access is recorded and the effective detained limit is calculated
3. After each volatile opcode, remaining compute gas is capped at the effective detained limit
4. Pre-Rex4 (absolute cap): if `compute_gas_used` already exceeds the cap, execution halts with `OutOfGas`
5. Rex4+ (relative cap): the effective limit is `current_usage + cap`, so the transaction can always perform at least `cap` more gas of computation after the access
6. Excess gas beyond the effective limit is "detained" and refunded at transaction end

## Example

```
Transaction starts with 200M compute gas budget

1. Normal computation uses 5M gas
2. Transaction reads TIMESTAMP → triggers block env detention
3. Compute gas is now capped at: 5M (used) + 20M (cap) = 25M effective limit
4. Transaction can perform at most 20M more gas of computation
5. At transaction end, the detained 175M gas is refunded
```

## Evolution Across Specs

| Spec        | Block Env / Beneficiary Cap | Oracle Cap | Oracle Trigger                   |
| ----------- | --------------------------- | ---------- | -------------------------------- |
| MiniRex     | 20M                         | 1M         | CALL to oracle contract          |
| Rex–Rex2    | 20M                         | 1M         | CALL to oracle contract          |
| Rex3        | 20M                         | 20M        | SLOAD from oracle storage        |
| Rex4        | 20M (relative)              | 20M (relative) | SLOAD from oracle storage    |

The shift from CALL-based to SLOAD-based oracle detention (Rex3) means simply calling the oracle without reading its storage no longer activates gas detention.
