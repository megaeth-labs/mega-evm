# Pending tests

Tests of the legacy engine that survive into Satin but cannot run until the mechanism that owns them lands.
The dispositions come from the test inventory of the Satin work order and were applied mechanically when the Satin skeleton replaced the legacy core.
Every test the inventory marks 仅 legacy (legacy-only) or 废弃 (retired) was deleted; every other test was moved here unchanged.
The decision ids (`Dnn`) refer to the Satin decision table.

## How this directory is excluded from the build

Cargo discovers integration tests only as `tests/*.rs` and `tests/*/main.rs`.
`_pending/` has no `main.rs`, so nothing below it is compiled, formatted or linted.
Do not add a `_pending/main.rs`.

## Rules for porting

- Port the rows your mechanism owns into a real test target, adapting them to the Satin API and the decision cited in the table.
- 保留 (keep) rows keep their scenario and expectation; 重写 (rewrite) rows keep the scenario and take the new expectation from the cited decision; 待定 (undecided) rows wait for their decision.
- Delete a row from its file here in the same commit that ports it, and delete the file once it holds no rows.
- Files under `mutation/` are machine-generated mutant killers; the test gates regenerate them against the Satin sources instead of porting them by hand.
- Files under `src/` are the inline unit-test modules of the legacy core, extracted when the Satin skeleton replaced `crates/mega-evm/src`.
  The code they test is at `git show a8f8c7c9:crates/mega-evm/src/<path>`.
- Helper functions and `main.rs` / `common.rs` harness files were moved as they were; the owner decides what to keep.

## Tests per owning mechanism

| Owning mechanism | Tests | From `tests/` | From `src/` | 保留 | 重写 | 待定 |
|---|---:|---:|---:|---:|---:|---:|
| the test gates | 21 | 21 | 0 | 21 | 0 | 0 |
| the common execution layer | 41 | 19 | 22 | 22 | 19 | 0 |
| the Satin gas table | 37 | 30 | 7 | 7 | 30 | 0 |
| SALT pricing | 83 | 72 | 11 | 25 | 58 | 0 |
| history gas | 28 | 24 | 4 | 4 | 24 | 0 |
| compute gas | 27 | 27 | 0 | 0 | 27 | 0 |
| the data-size limit | 49 | 47 | 2 | 42 | 7 | 0 |
| detention | 87 | 82 | 5 | 79 | 8 | 0 |
| the state-growth and KV limits | 58 | 57 | 1 | 0 | 36 | 22 |
| revert-class aborts | 17 | 17 | 0 | 0 | 17 | 0 |
| the pre-block system calls | 18 | 15 | 3 | 14 | 4 | 0 |
| the system contract interceptors | 65 | 57 | 8 | 56 | 9 | 0 |
| system contract deployment | 67 | 17 | 50 | 62 | 5 | 0 |
| the oracle and control contracts | 77 | 77 | 0 | 67 | 10 | 0 |
| native keyless deployment | 75 | 73 | 2 | 29 | 46 | 0 |
| the block executor | 75 | 45 | 30 | 56 | 19 | 0 |
| inspector support | 4 | 4 | 0 | 1 | 3 | 0 |
| — (undecided: D57 preload-warm cold charging, D58 98/100 forwarding) | 7 | 7 | 0 | 0 | 0 | 7 |
| **Total** | **836** | **691** | **145** | **485** | **322** | **29** |

## Tests ported in place

These rows came back with the code they test and run in `crates/mega-evm/src`, so the counts above are lower than the inventory's per-mechanism totals by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `src/evm/context.rs` | the Satin skeleton (3) | 3 | `src/evm/context.rs` |
| `src/evm/factory.rs` | the Satin skeleton (1) | 1 | `src/evm/factory.rs` |
| `src/evm/mod.rs` | the Satin skeleton (6) | 6 | `src/evm/mod.rs` |
| `src/evm/spec.rs` | the Satin skeleton (3) | 3 | `src/evm/spec.rs` |
| `src/external/hasher/mod.rs` | SALT pricing (5) | 5 | `src/external/hasher/mod.rs` |
| `src/external/mod.rs` | the Satin skeleton (1) | 1 | `src/external/mod.rs` |
| `src/external/test_utils.rs` | the Satin skeleton (1) | 1 | `src/external/test_utils.rs` |
| `src/sandbox/error.rs` | native keyless deployment (4) | 4 | `src/system/keyless/error.rs` |
| `src/sandbox/tx.rs` | native keyless deployment (10) | 10 | `src/system/keyless/tx.rs` |
| `src/test_utils/opcode_gen.rs` | the Satin skeleton (2) | 2 | `src/test_utils/opcode_gen.rs` |
| **Total** | | **36** | |

