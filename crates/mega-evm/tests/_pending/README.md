# Pending tests

Tests of the legacy engine that survive into Satin but cannot run until the ticket that owns them lands.
The dispositions come from the T0 test inventory of the Satin work order, and T2.1 applied them mechanically.
Every test T0 marks 仅 legacy (legacy-only) or 废弃 (retired) was deleted; every other test was moved here unchanged.
The decision ids (`Dnn`) refer to the Satin decision table.

## How this directory is excluded from the build

Cargo discovers integration tests only as `tests/*.rs` and `tests/*/main.rs`.
`_pending/` has no `main.rs`, so nothing below it is compiled, formatted or linted.
Do not add a `_pending/main.rs`.

## Rules for the owning ticket

- Port the rows your ticket owns into a real test target, adapting them to the Satin API and the decision cited in the table.
- 保留 (keep) rows keep their scenario and expectation; 重写 (rewrite) rows keep the scenario and take the new expectation from the cited decision; 待定 (undecided) rows wait for their decision.
- Delete a row from its file here in the same commit that ports it, and delete the file once it holds no rows.
- Files under `mutation/` are machine-generated mutant killers; T2.2 regenerates them against the Satin sources instead of porting them by hand.
- Files under `src/` are the inline unit-test modules of the legacy core, extracted when T2.1 replaced `crates/mega-evm/src`.
  The code they test is at `git show a8f8c7c9:crates/mega-evm/src/<path>`.
- Helper functions and `main.rs` / `common.rs` harness files were moved as they were; the owner decides what to keep.

## Tests per owning ticket

| Ticket | Tests | From `tests/` | From `src/` | 保留 | 重写 | 待定 |
|---|---:|---:|---:|---:|---:|---:|
| T2.2 | 21 | 21 | 0 | 21 | 0 | 0 |
| T2.3 | 41 | 19 | 22 | 22 | 19 | 0 |
| T3.1 | 37 | 30 | 7 | 7 | 30 | 0 |
| T3.2 | 83 | 72 | 11 | 25 | 58 | 0 |
| T3.3 | 28 | 24 | 4 | 4 | 24 | 0 |
| T3.4 | 27 | 27 | 0 | 0 | 27 | 0 |
| T4.1 | 49 | 47 | 2 | 42 | 7 | 0 |
| T4.2 | 87 | 82 | 5 | 79 | 8 | 0 |
| T4.3 | 58 | 57 | 1 | 0 | 36 | 22 |
| T5.1 | 17 | 17 | 0 | 0 | 17 | 0 |
| T5.2 | 18 | 15 | 3 | 14 | 4 | 0 |
| T6.1 | 65 | 57 | 8 | 56 | 9 | 0 |
| T6.2 | 67 | 17 | 50 | 62 | 5 | 0 |
| T6.3 | 77 | 77 | 0 | 67 | 10 | 0 |
| T7 | 75 | 73 | 2 | 29 | 46 | 0 |
| T8.1 | 75 | 45 | 30 | 56 | 19 | 0 |
| T9 | 4 | 4 | 0 | 1 | 3 | 0 |
| — (undecided: D57 preload-warm cold charging, D58 98/100 forwarding) | 7 | 7 | 0 | 0 | 0 | 7 |
| **Total** | **836** | **691** | **145** | **485** | **322** | **29** |

## Tests ported in place

These rows came back with the code they test and run in `crates/mega-evm/src`, so the counts above are lower than T0's per-ticket totals by exactly these rows.

| Legacy file | Owner in T0 | Tests | Now in |
|---|---|---:|---|
| `src/evm/context.rs` | T2.1 (3) | 3 | `src/evm/context.rs` |
| `src/evm/factory.rs` | T2.1 (1) | 1 | `src/evm/factory.rs` |
| `src/evm/mod.rs` | T2.1 (6) | 6 | `src/evm/mod.rs` |
| `src/evm/spec.rs` | T2.1 (3) | 3 | `src/evm/spec.rs` |
| `src/external/hasher/mod.rs` | T3.2 (5) | 5 | `src/external/hasher/mod.rs` |
| `src/external/mod.rs` | T2.1 (1) | 1 | `src/external/mod.rs` |
| `src/external/test_utils.rs` | T2.1 (1) | 1 | `src/external/test_utils.rs` |
| `src/sandbox/error.rs` | T7 (4) | 4 | `src/system/keyless/error.rs` |
| `src/sandbox/tx.rs` | T7 (10) | 10 | `src/system/keyless/tx.rs` |
| `src/test_utils/opcode_gen.rs` | T2.1 (2) | 2 | `src/test_utils/opcode_gen.rs` |
| **Total** | | **36** | |

