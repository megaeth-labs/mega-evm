# Rex3 Implementation References

This document is informative.
Normative semantics are defined in [Rex3 Specification](../Rex3.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps each Rex3 spec change and invariant to implementation.
It is intended for code navigation and auditing.

## Change Mapping

### 1. Oracle access compute gas limit increase

Spec clauses:
- Oracle contract access caps compute gas at 20,000,000 (20M).
- Block environment access cap remains at 20M.

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`rex3::ORACLE_ACCESS_COMPUTE_GAS` = 20,000,000, lines 100–106)
- [crates/mega-evm/src/evm/limit.rs](../../crates/mega-evm/src/evm/limit.rs) (`rex3()` — sets `oracle_access_compute_gas_limit` from rex3 constant, inherits remaining limits from Rex, lines 76–82)

### 2. Oracle gas detention triggers on SLOAD

Spec clauses:
- SLOAD from oracle contract storage triggers gas detention.
- CALL to oracle contract does not trigger gas detention.
- Trigger is caller-agnostic (any SLOAD from oracle storage, regardless of call depth).
- DELEGATECALL to oracle does not trigger detention.
- MEGA_SYSTEM_ADDRESS transactions are exempted via `TxEnv.caller` check.

Implementation:
- [crates/mega-evm/src/evm/host.rs](../../crates/mega-evm/src/evm/host.rs) (`sload` method — marks oracle access when `MegaSpecId::REX3` enabled and caller is not MEGA_SYSTEM_ADDRESS, lines 109–141)
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`rex3::instruction_table` — maps SLOAD to `volatile_data_ext::sload`, lines 248–272; `volatile_data_ext::sload` — applies compute gas limit after SLOAD, lines 898–915)
- [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) (`frame_init` — skips CALL-based oracle detection for Rex3+, line 590)
- [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs) (`VolatileDataAccessTracker::check_and_mark_oracle_access` — shared marking logic, lines 148–156)

### 3. Keyless deploy compute gas tracking

Spec clauses:
- 100K keyless deploy overhead is recorded as compute gas.
- Compute gas limit exceeded halts execution.

Implementation:
- [crates/mega-evm/src/sandbox/execution.rs](../../crates/mega-evm/src/sandbox/execution.rs) (`execute_keyless_deploy_call` — calls `record_compute_gas(cost)` when `MegaSpecId::REX3` enabled, lines 157–174)

## Invariant Mapping

- `I-1`: Stable Rex2 semantics unchanged.
  Rex3 only modifies oracle detection mechanism (SLOAD vs CALL), oracle gas cap constant, and keyless deploy compute gas tracking.
- `I-2`: DELEGATECALL to oracle does not trigger detention.
  Implementation: [crates/mega-evm/src/evm/host.rs](../../crates/mega-evm/src/evm/host.rs) — SLOAD address check uses the storage context address, which is the caller's address under DELEGATECALL, not the oracle's.
- `I-3`: MEGA_SYSTEM_ADDRESS exemption regardless of call depth.
  Implementation: [crates/mega-evm/src/evm/host.rs](../../crates/mega-evm/src/evm/host.rs) — checks `self.caller()` (transaction sender from TxEnv), not frame-level caller.

## Maintenance Notes

Update this mapping when Rex3 semantics change.
Update this mapping when implementation locations move.