## Tests the inventory assigns to the Satin skeleton that are parked under another mechanism

| File | Test | Parked under | Reason |
|---|---|---|---|
| `src/evm/context.rs` | `test_shared_salt_env_keeps_dynamic_gas_cache_isolated` | SALT pricing | exercises the dynamic storage-gas cache that SALT pricing brings back |
| `src/evm/factory.rs` | `test_dyn_precompiles_builder_receives_the_behavior_spec` | the Satin gas table | the dynamic precompile builder returns with the Satin precompile set; the behavior projection it pinned has no counterpart in a single-spec engine |
| `src/evm/mod.rs` | `test_convenience_execution_methods_work` | the common execution layer | `execute_transaction` returns the transaction outcome type that the common execution layer defines |
| `src/evm/mod.rs` | `test_mega_evm_exposes_state_wrapper_block_hashes` | the block executor | reads the accessed-block-hash record, which returns with the block executor |

## Files

Each cell lists `disposition count (mechanism · decision)`.

| File | Tests | Owners |
|---|---:|---|
| `block_executor/accessed_block_hashes.rs` | 1 | 保留 1 (the block executor) |
| `block_executor/block_limits.rs` | 14 | 保留 12 (the block executor · D47); 待定 2 (the state-growth and KV limits · D46) |
| `block_executor/canonical_schedule.rs` | 1 | 重写 1 (the block executor · single Satin fork; the specification pages own the doc side) |
| `block_executor/deposit_da_exemption.rs` | 4 | 保留 4 (the block executor · D23) |
| `block_executor/inspector.rs` | 3 | 保留 1 (inspector support); 重写 2 (inspector support · D39/D41) |
| `block_executor/sequencer_registry.rs` | 8 | 保留 8 (system contract deployment) |
| `block_executor/trait_factory_runtime_limits.rs` | 4 | 重写 4 (the block executor · D10 (200M execution cap replaces compute-gas runtime limit)) |
| `compute_gas/claims.rs` | 14 | 保留 2 (the system contract interceptors); 重写 2 (compute gas · D53 (compute = regular spent; state spill excluded)); 保留 1 (the Satin gas table · D04 (KZG 100k)); 待定 5 (— · no D-id: preload-warm addresses charged cold (precompile / beneficiary / access-list) is absent from DECISIONS.md); 重写 2 (the Satin gas table · D13); 重写 1 (the Satin gas table · D11); 重写 1 (the data-size limit · D33/D48) |
| `compute_gas/main.rs` | 1 | 重写 1 (compute gas · D53) |
| `equivalence/evm_state.rs` | 3 | 保留 3 (the common execution layer) |
| `mini_rex/access_beneficiary_balance.rs` | 10 | 保留 9 (detention · D08); 重写 1 (revert-class aborts · D48) |
| `mini_rex/block_env_access_tracking.rs` | 3 | 保留 3 (detention) |
| `mini_rex/block_env_gas_limit.rs` | 16 | 保留 13 (detention · D08 cap 20M/1M unchanged); 重写 3 (revert-class aborts · D48 (detention halt -> revert-class)) |
| `mini_rex/compute_gas_limit.rs` | 25 | 重写 23 (compute gas · D10/D40/D53 (compute derived from Gas; 200M cap)); 重写 2 (detention · D48) |
| `mini_rex/contract_size_limit.rs` | 12 | 重写 12 (the Satin gas table · D04 (512 KiB kept; initcode bound under Osaka/EIP-3860 not pinned in DECISIONS.md)) |
| `mini_rex/db_error.rs` | 4 | 保留 4 (the common execution layer) |
| `mini_rex/gas.rs` | 26 | 重写 14 (SALT pricing · D12 (state gas x m via pricing hook)); 重写 6 (SALT pricing · D12/D13); 待定 2 (— · no D-id: 98/100 forwarding; D1 §2.5 says frame semantics follow 8037 (63/64) but DECISIONS.md is silent); 重写 4 (the Satin gas table · D11/D55 (7976 floor)) |
| `mini_rex/mega_system_transaction.rs` | 15 | 保留 15 (the system contract interceptors · D51) |
| `mini_rex/oracle.rs` | 13 | 重写 3 (revert-class aborts · D48); 保留 5 (detention); 保留 4 (the oracle and control contracts); 重写 1 (system contract deployment · deploy at Satin activation) |
| `mini_rex/state_growth_limit.rs` | 4 | 重写 4 (the state-growth and KV limits · D45 (state-gas limit)) |
| `mini_rex/tx_data_and_kv_update_limit.rs` | 28 | 保留 18 (the data-size limit · data-size numbers unchanged); 重写 4 (revert-class aborts · D48); 待定 6 (the state-growth and KV limits · D46 (count kept as output per R1b; limit semantics undecided)) |
| `mutation/access_evm.rs` | 10 | 保留 6 (detention · the test gates regenerate); 保留 2 (SALT pricing · the test gates regenerate); 保留 1 (the block executor · the test gates regenerate); 重写 1 (the Satin gas table · D11) |
| `mutation/block.rs` | 22 | 保留 14 (the block executor · the test gates regenerate); 重写 5 (the block executor · single spec / Satin schedule); 待定 1 (the state-growth and KV limits · D46); 重写 1 (compute gas · D53); 重写 1 (the state-growth and KV limits · D45) |
| `mutation/constants.rs` | 4 | 重写 2 (the Satin gas table · D04); 保留 2 (the data-size limit · 13,107,200 unchanged) |
| `mutation/external_exec.rs` | 8 | 保留 2 (the block executor · the test gates regenerate); 保留 6 (SALT pricing · the test gates regenerate) |
| `rex/oracle.rs` | 3 | 重写 3 (detention · D08 (mark at actual load: same outcome via SLOAD)) |
| `rex/storage_gas.rs` | 15 | 重写 15 (SALT pricing · D12 (base price and min-bucket value change; multiplier logic kept)) |
| `rex2/keyless_deploy.rs` | 37 | 重写 13 (native keyless deployment · D1 §2.6 native CREATE sub-frame; D37/D38); 保留 19 (native keyless deployment · validation rules 1-9 unchanged); 保留 1 (native keyless deployment · rule 4 (tx nonce == 0) unchanged); 重写 3 (native keyless deployment · D36); 重写 1 (system contract deployment · deploy at Satin activation) |
| `rex2/oracle_hint.rs` | 6 | 保留 6 (the oracle and control contracts) |
| `rex3/keyless_deploy.rs` | 2 | 重写 2 (the system contract interceptors · D15 (explicit 100k compute) / D48) |
| `rex3/oracle_gas_limit.rs` | 8 | 保留 7 (detention · D08); 重写 1 (revert-class aborts · D48) |
| `rex3/system_address.rs` | 2 | 保留 2 (detention · D51) |
| `rex4/access_control.rs` | 48 | 保留 46 (the oracle and control contracts); 重写 2 (detention · D07 (rejected volatile read pays static gas)) |
| `rex4/beneficiary_detention.rs` | 13 | 保留 12 (detention · D08); 重写 1 (revert-class aborts · D48) |
| `rex4/create_safety.rs` | 1 | 保留 1 (the test gates · canonical revm behaviour (differential harness)) |
| `rex4/deployment.rs` | 2 | 重写 2 (system contract deployment · deploy at Satin activation) |
| `rex4/eip7702_delegation_cycle.rs` | 9 | 保留 8 (SALT pricing · account inspection on the pricing path); 保留 1 (the common execution layer) |
| `rex4/frame_limits.rs` | 20 | 保留 10 (the data-size limit · data-size per-frame 98% kept); 重写 1 (revert-class aborts · D48); 待定 9 (the state-growth and KV limits · D46) |
| `rex4/gas_detention.rs` | 5 | 保留 3 (detention); 重写 2 (revert-class aborts · D48/D53) |
| `rex4/intrinsic_limit_bypass.rs` | 13 | 重写 6 (the data-size limit · D48/D49 (overflow outcome shape)); 待定 3 (the state-growth and KV limits · D46); 保留 3 (the data-size limit); 重写 1 (inspector support · D41) |
| `rex4/keyless_deploy.rs` | 2 | 保留 2 (native keyless deployment · native sub-frame inherits env) |
| `rex4/limit_control.rs` | 14 | 重写 9 (the oracle and control contracts · D40 (remaining compute derived from Gas)); 保留 5 (the system contract interceptors) |
| `rex4/storage_call_stipend.rs` | 12 | 重写 12 (history gas · D14 (separated history-only allowance 160 x CPHB; three leak paths)) |
| `rex5/apply_pending_changes_gas_budget.rs` | 4 | 重写 4 (the pre-block system calls · D51 (system source m = 1; F1b reservoir split)) |
| `rex5/call_too_deep_guard.rs` | 4 | 保留 4 (the system contract interceptors · the synthetic-result contract of the common execution layer) |
| `rex5/callcode_storage_gas.rs` | 6 | 重写 3 (SALT pricing · D12); 保留 3 (SALT pricing · pricing-failure propagation) |
| `rex5/create2_empty_initcode.rs` | 5 | 保留 5 (the test gates · canonical revm behaviour (differential harness)) |
| `rex5/create2_resize_gas_metering.rs` | 2 | 保留 2 (the test gates · canonical behaviour) |
| `rex5/db_error.rs` | 4 | 重写 3 (native keyless deployment · native path surfaces DB errors); 保留 1 (the system contract interceptors) |
| `rex5/deposit_caller_accounting.rs` | 7 | 重写 7 (the system contract interceptors · D16 (kept; must not double-charge with 2780)) |
| `rex5/deposit_create_storage_gas.rs` | 4 | 重写 4 (SALT pricing · D12/D37) |
| `rex5/eip7702_metering.rs` | 4 | 重写 4 (SALT pricing · D12/D45) |
| `rex5/eip7702_state_growth.rs` | 8 | 重写 8 (the state-growth and KV limits · D28/D31/D45 (7702 authorization matrix)) |
| `rex5/frame_target_updated_dedup.rs` | 5 | 重写 5 (the common execution layer · D50/D56 write-record layer) |
| `rex5/gas_validation.rs` | 4 | 重写 4 (the Satin gas table · D49/D55 (intrinsic state component; floor vs cap)) |
| `rex5/interceptor_selector_probe.rs` | 6 | 保留 6 (the system contract interceptors) |
| `rex5/keyless_deploy_dispatch_parity.rs` | 3 | 保留 3 (the system contract interceptors) |
| `rex5/keyless_empty_code_logs.rs` | 2 | 重写 2 (native keyless deployment · native sub-frame keeps logs inherently) |
| `rex5/keyless_fee_free.rs` | 12 | 重写 8 (native keyless deployment · D16/D37/D38 (GASPRICE native; materialisation explicit)); 保留 4 (native keyless deployment · rules unchanged) |
| `rex5/keyless_gas_cap_postcap_recheck.rs` | 3 | 重写 3 (native keyless deployment · native sub-frame gas handling) |
| `rex5/keyless_replay_barrier.rs` | 3 | 重写 3 (native keyless deployment · D36 (real nonce increment)) |
| `rex5/oracle_hint_metering.rs` | 9 | 保留 7 (the oracle and control contracts · D50 (hint not charged history; data-size metering kept)); 重写 1 (the oracle and control contracts · D11 intrinsic number); 重写 1 (revert-class aborts · D48) |
| `rex5/pre_block_system_calls.rs` | 11 | 保留 11 (the pre-block system calls) |
| `rex5/precompile_compute_gas.rs` | 3 | 重写 3 (the Satin gas table · D04/D40 (assert via gas_used)) |
| `rex5/sandbox_accounting.rs` | 9 | 重写 9 (native keyless deployment · native sub-frame: parent tracker sees child directly) |
| `rex5/selfdestruct_beneficiary.rs` | 7 | 重写 4 (the state-growth and KV limits · D45 (state gas via new-account site)); 保留 2 (detention); 保留 1 (the data-size limit) |
| `rex5/sstore_storage_gas_error.rs` | 1 | 保留 1 (SALT pricing · pricing-failure path) |
| `rex5/stipend_accounting.rs` | 6 | 重写 6 (history gas · D14 (history-only allowance lifecycle)) |
| `rex5/system_tx_replay.rs` | 12 | 保留 12 (the system contract interceptors) |
| `rex6/beneficiary_detention.rs` | 16 | 保留 13 (detention · D08); 保留 2 (the data-size limit · D50 write record 40 B); 重写 1 (the state-growth and KV limits · D45) |
| `rex6/create2_metering_order.rs` | 11 | 保留 11 (the test gates · canonical halt reasons; D04 512 KiB boundary) |
| `rex6/create_frame_accounting.rs` | 3 | 重写 2 (the common execution layer · D50/D56 write-record layer); 保留 1 (SALT pricing) |
| `rex6/eip7702_authority_accounting.rs` | 18 | 重写 18 (the state-growth and KV limits · D12/D28/D31/D45 (7702 matrix; SALT pricing for the SALT half)) |
| `rex6/error_paths.rs` | 4 | 保留 2 (SALT pricing); 保留 2 (the test gates · canonical) |
| `rex6/fee_reward_accounting.rs` | 6 | 重写 6 (history gas · D50/D56 (tx body constant 310 = 110 + 40 x 5) / D45) |
| `rex6/frame_local_accounting.rs` | 3 | 保留 3 (the data-size limit · LOG base 32 unchanged) |
| `rex6/keyless_sandbox_hardening.rs` | 3 | 重写 1 (native keyless deployment · D44 / EIP-6780 native); 保留 2 (native keyless deployment · canonical CREATE rules) |
| `rex6/oracle_hint_volatile_access.rs` | 4 | 保留 4 (the oracle and control contracts) |
| `rex6/self_transfer_account_dedup.rs` | 5 | 重写 4 (the common execution layer · D50/D56); 保留 1 (the data-size limit) |
| `rex6/sequencer_registry_rotation.rs` | 6 | 保留 5 (system contract deployment); 保留 1 (the block executor · params validation at load) |
| `rex6/system_tx_metering_exemption.rs` | 3 | 重写 3 (SALT pricing · D51 (m = 1 for system source; history exempt)) |
| `src/access/volatile.rs` | 4 | 保留 4 (detention) |
| `src/block/chain.rs` | 5 | 重写 4 (the block executor · Satin activation timestamps; fallback pin = Satin); 保留 1 (the block executor) |
| `src/block/eips.rs` | 1 | 保留 1 (the block executor) |
| `src/block/hardfork.rs` | 12 | 重写 3 (the block executor · single fork); 保留 9 (the block executor) |
| `src/block/helpers.rs` | 3 | 保留 3 (the block executor) |
| `src/block/limit.rs` | 5 | 保留 5 (the block executor) |
| `src/block/result.rs` | 2 | 重写 2 (the block executor · D48 (error shape)) |
| `src/evm/context.rs` | 1 | 保留 1 (SALT pricing · SALT pricing for the SALT cache test) |
| `src/evm/factory.rs` | 1 | 重写 1 (the Satin gas table · no behaviour projection) |
| `src/evm/host.rs` | 11 | 保留 10 (the common execution layer · Host observation layer (D33) on the revm 40 journal); 保留 1 (SALT pricing) |
| `src/evm/mod.rs` | 5 | 保留 3 (the pre-block system calls); 保留 1 (the block executor); 保留 1 (the common execution layer) |
| `src/evm/precompiles.rs` | 6 | 保留 6 (the Satin gas table · D04) |
| `src/evm/result.rs` | 4 | 重写 4 (the common execution layer · D17/D48 (halt-reason set changes)) |
| `src/evm/state.rs` | 1 | 保留 1 (the block executor) |
| `src/external/gas.rs` | 9 | 重写 9 (SALT pricing · D12/D51) |
| `src/limit/compute_gas.rs` | 1 | 重写 1 (detention · D40/D48) |
| `src/limit/data_size.rs` | 2 | 保留 2 (the data-size limit) |
| `src/limit/frame_limit.rs` | 4 | 重写 4 (the common execution layer · data-size-only per-frame tracker) |
| `src/limit/kv_update.rs` | 1 | 待定 1 (the state-growth and KV limits · D46) |
| `src/limit/limit.rs` | 4 | 保留 4 (history gas · D51) |
| `src/limit/mod.rs` | 3 | 保留 3 (the common execution layer) |
| `src/sandbox/execution.rs` | 2 | 保留 1 (native keyless deployment · rule); 重写 1 (native keyless deployment · D16) |
| `src/system/control.rs` | 8 | 保留 8 (system contract deployment · the system contract interceptors for selector/revert-data tests) |
| `src/system/deploy.rs` | 4 | 保留 3 (system contract deployment); 重写 1 (system contract deployment · single version) |
| `src/system/intercept.rs` | 2 | 保留 2 (the system contract interceptors) |
| `src/system/keyless_deploy.rs` | 2 | 保留 2 (system contract deployment) |
| `src/system/limit_control.rs` | 5 | 保留 5 (system contract deployment) |
| `src/system/oracle.rs` | 7 | 保留 7 (system contract deployment · v2.0.0 only) |
| `src/system/sequencer_registry.rs` | 24 | 保留 24 (system contract deployment · the pre-block system calls for transact_apply_pending_changes) |
| `src/system/tx.rs` | 6 | 保留 6 (the system contract interceptors · D51) |