## Tests T0 assigns to T2.1 that are parked under another ticket

| File | Test | Parked under | Reason |
|---|---|---|---|
| `src/evm/context.rs` | `test_shared_salt_env_keeps_dynamic_gas_cache_isolated` | T3.2 | exercises the dynamic storage-gas cache that SALT pricing brings back |
| `src/evm/factory.rs` | `test_dyn_precompiles_builder_receives_the_behavior_spec` | T3.1 | the dynamic precompile builder returns with the Satin precompile set; the behavior projection it pinned has no counterpart in a single-spec engine |
| `src/evm/mod.rs` | `test_convenience_execution_methods_work` | T2.3 | `execute_transaction` returns the transaction outcome type that T2.3 defines |
| `src/evm/mod.rs` | `test_mega_evm_exposes_state_wrapper_block_hashes` | T8.1 | reads the accessed-block-hash record, which returns with the block executor |

## Files

Each cell lists `disposition count (ticket · decision)`.

| File | Tests | Owners |
|---|---:|---|
| `block_executor/accessed_block_hashes.rs` | 1 | 保留 1 (T8.1) |
| `block_executor/block_limits.rs` | 14 | 保留 12 (T8.1 · D47); 待定 2 (T4.3 · D46) |
| `block_executor/canonical_schedule.rs` | 1 | 重写 1 (T8.1 · single Satin fork; T15 owns the doc side) |
| `block_executor/deposit_da_exemption.rs` | 4 | 保留 4 (T8.1 · D23) |
| `block_executor/inspector.rs` | 3 | 保留 1 (T9); 重写 2 (T9 · D39/D41) |
| `block_executor/sequencer_registry.rs` | 8 | 保留 8 (T6.2) |
| `block_executor/trait_factory_runtime_limits.rs` | 4 | 重写 4 (T8.1 · D10 (200M execution cap replaces compute-gas runtime limit)) |
| `compute_gas/claims.rs` | 14 | 保留 2 (T6.1); 重写 2 (T3.4 · D53 (compute = regular spent; state spill excluded)); 保留 1 (T3.1 · D04 (KZG 100k)); 待定 5 (— · no D-id: preload-warm addresses charged cold (precompile / beneficiary / access-list) is absent from DECISIONS.md); 重写 2 (T3.1 · D13); 重写 1 (T3.1 · D11); 重写 1 (T4.1 · D33/D48) |
| `compute_gas/main.rs` | 1 | 重写 1 (T3.4 · D53) |
| `equivalence/evm_state.rs` | 3 | 保留 3 (T2.3) |
| `mini_rex/access_beneficiary_balance.rs` | 10 | 保留 9 (T4.2 · D08); 重写 1 (T5.1 · D48) |
| `mini_rex/block_env_access_tracking.rs` | 3 | 保留 3 (T4.2) |
| `mini_rex/block_env_gas_limit.rs` | 16 | 保留 13 (T4.2 · D08 cap 20M/1M unchanged); 重写 3 (T5.1 · D48 (detention halt -> revert-class)) |
| `mini_rex/compute_gas_limit.rs` | 25 | 重写 23 (T3.4 · D10/D40/D53 (compute derived from Gas; 200M cap)); 重写 2 (T4.2 · D48) |
| `mini_rex/contract_size_limit.rs` | 12 | 重写 12 (T3.1 · D04 (512 KiB kept; initcode bound under Osaka/EIP-3860 not pinned in DECISIONS.md)) |
| `mini_rex/db_error.rs` | 4 | 保留 4 (T2.3) |
| `mini_rex/gas.rs` | 26 | 重写 14 (T3.2 · D12 (state gas x m via pricing hook)); 重写 6 (T3.2 · D12/D13); 待定 2 (— · no D-id: 98/100 forwarding; D1 §2.5 says frame semantics follow 8037 (63/64) but DECISIONS.md is silent); 重写 4 (T3.1 · D11/D55 (7976 floor)) |
| `mini_rex/mega_system_transaction.rs` | 15 | 保留 15 (T6.1 · D51) |
| `mini_rex/oracle.rs` | 13 | 重写 3 (T5.1 · D48); 保留 5 (T4.2); 保留 4 (T6.3); 重写 1 (T6.2 · deploy at Satin activation) |
| `mini_rex/state_growth_limit.rs` | 4 | 重写 4 (T4.3 · D45 (state-gas limit)) |
| `mini_rex/tx_data_and_kv_update_limit.rs` | 28 | 保留 18 (T4.1 · data-size numbers unchanged); 重写 4 (T5.1 · D48); 待定 6 (T4.3 · D46 (count kept as output per R1b; limit semantics undecided)) |
| `mutation/access_evm.rs` | 10 | 保留 6 (T4.2 · T2.2 regenerates); 保留 2 (T3.2 · T2.2 regenerates); 保留 1 (T8.1 · T2.2 regenerates); 重写 1 (T3.1 · D11) |
| `mutation/block.rs` | 22 | 保留 14 (T8.1 · T2.2 regenerates); 重写 5 (T8.1 · single spec / Satin schedule); 待定 1 (T4.3 · D46); 重写 1 (T3.4 · D53); 重写 1 (T4.3 · D45) |
| `mutation/constants.rs` | 4 | 重写 2 (T3.1 · D04); 保留 2 (T4.1 · 13,107,200 unchanged) |
| `mutation/external_exec.rs` | 8 | 保留 2 (T8.1 · T2.2 regenerates); 保留 6 (T3.2 · T2.2 regenerates) |
| `rex/oracle.rs` | 3 | 重写 3 (T4.2 · D08 (mark at actual load: same outcome via SLOAD)) |
| `rex/storage_gas.rs` | 15 | 重写 15 (T3.2 · D12 (base price and min-bucket value change; multiplier logic kept)) |
| `rex2/keyless_deploy.rs` | 37 | 重写 13 (T7 · D1 §2.6 native CREATE sub-frame; D37/D38); 保留 19 (T7 · validation rules 1-9 unchanged); 保留 1 (T7 · rule 4 (tx nonce == 0) unchanged); 重写 3 (T7 · D36); 重写 1 (T6.2 · deploy at Satin activation) |
| `rex2/oracle_hint.rs` | 6 | 保留 6 (T6.3) |
| `rex3/keyless_deploy.rs` | 2 | 重写 2 (T6.1 · D15 (explicit 100k compute) / D48) |
| `rex3/oracle_gas_limit.rs` | 8 | 保留 7 (T4.2 · D08); 重写 1 (T5.1 · D48) |
| `rex3/system_address.rs` | 2 | 保留 2 (T4.2 · D51) |
| `rex4/access_control.rs` | 48 | 保留 46 (T6.3); 重写 2 (T4.2 · D07 (rejected volatile read pays static gas)) |
| `rex4/beneficiary_detention.rs` | 13 | 保留 12 (T4.2 · D08); 重写 1 (T5.1 · D48) |
| `rex4/create_safety.rs` | 1 | 保留 1 (T2.2 · canonical revm behaviour (differential harness)) |
| `rex4/deployment.rs` | 2 | 重写 2 (T6.2 · deploy at Satin activation) |
| `rex4/eip7702_delegation_cycle.rs` | 9 | 保留 8 (T3.2 · account inspection on the pricing path); 保留 1 (T2.3) |
| `rex4/frame_limits.rs` | 20 | 保留 10 (T4.1 · data-size per-frame 98% kept); 重写 1 (T5.1 · D48); 待定 9 (T4.3 · D46) |
| `rex4/gas_detention.rs` | 5 | 保留 3 (T4.2); 重写 2 (T5.1 · D48/D53) |
| `rex4/intrinsic_limit_bypass.rs` | 13 | 重写 6 (T4.1 · D48/D49 (overflow outcome shape)); 待定 3 (T4.3 · D46); 保留 3 (T4.1); 重写 1 (T9 · D41) |
| `rex4/keyless_deploy.rs` | 2 | 保留 2 (T7 · native sub-frame inherits env) |
| `rex4/limit_control.rs` | 14 | 重写 9 (T6.3 · D40 (remaining compute derived from Gas)); 保留 5 (T6.1) |
| `rex4/storage_call_stipend.rs` | 12 | 重写 12 (T3.3 · D14 (separated history-only allowance 160 x CPHB; three leak paths)) |
| `rex5/apply_pending_changes_gas_budget.rs` | 4 | 重写 4 (T5.2 · D51 (system source m = 1; F1b reservoir split)) |
| `rex5/call_too_deep_guard.rs` | 4 | 保留 4 (T6.1 · T2.3 synthetic-result contract) |
| `rex5/callcode_storage_gas.rs` | 6 | 重写 3 (T3.2 · D12); 保留 3 (T3.2 · pricing-failure propagation) |
| `rex5/create2_empty_initcode.rs` | 5 | 保留 5 (T2.2 · canonical revm behaviour (differential harness)) |
| `rex5/create2_resize_gas_metering.rs` | 2 | 保留 2 (T2.2 · canonical behaviour) |
| `rex5/db_error.rs` | 4 | 重写 3 (T7 · native path surfaces DB errors); 保留 1 (T6.1) |
| `rex5/deposit_caller_accounting.rs` | 7 | 重写 7 (T6.1 · D16 (kept; must not double-charge with 2780)) |
| `rex5/deposit_create_storage_gas.rs` | 4 | 重写 4 (T3.2 · D12/D37) |
| `rex5/eip7702_metering.rs` | 4 | 重写 4 (T3.2 · D12/D45) |
| `rex5/eip7702_state_growth.rs` | 8 | 重写 8 (T4.3 · D28/D31/D45 (7702 authorization matrix)) |
| `rex5/frame_target_updated_dedup.rs` | 5 | 重写 5 (T2.3 · D50/D56 write-record layer) |
| `rex5/gas_validation.rs` | 4 | 重写 4 (T3.1 · D49/D55 (intrinsic state component; floor vs cap)) |
| `rex5/interceptor_selector_probe.rs` | 6 | 保留 6 (T6.1) |
| `rex5/keyless_deploy_dispatch_parity.rs` | 3 | 保留 3 (T6.1) |
| `rex5/keyless_empty_code_logs.rs` | 2 | 重写 2 (T7 · native sub-frame keeps logs inherently) |
| `rex5/keyless_fee_free.rs` | 12 | 重写 8 (T7 · D16/D37/D38 (GASPRICE native; materialisation explicit)); 保留 4 (T7 · rules unchanged) |
| `rex5/keyless_gas_cap_postcap_recheck.rs` | 3 | 重写 3 (T7 · native sub-frame gas handling) |
| `rex5/keyless_replay_barrier.rs` | 3 | 重写 3 (T7 · D36 (real nonce increment)) |
| `rex5/oracle_hint_metering.rs` | 9 | 保留 7 (T6.3 · D50 (hint not charged history; data-size metering kept)); 重写 1 (T6.3 · D11 intrinsic number); 重写 1 (T5.1 · D48) |
| `rex5/pre_block_system_calls.rs` | 11 | 保留 11 (T5.2) |
| `rex5/precompile_compute_gas.rs` | 3 | 重写 3 (T3.1 · D04/D40 (assert via gas_used)) |
| `rex5/sandbox_accounting.rs` | 9 | 重写 9 (T7 · native sub-frame: parent tracker sees child directly) |
| `rex5/selfdestruct_beneficiary.rs` | 7 | 重写 4 (T4.3 · D45 (state gas via new-account site)); 保留 2 (T4.2); 保留 1 (T4.1) |
| `rex5/sstore_storage_gas_error.rs` | 1 | 保留 1 (T3.2 · pricing-failure path) |
| `rex5/stipend_accounting.rs` | 6 | 重写 6 (T3.3 · D14 (history-only allowance lifecycle)) |
| `rex5/system_tx_replay.rs` | 12 | 保留 12 (T6.1) |
| `rex6/beneficiary_detention.rs` | 16 | 保留 13 (T4.2 · D08); 保留 2 (T4.1 · D50 write record 40 B); 重写 1 (T4.3 · D45) |
| `rex6/create2_metering_order.rs` | 11 | 保留 11 (T2.2 · canonical halt reasons; D04 512 KiB boundary) |
| `rex6/create_frame_accounting.rs` | 3 | 重写 2 (T2.3 · D50/D56 write-record layer); 保留 1 (T3.2) |
| `rex6/eip7702_authority_accounting.rs` | 18 | 重写 18 (T4.3 · D12/D28/D31/D45 (7702 matrix; T3.2 for the SALT half)) |
| `rex6/error_paths.rs` | 4 | 保留 2 (T3.2); 保留 2 (T2.2 · canonical) |
| `rex6/fee_reward_accounting.rs` | 6 | 重写 6 (T3.3 · D50/D56 (tx body constant 310 = 110 + 40 x 5) / D45) |
| `rex6/frame_local_accounting.rs` | 3 | 保留 3 (T4.1 · LOG base 32 unchanged) |
| `rex6/keyless_sandbox_hardening.rs` | 3 | 重写 1 (T7 · D44 / EIP-6780 native); 保留 2 (T7 · canonical CREATE rules) |
| `rex6/oracle_hint_volatile_access.rs` | 4 | 保留 4 (T6.3) |
| `rex6/self_transfer_account_dedup.rs` | 5 | 重写 4 (T2.3 · D50/D56); 保留 1 (T4.1) |
| `rex6/sequencer_registry_rotation.rs` | 6 | 保留 5 (T6.2); 保留 1 (T8.1 · params validation at load) |
| `rex6/system_tx_metering_exemption.rs` | 3 | 重写 3 (T3.2 · D51 (m = 1 for system source; history exempt)) |
| `src/access/volatile.rs` | 4 | 保留 4 (T4.2) |
| `src/block/chain.rs` | 5 | 重写 4 (T8.1 · Satin activation timestamps; fallback pin = Satin); 保留 1 (T8.1) |
| `src/block/eips.rs` | 1 | 保留 1 (T8.1) |
| `src/block/hardfork.rs` | 12 | 重写 3 (T8.1 · single fork); 保留 9 (T8.1) |
| `src/block/helpers.rs` | 3 | 保留 3 (T8.1) |
| `src/block/limit.rs` | 5 | 保留 5 (T8.1) |
| `src/block/result.rs` | 2 | 重写 2 (T8.1 · D48 (error shape)) |
| `src/evm/context.rs` | 1 | 保留 1 (T3.2 · T3.2 for the SALT cache test) |
| `src/evm/factory.rs` | 1 | 重写 1 (T3.1 · no behaviour projection) |
| `src/evm/host.rs` | 11 | 保留 10 (T2.3 · Host observation layer (D33) on the revm 40 journal); 保留 1 (T3.2) |
| `src/evm/mod.rs` | 5 | 保留 3 (T5.2); 保留 1 (T8.1); 保留 1 (T2.3) |
| `src/evm/precompiles.rs` | 6 | 保留 6 (T3.1 · D04) |
| `src/evm/result.rs` | 4 | 重写 4 (T2.3 · D17/D48 (halt-reason set changes)) |
| `src/evm/state.rs` | 1 | 保留 1 (T8.1) |
| `src/external/gas.rs` | 9 | 重写 9 (T3.2 · D12/D51) |
| `src/limit/compute_gas.rs` | 1 | 重写 1 (T4.2 · D40/D48) |
| `src/limit/data_size.rs` | 2 | 保留 2 (T4.1) |
| `src/limit/frame_limit.rs` | 4 | 重写 4 (T2.3 · data-size-only per-frame tracker) |
| `src/limit/kv_update.rs` | 1 | 待定 1 (T4.3 · D46) |
| `src/limit/limit.rs` | 4 | 保留 4 (T3.3 · D51) |
| `src/limit/mod.rs` | 3 | 保留 3 (T2.3) |
| `src/sandbox/execution.rs` | 2 | 保留 1 (T7 · rule); 重写 1 (T7 · D16) |
| `src/system/control.rs` | 8 | 保留 8 (T6.2 · T6.1 for selector/revert-data tests) |
| `src/system/deploy.rs` | 4 | 保留 3 (T6.2); 重写 1 (T6.2 · single version) |
| `src/system/intercept.rs` | 2 | 保留 2 (T6.1) |
| `src/system/keyless_deploy.rs` | 2 | 保留 2 (T6.2) |
| `src/system/limit_control.rs` | 5 | 保留 5 (T6.2) |
| `src/system/oracle.rs` | 7 | 保留 7 (T6.2 · v2.0.0 only) |
| `src/system/sequencer_registry.rs` | 24 | 保留 24 (T6.2 · T5.2 for transact_apply_pending_changes) |
| `src/system/tx.rs` | 6 | 保留 6 (T6.1 · D51) |
