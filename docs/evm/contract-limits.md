---
description: MegaETH contract size limits — 512 KB max bytecode, 1 MB max initcode, inherited from MiniRex.
spec: Rex3
---

# Contract Limits

This page specifies MegaETH's limits on deployed contract bytecode size and initcode size.
It defines the contract-size limits inherited from [MiniRex](../upgrades/minirex.md).

## Motivation

Contract size and initcode size directly affect execution cost, state footprint, and validation overhead.
MegaETH raises these limits to accommodate larger deployments, but the protocol must still define explicit maximum values so all nodes reject oversized contracts consistently.

## Specification

A node MUST enforce the following limits:

| Limit | Value |
| ----- | ----- |
| Maximum deployed contract size | `MAX_CONTRACT_SIZE` |
| Maximum initcode size | `MAX_INITCODE_SIZE` |

If deployed runtime bytecode exceeds `MAX_CONTRACT_SIZE`, the node MUST reject the deployment.
If initcode exceeds `MAX_INITCODE_SIZE`, the node MUST reject the creation transaction or creation opcode execution.

The initcode limit is defined as:

`MAX_INITCODE_SIZE = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `MAX_CONTRACT_SIZE` | 524,288 bytes | Maximum size of deployed contract bytecode |
| `ADDITIONAL_INITCODE_SIZE` | 24,576 bytes | Additional bytes allowed above the contract-size limit for initcode |
| `MAX_INITCODE_SIZE` | 548,864 bytes | Maximum initcode size |

## Rationale

**Why raise the contract limits?**
MegaETH allows substantially larger contracts than standard Ethereum.
The enlarged limits support deployment patterns that would otherwise exceed Ethereum's contract-size constraints.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the enlarged contract and initcode limits.
- [Rex](../upgrades/rex.md), [Rex1](../upgrades/rex1.md), [Rex2](../upgrades/rex2.md), and [Rex3](../upgrades/rex3.md) retain the same stable limits.
