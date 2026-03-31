---
description: MegaETH precompile gas overrides — KZG Point Evaluation and ModExp cost schedules diverging from standard Ethereum.
spec: Rex3
---

# Precompiles

This page specifies precompile behavior in MegaETH.
MegaETH inherits the standard precompile set from the underlying EVM baseline unless explicitly overridden on this page.

## Motivation

MegaETH also overrides the gas cost of selected precompiles to better match the actual computation they consume.
If a precompile is materially underpriced, an attacker can pack many such calls into a transaction or block and impose disproportionate computation on the sequencer.
The overrides on this page exist to reduce that denial-of-service risk by bringing charged gas closer to actual execution cost.

## Specification

A node MUST inherit the standard precompile set from the Optimism Isthmus / Ethereum Prague baseline except for the following MegaETH-specific overrides.

| Precompile | Address | MegaETH-Specific Behavior |
| ---------- | ------- | ------------------------- |
| KZG Point Evaluation | `0x0A` | Fixed gas cost of `KZG_POINT_EVALUATION_GAS_COST` |
| ModExp | `0x05` | Uses the Osaka / [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) pricing schedule |

For KZG Point Evaluation, if the supplied gas is less than `KZG_POINT_EVALUATION_GAS_COST`, the precompile MUST fail with `OutOfGas`.
Otherwise the node MUST charge exactly `KZG_POINT_EVALUATION_GAS_COST` gas for the precompile.

For ModExp, the node MUST use the Osaka / [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) pricing schedule instead of the earlier inherited pricing schedule.

All other precompiles MUST behave according to the inherited EVM baseline unless explicitly overridden elsewhere in this specification.

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `KZG_POINT_EVALUATION_GAS_COST` | 100,000 | Fixed gas cost for the KZG Point Evaluation precompile |

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the stable KZG Point Evaluation and ModExp overrides.
- [Rex](../upgrades/rex.md), [Rex1](../upgrades/rex1.md), [Rex2](../upgrades/rex2.md), and [Rex3](../upgrades/rex3.md) retain the same stable overrides.
