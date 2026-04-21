---
description: Rex5 network upgrade — placeholder for the next upgrade cycle (unstable).
---

# Rex5 Network Upgrade

> **Unstable** — This spec is under active development.
> Its semantics may change before network activation.
> Activation timestamps have not been set.

This page is an informative summary of the Rex5 specification.
For the full normative definition, see the Rex5 spec in the mega-evm repository.

## Summary

Rex5 is the next upgrade cycle after Rex4.
It corrects a resource-accounting bug where the caller-account update was overcounted whenever a contract performed multiple value-transferring sub-calls or contract creations from the same call frame.

## What Changed

### Caller-Account Update Deduplication (Data Size and KV Updates)

**Previous behavior (Rex4 and earlier):**
When a call frame performed a value-transferring `CALL` / `CALLCODE` or a `CREATE` / `CREATE2`, the implementation charged the _caller_ account update to the child frame's discardable budget.
However, the parent frame's `target_updated` flag was never marked after the first charge.
As a result, every subsequent value-transferring sub-call or create from the same parent frame re-charged the caller account, overcounting both data-size bytes and KV-update counts for the caller.

**New behavior (Rex5):**
After charging the first caller-account update within a parent call frame, the frame's `target_updated` flag is marked.
All subsequent value-transferring sub-calls and creates from the same parent frame no longer re-charge the caller account.
Each distinct callee or created account is still counted independently.
The discardable-on-revert mechanic is unchanged: charges recorded inside a child frame that reverts are still dropped.

## Developer Impact

Transactions that perform multiple value-transferring sub-calls or creates from the same contract now report lower data-size and KV-update usage than they did under Rex4.
This only affects usage tracking; it does not change execution semantics, state transitions, or the base transaction gas model.

## Safety and Compatibility

This change is backward-compatible for stable specs.
Rex4 and earlier retain the original overcounting behavior unchanged.
Rex5 is the current unstable spec under active development; its semantics may still change before network activation.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model
