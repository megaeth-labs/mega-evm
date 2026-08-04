---
description: Rex7 network upgrade — the current unstable spec, adopting the full EIP-7883 ModExp pricing schedule.
---

# Rex7 Network Upgrade

This page is an informative summary of the Rex7 specification.
For the full normative definition, see the Rex7 spec in the mega-evm repository.

{% hint style="warning" %}
**Unstable** — Rex7 is under active development.
Anything recorded on this page may change before Rex7 is frozen, and nothing here should be relied on.
{% endhint %}

## Summary

Rex7 is the spec currently open for development.
It inherits every [Rex6](rex6.md) behavior and, as of this page, changes one of them: the ModExp precompile charges the full EIP-7883 formula cost for zero-length inputs that the frozen specs short-circuit to the flat minimum.

Rex7 exists so that new behavior has somewhere to land.
[Rex6](rex6.md) is frozen — its semantics are fixed and may no longer be modified — so any change to gas costs, opcode behavior, resource accounting, or a system contract must be introduced under Rex7.

## What Changed

Each change landed under Rex7 is recorded here as a **Previous behavior** / **New behavior** pair, in the order it is specified.

### ModExp charges the full EIP-7883 formula cost for zero-length inputs

**Previous behavior**: a ModExp (`0x05`) call whose header declares `base_length = 0` and `modulus_length = 0` was charged the flat 500-gas minimum regardless of the declared exponent length — the behavior of the implementation the [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) schedule was adopted through, which returned the minimum before computing the formula cost.

**New behavior**: these inputs are charged the EIP-7883 formula cost, as the EIP text specifies — the formula's multiplication complexity never falls below 16, so a large declared exponent length prices in the thousands.
A call that forwards enough gas for the 500-gas minimum but not for the formula cost now halts with an out-of-gas failure, consuming the entire forwarded amount.
See [Precompiles](../evm/precompiles.md) for the full pricing rule and the frozen-spec boundary.

## Developer Impact

Contracts calling ModExp with degenerate zero-length headers — a shape with no computational use — pay the formula cost instead of the flat 500-gas minimum, and under-provisioned calls of that shape fail instead of succeeding.
No other contract, tool, or integration is affected; Rex7 is not scheduled on any network.

## Safety and Compatibility

Rex7 changes nothing about how blocks under earlier specs are executed.
Every spec through Rex6 is frozen: a node replaying historical blocks resolves each block's spec from its timestamp and applies that spec's semantics, unaffected by Rex7's existence.

Because Rex7 is unstable, its semantics may change in either direction until it is frozen.
Any node, tool, or test fixture pinned to Rex7 must expect its results to move.
A deployment that needs stable semantics must select a frozen spec explicitly rather than relying on the latest one.

## References

- [Hardforks and Specs](../hardfork-spec.md) — how specs are versioned, frozen, and activated.
- [Rex6 Network Upgrade](rex6.md) — the frozen spec Rex7 inherits from.
