---
description: SELFDESTRUCT opcode on MegaETH — EIP-6780 semantics, same-transaction destruction, and spec history from MiniRex to Rex2.
spec: Rex3
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

### MiniRex Behavior

For [MiniRex](../upgrades/minirex.md), [Rex](../upgrades/rex.md), and [Rex1](../upgrades/rex1.md), `SELFDESTRUCT` MUST be disabled.
When disabled, executing `SELFDESTRUCT` MUST halt with `InvalidFEOpcode`.

<details>
<summary>Rex4 (unstable): Beneficiary-triggered volatile access</summary>

In Rex4, `SELFDESTRUCT` targeting the [beneficiary](../glossary.md#beneficiary) MUST participate in volatile-data access handling.
That behavior is unstable and is not part of the current stable semantics.

</details>

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
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
- [Rex4](../upgrades/rex4.md) adds unstable beneficiary-related volatile-access behavior.
