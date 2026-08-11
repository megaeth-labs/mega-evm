---
description: SELFDESTRUCT opcode on MegaETH — EIP-6780 semantics, same-transaction destruction, beneficiary account metering, and spec history from MiniRex to Rex6.
spec: Rex6
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

A `SELFDESTRUCT` whose **executing contract** is the beneficiary reads and zeroes that contract's balance, so it observes beneficiary state regardless of its stack target.
While volatile data access is disabled, such a `SELFDESTRUCT` MUST revert with `VolatileDataAccessDisabled`, as specified in [MegaAccessControl](../system-contracts/mega-access-control.md#disablevolatiledataaccess).
This is a disabled-state guard only and MUST NOT be read as an additional detention trigger; [Gas Detention](gas-detention.md#beneficiary-access) is the complete list of triggers.

### Beneficiary Account Creation

When `SELFDESTRUCT` transfers a non-zero balance to a target address that does not yet exist in state, the value transfer creates a new account.
A node MUST meter this account creation identically to account creation by any other means:

- charge the account-creation [storage gas](../glossary.md#storage-gas) (`ACCOUNT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)`, where `multiplier` is the target's [SALT bucket](../glossary.md#salt-bucket) multiplier), and
- record the creation against the [data size](resource-accounting.md#data-size) (`+ACCOUNT_UPDATE_DATA_SIZE`), [KV updates](resource-accounting.md#kv-updates) (`+1`), and [state growth](resource-accounting.md#state-growth) (`+1`) resource lanes.

A `SELFDESTRUCT` whose transferred balance is zero MUST NOT incur any of these charges, because a zero-value transfer does not create the target account.

### Beneficiary Balance Credit to an Existing Account

When `SELFDESTRUCT` transfers a non-zero balance to a target that already exists in state and is **distinct** from the executing contract, no account is created, but the target's balance is written.
A node MUST record that write against the [data size](resource-accounting.md#data-size) (`+ACCOUNT_UPDATE_DATA_SIZE`) and [KV updates](resource-accounting.md#kv-updates) (`+1`) resource lanes.
A node MUST NOT charge account-creation storage gas and MUST NOT record state growth for this case — the account already exists.

Two cases record nothing:

- a `SELFDESTRUCT` whose transferred balance is zero, which performs no balance credit; and
- a `SELFDESTRUCT` whose target is the executing contract itself, which credits no other account. Under [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) this is a balance no-op for a contract not created in the current transaction, and burns the balance for one that was; neither writes a distinct target account.

### State Growth Refund

When a contract that was created in the same transaction executes `SELFDESTRUCT` ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics), the node MUST apply a [state growth](resource-accounting.md#state-growth) refund:

- `-1` for the account itself (reversing the `+1` from `CREATE`/`CREATE2`).
- `-1` for each storage slot whose original value was zero and current value is non-zero (reversing each `+1` from `SSTORE`).

This refund MUST only be applied on the **first** effective destruction.
If the same account is the target of `SELFDESTRUCT` more than once in the same transaction, subsequent destructions MUST NOT produce additional refunds.

This refund MUST NOT be applied when `SELFDESTRUCT` targets a pre-existing account (one not created in the current transaction), because pre-existing accounts do not have their code and storage removed under EIP-6780.

The refund is frame-aware: if the call frame that performed the `SELFDESTRUCT` reverts, the refund MUST be discarded together with the destruction effect.

## Constants

| Constant                            | Value  | Description                                                                                                    |
| ----------------------------------- | ------ | -------------------------------------------------------------------------------------------------------------- |
| `ACCOUNT_CREATION_STORAGE_GAS_BASE` | 25,000 | Base storage gas charged when a value transfer creates a previously empty account                              |
| `ACCOUNT_UPDATE_DATA_SIZE`          | 40     | Data-size bytes recorded for a target account write, whether it creates the account or credits an existing one |

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

- [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex1](../upgrades/rex1.md) disable `SELFDESTRUCT`; executing it halts the frame and consumes all of its remaining gas.
- [Rex2](../upgrades/rex2.md) re-enables `SELFDESTRUCT` with [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics.
- [Rex4](../upgrades/rex4.md) — added beneficiary-triggered volatile-access behavior for SELFDESTRUCT, and [state growth refund](#state-growth-refund) for same-transaction-created accounts destroyed by `SELFDESTRUCT`.
- [Rex5](../upgrades/rex5.md) — charged account-creation storage gas and recorded data-size, KV-update, and state-growth usage when a value-carrying `SELFDESTRUCT` creates a previously non-existent beneficiary account.
- [Rex6](../upgrades/rex6.md) — recorded the data-size and KV-update cost of a non-zero balance credit to an **existing** distinct target, which through Rex5 was metered as nothing at all; extended the `disableVolatileDataAccess` guard to a `SELFDESTRUCT` whose executing contract is the beneficiary, not only one whose stack target is.
