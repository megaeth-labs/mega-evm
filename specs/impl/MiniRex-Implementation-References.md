# MiniRex Implementation References

This document is informative.
Normative semantics are defined in [MiniRex Specification](../MiniRex.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps each MiniRex specification and invariant to implementation.
It is intended for code navigation and auditing.

## Specification Mapping

### 1. Dual gas model

Spec clauses:
- Overall gas = compute gas + storage gas.
- SSTORE (0→non-0): 2,000,000 × multiplier.
- Account creation: 2,000,000 × multiplier.
- Code deposit: 10,000/byte.
- LOG topic: 3,750/topic. LOG data: 80/byte.
- Calldata: 40/zero-byte, 160/non-zero-byte. Floor: 100/zero, 400/non-zero.

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`mini_rex::SSTORE_SET_STORAGE_GAS`, `mini_rex::NEW_ACCOUNT_STORAGE_GAS`, `mini_rex::CODEDEPOSIT_STORAGE_GAS`, `mini_rex::LOG_TOPIC_STORAGE_GAS`, `mini_rex::LOG_DATA_STORAGE_GAS`)
- [crates/mega-evm/src/external/gas.rs](../../crates/mega-evm/src/external/gas.rs) (`sstore_set_gas`, `new_account_gas` — SALT bucket multiplier scaling)
- [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) (intrinsic calldata storage gas, account/contract creation storage gas)
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`additional_limit_ext::sstore`, `additional_limit_ext::log`)

### 2. Multi-dimensional resource limits

Spec clauses:
- Compute gas limit: 1B per transaction.
- Data size limit: 3.125 MB per transaction, 12.5 MB per block.
- KV updates limit: 125K per transaction, 500K per block.
- Halts when exceeded; remaining gas preserved.

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`mini_rex::TX_COMPUTE_GAS_LIMIT`, `mini_rex::TX_DATA_LIMIT`, `mini_rex::BLOCK_DATA_LIMIT`, `mini_rex::TX_KV_UPDATE_LIMIT`, `mini_rex::BLOCK_KV_UPDATE_LIMIT`)
- [crates/mega-evm/src/limit/mod.rs](../../crates/mega-evm/src/limit/mod.rs) (limit tracker initialization)
- [crates/mega-evm/src/limit/compute_gas.rs](../../crates/mega-evm/src/limit/compute_gas.rs), [crates/mega-evm/src/limit/data_size.rs](../../crates/mega-evm/src/limit/data_size.rs), [crates/mega-evm/src/limit/kv_update.rs](../../crates/mega-evm/src/limit/kv_update.rs) (per-dimension trackers)
- [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs) (`AdditionalLimit` — orchestrates all trackers)

### 3. Volatile data access control

Spec clauses:
- Block env opcodes cap compute gas at 20M.
- Beneficiary account access caps compute gas at 20M.
- CALL to oracle caps compute gas at 1M.
- MEGA_SYSTEM_ADDRESS exempted from oracle detention.
- Oracle SLOAD forced cold (2100 gas).
- Most restrictive cap applies.

Implementation:
- [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs) (`VolatileDataAccessTracker` — access tracking and cap enforcement, `apply_or_create_limit` — most-restrictive-wins logic)
- [crates/mega-evm/src/access/volatile.rs](../../crates/mega-evm/src/access/volatile.rs) (`VolatileDataAccess` — bitflag structure for access types)
- [crates/mega-evm/src/evm/host.rs](../../crates/mega-evm/src/evm/host.rs) (block env access marking, oracle SLOAD forced cold)
- [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) (`frame_init` — CALL-based oracle detection, MEGA_SYSTEM_ADDRESS exemption)
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`volatile_data_ext` — block env and beneficiary opcode wrappers with gas cap enforcement after volatile access, `compute_gas_ext` — compute gas usage tracking)
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`mini_rex::BLOCK_ENV_ACCESS_COMPUTE_GAS` = 20M, `mini_rex::ORACLE_ACCESS_COMPUTE_GAS` = 1M)

### 4. Modified gas forwarding (98/100 rule)

Spec clauses:
- Subcalls receive at most 98/100 of remaining gas.
- Applies to CALL and CREATE/CREATE2 in MiniRex.

Implementation:
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`forward_gas_ext` module — `wrap_gas_cap!` macro, 98/100 calculation; `mini_rex::instruction_table` — maps CALL, CREATE, CREATE2 to `forward_gas_ext` handlers)

### 5. SELFDESTRUCT disabled

Spec clauses:
- SELFDESTRUCT halts with `InvalidFEOpcode`.

Implementation:
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`mini_rex::instruction_table` — maps SELFDESTRUCT to `control::invalid`)

### 6. Contract size limits

Spec clauses:
- Max contract size: 524,288 bytes (512 KB).
- Max initcode size: 548,864 bytes (512 KB + 24 KB).

Implementation:
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`mini_rex::MAX_CONTRACT_SIZE`, `mini_rex::MAX_INITCODE_SIZE`)
- [crates/mega-evm/src/evm/context.rs](../../crates/mega-evm/src/evm/context.rs) (sets `limit_contract_code_size` and `limit_contract_initcode_size` in revm config)

### 7. Increased precompile gas costs

Spec clauses:
- KZG Point Evaluation (0x0A): 100,000 gas.
- ModExp (0x05): EIP-7883 gas schedule.

Implementation:
- [crates/mega-evm/src/evm/precompiles.rs](../../crates/mega-evm/src/evm/precompiles.rs) (`mini_rex()` — custom precompile set; `kzg_point_evaluation::GAS_COST` = 100,000; ModExp uses `revm::precompile::modexp::OSAKA`)

### 8. System contracts

Spec clauses:
- Oracle deployed at `0x6342000000000000000000000000000000000001`.
- High-precision timestamp oracle deployed at `0x6342000000000000000000000000000000000002`.
- Both deployed as pre-execution state changes on MiniRex activation.

Implementation:
- [crates/mega-evm/src/system/oracle.rs](../../crates/mega-evm/src/system/oracle.rs) (`ORACLE_CONTRACT_ADDRESS`, `transact_deploy_oracle_contract`, `transact_deploy_high_precision_timestamp_oracle`)
- [crates/mega-evm/src/block/executor.rs](../../crates/mega-evm/src/block/executor.rs) (deployment calls in `pre_execution_changes`)

## Invariant Mapping

- `I-1`: Overall gas = compute gas + storage gas.
  Implementation: gas accounting throughout [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) and [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs).
- `I-2`: Three resource dimensions enforced independently.
  Implementation: [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs) (`AdditionalLimit` checks each dimension separately).
- `I-3`: Halted transactions preserve remaining gas.
  Implementation: [crates/mega-evm/src/limit/limit.rs](../../crates/mega-evm/src/limit/limit.rs) (`rescued_gas` mechanism).
- `I-4`: Most restrictive volatile data cap applies.
  Implementation: [crates/mega-evm/src/access/tracker.rs](../../crates/mega-evm/src/access/tracker.rs) (`apply_or_create_limit` — takes minimum).
- `I-5`: Oracle SLOAD always cold.
  Implementation: [crates/mega-evm/src/evm/host.rs](../../crates/mega-evm/src/evm/host.rs) (`sload` — forces `is_cold = true` for oracle address).
- `I-6`: Subcalls receive at most 98/100 of remaining gas.
  Implementation: [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`forward_gas_ext::wrap_gas_cap!`).

## Maintenance Notes

Update this mapping when MiniRex semantics change.
Update this mapping when implementation locations move.
