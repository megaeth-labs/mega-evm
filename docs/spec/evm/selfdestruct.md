---
description: SELFDESTRUCT opcode on MegaETH — EIP-6780 semantics, same-transaction destruction, and spec history from MiniRex to Rex2.
spec: Rex4
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

### State Growth Refund

When a contract that was created in the same transaction executes `SELFDESTRUCT` ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics), the node MUST apply a [state growth](resource-accounting.md#state-growth) refund:

- `-1` for the account itself (reversing the `+1` from `CREATE`/`CREATE2`).
- `-1` for each storage slot whose original value was zero and current value is non-zero (reversing each `+1` from `SSTORE`).

This refund MUST only be applied on the **first** effective destruction.
If the same account is the target of `SELFDESTRUCT` more than once in the same transaction, subsequent destructions MUST NOT produce additional refunds.

This refund MUST NOT be applied when `SELFDESTRUCT` targets a pre-existing account (one not created in the current transaction), because pre-existing accounts do not have their code and storage removed under EIP-6780.

The refund is frame-aware: if the call frame that performed the `SELFDESTRUCT` reverts, the refund MUST be discarded together with the destruction effect.

### MiniRex Behavior

For [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex1](../upgrades/rex1.md), `SELFDESTRUCT` MUST be disabled.
When disabled, executing `SELFDESTRUCT` MUST halt with `InvalidFEOpcode`.

## Constants

| Constant                     | Value             | Description                                                         |
| ---------------------------- | ----------------- | ------------------------------------------------------------------- |
| `SELFDESTRUCT_DISABLED_HALT` | `InvalidFEOpcode` | Halt reason when `SELFDESTRUCT` is disabled in MiniRex through Rex1 |

## Rationale

**Why disable SELFDESTRUCT before Rex2?**
MegaETH initially disabled `SELFDESTRUCT` to avoid inheriting destructive account-lifecycle behavior before the protocol defined the intended stable semantics.

**Why adopt EIP-6780 in stable behavior?**
EIP-6780 is the post-Cancun Ethereum behavior and provides a widely understood baseline.
Adopting it restores compatibility while avoiding legacy full-destruction behavior for long-lived contracts.

## Spec History

- [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex1](../upgrades/rex1.md) disable `SELFDESTRUCT`.
- [Rex2](../upgrades/rex2.md) re-enables `SELFDESTRUCT` with [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics.
- [Rex4](../upgrades/rex4.md) — added beneficiary-triggered volatile-access behavior for SELFDESTRUCT, and [state growth refund](#state-growth-refund) for same-transaction-created accounts destroyed by `SELFDESTRUCT`.
