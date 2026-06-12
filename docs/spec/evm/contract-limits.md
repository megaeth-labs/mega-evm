---
description: MegaETH contract size limits — 512 KB max bytecode, 536 KB max initcode, inherited from MiniRex.
spec: Rex5
---

# Contract Limits

This page specifies MegaETH's limits on deployed contract bytecode size and initcode size.
It defines the contract-size limits inherited from [MiniRex](../upgrades/minirex.md).

## Motivation

Contract size and initcode size directly affect execution cost, state footprint, and validation overhead.
MegaETH raises these limits to accommodate larger deployments, but the protocol must still define explicit maximum values so all nodes reject oversized contracts consistently.

## Specification

A node MUST enforce the following limits:

| Limit                          | Value               |
| ------------------------------ | ------------------- |
| Maximum deployed contract size | `MAX_CONTRACT_SIZE` |
| Maximum initcode size          | `MAX_INITCODE_SIZE` |

If deployed runtime bytecode exceeds `MAX_CONTRACT_SIZE`, the node MUST reject the deployment.
If initcode exceeds `MAX_INITCODE_SIZE`, the node MUST reject the creation transaction or creation opcode execution.

The initcode limit is defined as:

`MAX_INITCODE_SIZE = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`

### Zero-Length Init Code in CREATE2

When a `CREATE2` opcode is executed with an init-code length of zero, a node MUST short-circuit after validating the salt operand: it MUST use the keccak-256 hash of the empty byte string as the resulting init-code hash, and MUST NOT perform any offset conversion, memory expansion, or hashing of memory.
Because the init-code length is zero, the init-code offset operand MUST be ignored entirely, even when it is a very large value.
This ensures that a zero-length `CREATE2` charges no memory-expansion gas (and no associated compute gas) for the unused offset operand and never halts with a spurious out-of-gas error caused by an out-of-range offset whose length is zero.

## Constants

| Constant                   | Value         | Description                                                         |
| -------------------------- | ------------- | ------------------------------------------------------------------- |
| `MAX_CONTRACT_SIZE`        | 524,288 bytes | Maximum size of deployed contract bytecode                          |
| `ADDITIONAL_INITCODE_SIZE` | 24,576 bytes  | Additional bytes allowed above the contract-size limit for initcode |
| `MAX_INITCODE_SIZE`        | 548,864 bytes | Maximum initcode size                                               |

## Rationale

**Why raise the contract limits?**
MegaETH allows substantially larger contracts than standard Ethereum.
The enlarged limits support deployment patterns that would otherwise exceed Ethereum's contract-size constraints.

## Security Considerations

This page has no security considerations.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the enlarged contract and initcode limits.
- [Rex](../upgrades/rex.md), [Rex1](../upgrades/rex1.md), [Rex2](../upgrades/rex2.md), [Rex3](../upgrades/rex3.md), and [Rex4](../upgrades/rex4.md) retain the same stable limits.
- [Rex5](../upgrades/rex5.md) short-circuits zero-length `CREATE2` after salt validation, using the empty-init-code hash without observing the init-code offset operand.
