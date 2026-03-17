# Rex1 Implementation References

This document is informative.
Normative semantics are defined in [Rex1 Specification](../Rex1.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps Rex1's spec change and invariants to implementation.
It is intended for code navigation and auditing.

## Change Mapping

### 1. Compute gas limit reset between transactions

Spec clauses:
- Compute gas limit resets to configured value at the start of each transaction.
- Gas detention from volatile data access is scoped to the triggering transaction only.

Implementation:
- [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs) (`reset()` — resets `detained_limit` to TX limit when `rex1_enabled`; also resets usage to zero as part of the frame tracker reset, lines 145–153)
- [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs) (`AdditionalLimit::reset()` — coordinates reset of all limit trackers, lines 145–152)
- [crates/mega-evm/src/evm/context.rs](../../crates/mega-evm/src/evm/context.rs) (`MegaContext::on_new_tx()` — calls `additional_limit.reset()` before each transaction, lines 545–554)

## Invariant Mapping

- `I-1`: Stable Rex semantics unchanged.
  This is a negative invariant (no new behavior introduced). Rex1 reuses Rex constants and instruction tables — no Rex1-specific constants exist in [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs), and no Rex1-specific instruction table exists in [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs).
  Not directly testable as a single assertion; coverage is structural.
- `I-2`: Volatile data access detention does not affect subsequent transactions.
  Implementation: [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs) (`reset()` method).

## Maintenance Notes

Update this mapping when Rex1 semantics change.
Update this mapping when implementation locations move.
