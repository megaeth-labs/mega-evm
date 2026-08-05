---
description: MegaETH precompile gas overrides — KZG Point Evaluation and ModExp cost schedules diverging from standard Ethereum.
spec: Rex6
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
A node MUST preserve these MegaETH-specific overrides whenever it refreshes or reselects its internal precompile table.
If multiple MegaETH specs share the same inherited upstream baseline, an implementation MUST NOT reconstruct the MegaETH precompile table from the upstream baseline identifier alone.

| Precompile           | Address | MegaETH-Specific Behavior                                                             |
| -------------------- | ------- | ------------------------------------------------------------------------------------- |
| KZG Point Evaluation | `0x0A`  | Fixed gas cost of `KZG_POINT_EVALUATION_GAS_COST`                                     |
| ModExp               | `0x05`  | Uses the Osaka / [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) pricing schedule |

For KZG Point Evaluation, if the supplied gas is less than `KZG_POINT_EVALUATION_GAS_COST`, the precompile MUST fail with `OutOfGas`.
Otherwise the precompile runs, and on a successful return the node MUST charge exactly `KZG_POINT_EVALUATION_GAS_COST` gas for it.

The fixed charge applies to a successful return only.
If the precompile runs and then fails — for example on malformed input — the inherited EVM's precompile-failure semantics apply unchanged, and the caller loses the entire gas limit it forwarded rather than the fixed cost.
The compute gas recorded for that failure is a separate amount; see [Compute Gas Accounting](compute-gas.md#precompiles).

For ModExp, the node MUST use the Osaka / [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) pricing schedule instead of the earlier inherited pricing schedule.

One input shape deviates from the EIP-7883 formula: a call whose header declares `base_length = 0` and `modulus_length = 0` MUST be charged the flat 500-gas minimum, regardless of the declared exponent length.
This preserves the behavior of the implementation the schedule was adopted through, which returned the minimum before computing the formula cost; EIP-7883 as written has no such special case, and its multiplication complexity never falls below 16, so the formula prices these inputs above the minimum whenever the exponent length is large.
The [EIP-7823](https://eips.ethereum.org/EIPS/eip-7823) input-size limits MUST still be enforced before this short-circuit: an oversized declared length fails the call even when base and modulus lengths are zero.

All other precompiles MUST behave according to the inherited EVM baseline unless explicitly overridden elsewhere in this specification.

## Constants

| Constant                        | Value   | Description                                            |
| ------------------------------- | ------- | ------------------------------------------------------ |
| `KZG_POINT_EVALUATION_GAS_COST` | 100,000 | Fixed gas cost for the KZG Point Evaluation precompile |

## Security Considerations

This page has no security considerations.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the stable KZG Point Evaluation and ModExp overrides.
- [Rex](../upgrades/rex.md), [Rex1](../upgrades/rex1.md), [Rex2](../upgrades/rex2.md), and [Rex3](../upgrades/rex3.md) retain the same stable overrides.
- [Rex4](../upgrades/rex4.md) retains the same stable overrides; no change to KZG or ModExp pricing.
- [Rex5](../upgrades/rex5.md) refines the KZG error-path compute-gas recording.
