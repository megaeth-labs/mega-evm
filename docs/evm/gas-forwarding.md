---
description: MegaETH 98/100 gas forwarding rule — CALL, DELEGATECALL, STATICCALL, CALLCODE, and CREATE/CREATE2 forwarding semantics.
spec: Rex3
---

# Gas Forwarding

This page specifies how MegaETH forwards gas into child call frames and contract-creation call frames.
It defines the stable 98/100 forwarding rule and its relation to value-transfer stipends.

## Motivation

The standard 63/64 forwarding rule was introduced to mitigate call-depth attacks by ensuring that each nested call retains less gas than its parent.
MegaETH is designed to support block gas limits up to 10 billion gas.
At that scale, the inherited 63/64 rule again leaves enough gas available at deep call depth to reintroduce the attack it was meant to mitigate.

MegaETH therefore replaces the inherited forwarding fraction with a stricter rule.

## Specification

### Stable Forwarding Rule

A node MUST cap gas forwarded by CALL-like opcodes and contract-creation opcodes to 98/100 of the parent's remaining gas.

The stable forwarding rule applies to:

- `CALL`,
- `CALLCODE`,
- `DELEGATECALL`,
- `STATICCALL`,
- `CREATE`,
- `CREATE2`.

The forwarding cap is:

`forwarded_gas_cap = parent_remaining_gas - parent_remaining_gas × 2 / 100`

The child call frame gas limit MUST be the minimum of:

- the gas requested by the caller, and
- `forwarded_gas_cap`.

### Value Transfer Stipend

For `CALL` and `CALLCODE` with non-zero value transfer, the standard EVM `CALL_STIPEND` MUST be preserved.
The forwarding cap applies to the forwarded gas portion, not to the stipend itself.

The child call frame gas for value-transferring `CALL` and `CALLCODE` MUST be:

`child_gas = min(requested_forwarded_gas, forwarded_gas_cap) + CALL_STIPEND`

For `DELEGATECALL`, `STATICCALL`, `CREATE`, and `CREATE2`, no call stipend applies.

### Opcode Scope by Spec Version

For [MiniRex](../upgrades/minirex.md), the 98/100 rule applied only to `CALL`, `CREATE`, and `CREATE2`.
For [Rex](../upgrades/rex.md) and later stable specs, the 98/100 rule applies to all CALL-like opcodes and both contract-creation opcodes.

<details>
<summary>Rex4 (unstable): Storage gas stipend interaction</summary>

For value-transferring `CALL` and `CALLCODE`, Rex4 adds `STORAGE_GAS_STIPEND` on top of the stable child-frame gas limit.
The extra stipend MUST be usable only for storage-gas-heavy operations.
The compute-gas limit of the child call frame MUST remain at the pre-stipend level.

</details>

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `GAS_FORWARD_NUMERATOR` | 98 | Numerator of the stable forwarding fraction |
| `GAS_FORWARD_DENOMINATOR` | 100 | Denominator of the stable forwarding fraction |
| `CALL_STIPEND` | 2,300 | Standard EVM stipend preserved for value-transferring `CALL` and `CALLCODE` |
| `STORAGE_GAS_STIPEND` | 23,000 | Additional unstable Rex4 stipend for storage-gas operations in value-transferring `CALL` and `CALLCODE` |

## Rationale

**Why 98/100 instead of 63/64?**
The inherited 63/64 rule was designed to mitigate call-depth attacks by ensuring that each nested call retains less gas than its parent.
MegaETH is intended to support block gas limits up to 10 billion gas, which makes the inherited 63/64 reduction insufficient to suppress that attack pattern at deep call depth.
Retaining 2% instead of approximately 1.56% reduces residual gas more aggressively and restores the protective intent of gas-based call-depth mitigation under MegaETH's higher gas regime.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the 98/100 rule for `CALL`, `CREATE`, and `CREATE2` only.
- [Rex](../upgrades/rex.md) extended the rule to all CALL-like opcodes.
- [Rex4](../upgrades/rex4.md) adds an unstable storage-gas stipend for value-transferring `CALL` and `CALLCODE`.
