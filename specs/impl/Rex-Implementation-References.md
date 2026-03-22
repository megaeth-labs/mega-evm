# Rex Implementation References

This document is informative.
Normative semantics are defined in [Rex Specification](../Rex.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps each Rex spec change and invariant to implementation.
It is intended for code navigation and auditing.

## Change Mapping

### 1. Transaction intrinsic storage gas

Spec clauses:
- Every transaction pays 39,000 additional storage gas.
- Total intrinsic gas is 60,000 (21,000 compute + 39,000 storage).

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`rex::TX_INTRINSIC_STORAGE_GAS`)
- [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) (intrinsic gas calculation, gated by `MegaSpecId::REX`)

### 2. Storage gas economics

Spec clauses:
- SSTORE (0→non-0) costs `20,000 × (multiplier - 1)`.
- Account creation costs `25,000 × (multiplier - 1)`.
- Contract creation costs `32,000 × (multiplier - 1)`.
- At multiplier = 1, all three operations cost zero storage gas.

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`rex::SSTORE_SET_STORAGE_GAS_BASE`, `rex::NEW_ACCOUNT_STORAGE_GAS_BASE`, `rex::CONTRACT_CREATION_STORAGE_GAS_BASE`)
- [crates/mega-evm/src/external/gas.rs](../../crates/mega-evm/src/external/gas.rs) (`sstore_set_gas`, `new_account_gas`, `create_contract_gas` — formula branching on `MegaSpecId::REX`)
- [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) (account/contract creation storage gas application)

### 3. Consistent behavior among CALL-like opcodes

Spec clauses:
- CALLCODE, DELEGATECALL, and STATICCALL enforce 98/100 gas forwarding.
- STATICCALL triggers oracle access detection.
- DELEGATECALL and CALLCODE do not trigger oracle access detection.

Implementation:
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`rex::instruction_table` — adds `forward_gas_ext::call_code`, `forward_gas_ext::delegate_call`, `forward_gas_ext::static_call`)
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`forward_gas_ext` module — 98/100 gas capping implementation)

### 4. Transaction and block limits

Spec clauses:
- Transaction data size limit is 12.5 MB.
- Transaction KV update limit is 500,000.
- Transaction compute gas limit is 200,000,000.
- Transaction and block state growth limits are 1,000.
- Transaction-level state growth exceed halts with OutOfGas.

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`rex::TX_COMPUTE_GAS_LIMIT`, `rex::TX_DATA_LIMIT`, `rex::TX_KV_UPDATE_LIMIT`, `rex::TX_STATE_GROWTH_LIMIT`, `rex::BLOCK_STATE_GROWTH_LIMIT`)
- [crates/mega-evm/src/limit/mod.rs](../../crates/mega-evm/src/limit/mod.rs) (limit tracker initialization and enforcement)

## Invariant Mapping

- `I-1`: Storage gas zero at multiplier=1.
  Implementation: [crates/mega-evm/src/external/gas.rs](../../crates/mega-evm/src/external/gas.rs) (`(multiplier - 1)` formula yields zero).
- `I-2`: All CALL-like opcodes enforce 98/100 gas forwarding.
  Implementation: [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`rex::instruction_table`).
- `I-3`: DELEGATECALL and CALLCODE do not trigger oracle detection.
  Implementation: [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (oracle detection only wired for CALL and STATICCALL).
- `I-4`: State growth exceed halts with OutOfGas.
  Implementation: [crates/mega-evm/src/limit/state_growth.rs](../../crates/mega-evm/src/limit/state_growth.rs) (`check_limit()`).
- `I-5`: Max total gas limit is chain-configurable.
  This is a design constraint, not directly mapped to a single source location.

## Maintenance Notes

Update this mapping when Rex semantics change.
Update this mapping when implementation locations move.
