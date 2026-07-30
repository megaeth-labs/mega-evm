---
description: MegaETH contract size limits — 512 KB max bytecode, 536 KB max initcode, inherited from MiniRex.
spec: Rex6
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

### Creation-Opcode Halt Ordering

`CREATE2` computes its target address by expanding memory, copying the initcode, and hashing it before the inner opcode body runs, and both `CREATE` and `CREATE2` charge the contract-creation [storage gas](../glossary.md#storage-gas) for the derived address before that body runs.
Two rejections MUST fire ahead of all of that prework.

When the initcode length exceeds `MAX_INITCODE_SIZE`, a node MUST halt with `CreateInitCodeSizeLimit` before the memory expansion, the initcode copy, the `keccak256` hash, and the address derivation.
The check follows canonical operand ordering: the length operand is converted first, then the size check runs, then the offset operand is converted.

Inside a static call frame, any `CREATE` or `CREATE2` MUST halt with the static-call rejection before its operands are read, before the size check runs, before the deployment address is derived, and before the contract-creation storage gas is charged.
This matches canonical ordering, in which the static-context check precedes every other check.

Both rules change only the halt reason and its timing, not the outcome: every halt involved consumes all gas regardless of when it fires, so committed gas and committed state are identical either way.

Earlier specs run the prework first.
There, an oversized initcode surfaces whichever halt the prework reaches first — a memory out-of-gas, for instance — rather than the size-limit halt, and a creation opcode in a static frame surfaces a stack underflow, a memory out-of-gas, a storage-gas out-of-gas, or a fatal external error from the storage-pricing lookup instead of the static-call rejection.

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
- [Rex6](../upgrades/rex6.md) moved the oversized-initcode halt and the static-frame rejection ahead of the creation opcode's address-computation prework; through Rex5 the prework runs first, so those cases surface whichever halt it reaches first instead. Only the halt reason and its timing change.
