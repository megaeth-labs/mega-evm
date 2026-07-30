---
description: Rex7 network upgrade — the current unstable spec, open for development and carrying no behavioral change over Rex6 yet.
---

# Rex7 Network Upgrade

This page is an informative summary of the Rex7 specification.
For the full normative definition, see the Rex7 spec in the mega-evm repository.

{% tabs %}
{% tab title="Testnet" %}
Not yet scheduled
{% endtab %}
{% tab title="Mainnet" %}
Not yet scheduled
{% endtab %}
{% endtabs %}

{% hint style="warning" %}
**Unstable** — Rex7 is under active development.
Anything recorded on this page may change before Rex7 is frozen, and nothing here should be relied on.
{% endhint %}

## Summary

Rex7 is the spec currently open for development.
It inherits every [Rex6](rex6.md) behavior and, as of this page, changes none of them: a transaction executed under Rex7 produces the same result as the same transaction executed under Rex6.

Rex7 exists so that new behavior has somewhere to land.
[Rex6](rex6.md) is frozen — its semantics are fixed and may no longer be modified — so any change to gas costs, opcode behavior, resource accounting, or a system contract must be introduced under Rex7.

## What Changed

Nothing yet.

Each change landed under Rex7 will be recorded here as a **Previous behavior** / **New behavior** pair, in the order it is specified.

## Developer Impact

None.

Rex7 is not scheduled on any network, and it is behaviorally identical to Rex6, so no contract, tool, or integration needs to do anything today.

## Safety and Compatibility

Rex7 changes nothing about how blocks under earlier specs are executed.
Every spec through Rex6 is frozen: a node replaying historical blocks resolves each block's spec from its timestamp and applies that spec's semantics, unaffected by Rex7's existence.

Because Rex7 is unstable, its semantics may change in either direction until it is frozen.
Any node, tool, or test fixture pinned to Rex7 must expect its results to move.
A deployment that needs stable semantics must select a frozen spec explicitly rather than relying on the latest one.

## References

- [Hardforks and Specs](../hardfork-spec.md) — how specs are versioned, frozen, and activated.
- [Rex6 Network Upgrade](rex6.md) — the frozen spec Rex7 inherits from.
