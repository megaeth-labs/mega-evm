---
description: SELFDESTRUCT opcode on MegaETH — EIP-6780 semantics, same-transaction destruction, and spec history from MiniRex to Rex2.
spec: Rex5
---

# SELFDESTRUCT

This page specifies the availability and semantics of the `SELFDESTRUCT` opcode in MegaETH.
It defines the current behavior and records the earlier MiniRex restriction.

## Motivation

Ethereum deprecated the legacy `SELFDESTRUCT` behavior because it breaks assumptions about contract permanence and state growth, and it interacts poorly with modern state-management and witness-generation requirements.
MegaETH inherits the same motivation.

The protocol therefore needs to specify whether `SELFDESTRUCT` is disabled entirely or, when enabled, which restricted semantics apply.

## Specification

### Stable Behavior

`SELFDESTRUCT` MUST follow [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics.

If the executing contract was created in the same transaction, `SELFDESTRUCT` MUST:

- transfer the remaining balance to the target address, and
- remove the contract's code and storage.

If the executing contract was not created in the same transaction, `SELFDESTRUCT` MUST:

- transfer the remaining balance to the target address, and
- preserve the contract's code and storage.

`SELFDESTRUCT` targeting the [beneficiary](../glossary.md#beneficiary) MUST trigger beneficiary [gas detention](gas-detention.md).

### Beneficiary Account Creation

When `SELFDESTRUCT` transfers a non-zero balance to a target address that does not yet exist in state, the value transfer creates a new account.
A node MUST meter this account creation identically to account creation by any other means:

- charge the account-creation [storage gas](../glossary.md#storage-gas) (`ACCOUNT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)`, where `multiplier` is the target's [SALT bucket](../glossary.md#salt-bucket) multiplier), and
- record the creation against the [data size](resource-accounting.md#data-size) (`+SELFDESTRUCT_BENEFICIARY_DATA_BYTES`), [KV updates](resource-accounting.md#kv-updates) (`+1`), and [state growth](resource-accounting.md#state-growth) (`+1`) resource lanes.

A `SELFDESTRUCT` whose transferred balance is zero MUST NOT incur any of these charges, because a zero-value transfer does not create the target account.
A `SELFDESTRUCT` whose target already exists in state MUST NOT incur these charges.

### State Growth Refund

When a contract that was created in the same transaction executes `SELFDESTRUCT` ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics), the node MUST apply a [state growth](resource-accounting.md#state-growth) refund:

- `-1` for the account itself (reversing the `+1` from `CREATE`/`CREATE2`).
- `-1` for each storage slot whose original value was zero and current value is non-zero (reversing each `+1` from `SSTORE`).

This refund MUST only be applied on the **first** effective destruction.
If the same account is the target of `SELFDESTRUCT` more than once in the same transaction, subsequent destructions MUST NOT produce additional refunds.

This refund MUST NOT be applied when `SELFDESTRUCT` targets a pre-existing account (one not created in the current transaction), because pre-existing accounts do not have their code and storage removed under EIP-6780.

The refund is frame-aware: if the call frame that performed the `SELFDESTRUCT` reverts, the refund MUST be discarded together with the destruction effect.

## Constants

| Constant                              | Value  | Description                                                                        |
| ------------------------------------- | ------ | ---------------------------------------------------------------------------------- |
| `ACCOUNT_CREATION_STORAGE_GAS_BASE`   | 25,000 | Base storage gas charged when a value transfer creates a previously empty account  |
| `SELFDESTRUCT_BENEFICIARY_DATA_BYTES` | 40     | Data-size bytes recorded for the beneficiary account write on beneficiary creation |

## Rationale

**Why disable SELFDESTRUCT before Rex2?**
MegaETH initially disabled `SELFDESTRUCT` to avoid inheriting destructive account-lifecycle behavior before the protocol defined the intended stable semantics.

**Why adopt EIP-6780 in stable behavior?**
EIP-6780 is the post-Cancun Ethereum behavior and provides a widely understood baseline.
Adopting it restores compatibility while avoiding legacy full-destruction behavior for long-lived contracts.

**Why meter beneficiary account creation?**
A value-carrying `SELFDESTRUCT` to a non-existent target creates an account exactly as a value-transferring `CALL` to an empty address does.
Charging the same account-creation storage gas and recording the same resource-lane usage closes a path by which state could be grown at compute-gas cost only, without going through the metered account-creation surcharge.

## Security Considerations

**If `SELFDESTRUCT` targeting the [beneficiary](../glossary.md#beneficiary) does not trigger gas detention**, contracts can use it to access beneficiary balance without being detained, creating an unmitigated conflict hotspot for parallel execution.

**If `SELFDESTRUCT` does not charge new-account costs when it creates its target**, an attacker can create accounts and grow state through `SELFDESTRUCT` at compute-gas cost only, bypassing the account-creation storage gas and resource-lane accounting that every other account-creation path pays.

## Spec History

- [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex1](../upgrades/rex1.md) disable `SELFDESTRUCT`; executing it halts with `InvalidFEOpcode`.
- [Rex2](../upgrades/rex2.md) re-enables `SELFDESTRUCT` with [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics.
- [Rex4](../upgrades/rex4.md) — added beneficiary-triggered volatile-access behavior for SELFDESTRUCT, and [state growth refund](#state-growth-refund) for same-transaction-created accounts destroyed by `SELFDESTRUCT`.
- [Rex5](../upgrades/rex5.md) — charged account-creation storage gas and recorded data-size, KV-update, and state-growth usage when a value-carrying `SELFDESTRUCT` creates a previously non-existent beneficiary account.
