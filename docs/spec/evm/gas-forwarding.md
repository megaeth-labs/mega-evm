---
description: MegaETH 98/100 gas forwarding rule — CALL, DELEGATECALL, STATICCALL, CALLCODE, and CREATE/CREATE2 forwarding semantics.
spec: Rex5
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

<details>
<summary>Rex6 (unstable): forwarded gas returned on a compute-gas-limit halt</summary>

When a `CALL`-family or `CREATE` / `CREATE2` opcode records its compute gas after its body and the recording exceeds the [compute gas limit](resource-limits.md), the opcode halts and its pending child frame is discarded before the child runs.

Pre-Rex6, the gas already forwarded to that discarded child is not returned to the parent frame, so the transaction's `gas_used` is inflated by the forwarded amount even though the child never executed.

Under Rex6, the node MUST return the forwarded gas to the parent frame before halting, so `gas_used` reflects only the gas actually consumed.

</details>

### Value Transfer Stipend

For `CALL` and `CALLCODE` with non-zero value transfer, the standard EVM `CALL_STIPEND` MUST be preserved.
The forwarding cap applies to the forwarded gas portion, not to the stipend itself.

The child call frame gas for value-transferring `CALL` and `CALLCODE` MUST be:

`child_gas = min(requested_forwarded_gas, forwarded_gas_cap) + CALL_STIPEND`

For `DELEGATECALL`, `STATICCALL`, `CREATE`, and `CREATE2`, no call stipend applies.

### [Storage Gas Stipend](../glossary.md#storage-gas-stipend) Interaction

For internal (call depth greater than zero) value-transferring `CALL` and `CALLCODE`, the callee frame receives a `STORAGE_CALL_STIPEND` allowance.
This allowance MUST NOT inflate the callee's `gas_limit`: the child-frame gas limit is exactly `min(requested_forwarded_gas, forwarded_gas_cap) + CALL_STIPEND`, with no `STORAGE_CALL_STIPEND` term.

The allowance is a per-frame budget reserved exclusively for the storage-gas surcharges that MegaETH adds on top of standard EVM opcode costs.
A node MUST apply the allowance only to the following storage-gas surcharge sites incurred within the callee frame:

- empty-account creation via value-transferring `CALL` / `CALLCODE`,
- contract creation via `CREATE` / `CREATE2`,
- the first-time zero-to-non-zero `SSTORE` write,
- `LOG` topic and data storage gas, and
- empty-beneficiary creation via `SELFDESTRUCT`.

At each such site, a node MUST draw up to `STORAGE_CALL_STIPEND` from the frame's remaining allowance and charge only the residual surcharge (the surcharge minus the amount drawn) against the frame's gas.

Because the allowance never enters the frame's gas limit, it MUST NOT be spendable on compute (standard EVM opcode) gas.
The allowance does not apply to standard EVM opcode costs: a callee whose forwarded gas plus `CALL_STIPEND` does not cover the standard EVM cost of an opcode MUST run out of gas normally regardless of the remaining allowance.
Any portion of the allowance not drawn by a surcharge site is not returned to the caller; the allowance never contributes to the gas a frame returns to its parent.

The allowance applies to internal value-transferring `CALL` and `CALLCODE` only.
Top-level transactions, `DELEGATECALL`, `STATICCALL`, `CREATE`, `CREATE2`, and any value-zero call MUST NOT receive the allowance.

## Constants

| Constant                  | Value  | Description                                                                           |
| ------------------------- | ------ | ------------------------------------------------------------------------------------- |
| `GAS_FORWARD_NUMERATOR`   | 98     | Numerator of the stable forwarding fraction                                           |
| `GAS_FORWARD_DENOMINATOR` | 100    | Denominator of the stable forwarding fraction                                         |
| `CALL_STIPEND`            | 2,300  | Standard EVM stipend preserved for value-transferring `CALL` and `CALLCODE`           |
| `STORAGE_CALL_STIPEND`    | 23,000 | Per-frame storage-gas allowance for internal value-transferring `CALL` and `CALLCODE` |

## Rationale

**Why 98/100 instead of 63/64?**
The inherited 63/64 rule was designed to mitigate call-depth attacks by ensuring that each nested call retains less gas than its parent.
MegaETH is intended to support block gas limits up to 10 billion gas, which makes the inherited 63/64 reduction insufficient to suppress that attack pattern at deep call depth.
Retaining 2% instead of approximately 1.56% reduces residual gas more aggressively and restores the protective intent of gas-based call-depth mitigation under MegaETH's higher gas regime.

**Why make the storage gas stipend a per-frame allowance instead of inflating the child gas limit?**
The stipend exists so a value-transferring internal call can pay MegaETH's storage-gas surcharges even when the caller forwards little or no gas.
Adding the stipend to the child's gas limit, Rex4 left it spendable on compute opcodes; the per-frame compute-gas cap that fenced it off was enforced only after each opcode completed, so a single expensive opcode could record its full compute cost into the parent's compute-gas counter before the cap triggered, and repeated value-transferring calls could amplify recorded compute gas beyond the transaction's compute-gas limit.
Keeping the stipend as an allowance that never enters the child's gas limit makes it structurally unspendable on compute, removing the amplification path without changing the grant amount or which calls qualify.

## Security Considerations

**If more than 98/100 of gas is forwarded**, MegaETH's high block gas limits (up to 10 billion) mean enough residual gas survives at deep call depth to sustain a call-depth denial-of-service attack.
The 63/64 rule inherited from Ethereum was designed for much lower gas budgets; at 10B gas, 98/100 is needed to achieve the same protective effect.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the 98/100 rule for `CALL`, `CREATE`, and `CREATE2` only.
- [Rex](../upgrades/rex.md) extended the rule to all CALL-like opcodes.
- [Rex4](../upgrades/rex4.md) — added storage-gas stipend for value-transferring `CALL` and `CALLCODE`.
- [Rex5](../upgrades/rex5.md) — recast the storage-gas stipend as a per-frame allowance that no longer inflates the child's gas limit (so it cannot be spent on compute), and corrected the parent's compute-gas attribution to exclude the `CALL_STIPEND` for value-transferring `CALL` / `CALLCODE`; the 23,000 grant amount and admission conditions are unchanged.
- [Rex6](../upgrades/rex6.md) (**unstable**) — returns forwarded gas to the parent when a `CALL` or `CREATE` halts on the compute-gas limit: pre-Rex6, when the opcode recorded its compute gas and exceeded the limit, the pending child frame was discarded but the gas already forwarded to it was not returned, inflating `gas_used`; Rex6 returns the forwarded gas to the parent before halting.
