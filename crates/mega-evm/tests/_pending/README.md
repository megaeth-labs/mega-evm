# Pending tests

Tests of the legacy engine that survive into Satin but cannot run until the mechanism that owns them lands.
The dispositions come from the test inventory made for the engine rewrite and were applied mechanically when the Satin skeleton replaced the legacy core.
Every test the inventory marks `legacy-only` or `retired` was deleted; every other test is marked `keep`, `rewrite` or `undecided` and was moved here unchanged.
Decision ids (`Dnn`) index the Satin decision table, the numbered list of design decisions behind this engine; it is published with the engine's specification.

## How this directory is excluded from the build

Cargo discovers integration tests only as `tests/*.rs` and `tests/*/main.rs`.
`_pending/` has no `main.rs`, so nothing below it is compiled, formatted or linted.
Do not add a `_pending/main.rs`.

## Rules for porting

- Port the rows your mechanism owns into a real test target, adapting them to the Satin API and the decision cited in the table.
- `keep` rows keep their scenario and expectation; `rewrite` rows keep the scenario and take the new expectation from the cited decision; `undecided` rows wait for their decision.
- Delete a row from its file here in the same commit that ports it, and delete the file once it holds no rows.
- A row whose owning mechanism has landed and left it nothing to pin is retired rather than kept parked: it moves to "Tests retired after the inventory" with the reason, and its row is deleted from the file here in the same commit.
- The 44 legacy mutant killers the inventory kept (`tests/mutation/`) are not here: each was keyed to a surviving mutant of the legacy sources, and the test gates found no survivor in the Satin sources to regenerate one for.
  A mechanism whose code leaves a survivor gets a new killer from the mutation gate, next to the code or as a system test under `tests/mutation/`; what the retired rows cited is kept in "Retired mutant killers" below.
- Files under `src/` are the inline unit-test modules of the legacy core, extracted when the Satin skeleton replaced `crates/mega-evm/src`.
  The code they test is at `git show a8f8c7c9:crates/mega-evm/src/<path>`.
- Helper functions and `main.rs` / `common.rs` harness files were moved as they were; the owner decides what to keep.

## Tests per owning mechanism

| Owning mechanism | Tests | From `tests/` | From `src/` | Keep | Rewrite | Undecided |
|---|---:|---:|---:|---:|---:|---:|
| the common execution layer | 21 | 21 | 0 | 21 | 0 | 0 |
| history gas | 28 | 24 | 4 | 4 | 24 | 0 |
| compute gas | 26 | 26 | 0 | 0 | 26 | 0 |
| the data-size limit | 47 | 45 | 2 | 40 | 7 | 0 |
| detention | 81 | 76 | 5 | 73 | 8 | 0 |
| the state-growth and KV limits | 56 | 55 | 1 | 0 | 35 | 21 |
| revert-class aborts | 17 | 17 | 0 | 0 | 17 | 0 |
| the pre-block system calls | 34 | 22 | 12 | 30 | 4 | 0 |
| system contract deployment | 13 | 5 | 8 | 13 | 0 | 0 |
| the oracle and control contracts | 77 | 77 | 0 | 67 | 10 | 0 |
| native keyless deployment | 75 | 73 | 2 | 29 | 46 | 0 |
| inspector support | 4 | 4 | 0 | 1 | 3 | 0 |
| — (undecided: D57 preload-warm cold charging, D58 98/100 forwarding) | 7 | 7 | 0 | 0 | 0 | 7 |
| **Total** | **486** | **452** | **34** | **278** | **180** | **28** |

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

## Retired mutant killers

What the retired `tests/mutation/` rows cited, for the mechanisms that own them.

| Owning mechanism | Tests | Disposition | Decision |
|---|---:|---|---|
| the Satin gas table | 2 | rewrite | D04 |
| the Satin gas table | 1 | rewrite | D11 |
| SALT pricing | 8 | keep | the test gates regenerate |
| compute gas | 1 | rewrite | D53 |
| the data-size limit | 2 | keep | 13,107,200 unchanged |
| detention | 6 | keep | the test gates regenerate |
| the state-growth and KV limits | 1 | undecided | D46 |
| the state-growth and KV limits | 1 | rewrite | D45 |
| the block executor | 17 | keep | the test gates regenerate |
| the block executor | 5 | rewrite | single spec / Satin schedule |

## Tests parked under the common execution layer

These 21 rows belong to the test gates in the inventory and pin canonical CREATE and CREATE2 behavior.
The test gates had turned them into scenarios of a differential harness against revm 43; that harness is maintained outside this repository now, so the rows are parked here again, owed to the common execution layer, and counted under it above.
6 of them also assert the halt reason of a creation in a static callee: those assertions already run in `tests/satin/static_callee.rs`, under the same test names, and a port of one of those rows carries the rest of its assertions.

| Legacy file | Test | Static-callee halt reason |
|---|---|---|
| `rex4/create_safety.rs` | `test_create2_with_oversize_initcode_len_does_not_panic` | — |
| `rex5/create2_empty_initcode.rs` | `test_create2_len_zero_offset_zero_succeeds_on_both_specs` | — |
| `rex5/create2_empty_initcode.rs` | `test_rex5_create2_len_nonzero_offset_max_still_halts` | — |
| `rex5/create2_empty_initcode.rs` | `test_rex5_create2_len_zero_large_offset_skips_memory_expansion` | — |
| `rex5/create2_empty_initcode.rs` | `test_rex5_create2_len_zero_offset_max_succeeds` | — |
| `rex5/create2_empty_initcode.rs` | `test_rex5_create2_len_zero_offset_zero_succeeds` | — |
| `rex5/create2_resize_gas_metering.rs` | `test_create2_missing_salt_halts_consistently_across_specs` | — |
| `rex5/create2_resize_gas_metering.rs` | `test_rex5_create2_with_non_trivial_resize_succeeds` | — |
| `rex6/create2_metering_order.rs` | `test_create2_exact_boundary_initcode_length` | — |
| `rex6/create2_metering_order.rs` | `test_create2_missing_salt_consistent_rex5_rex6` | — |
| `rex6/create2_metering_order.rs` | `test_create2_moderately_oversized_initcode_same_reason_both_specs` | — |
| `rex6/create2_metering_order.rs` | `test_create2_oversized_initcode_halts_before_prework_rex6` | — |
| `rex6/create2_metering_order.rs` | `test_create2_oversized_len_unrepresentable_offset_halts_initcode_limit_rex6` | — |
| `rex6/create2_metering_order.rs` | `test_create2_static_hugely_oversized_initcode_halt_reason` | `tests/satin/static_callee.rs` |
| `rex6/create2_metering_order.rs` | `test_create2_static_missing_operands_halt_reason` | `tests/satin/static_callee.rs` |
| `rex6/create2_metering_order.rs` | `test_create2_static_oversized_initcode_reports_static_rejection` | `tests/satin/static_callee.rs` |
| `rex6/create2_metering_order.rs` | `test_create2_static_zero_length_initcode_reports_static_rejection` | `tests/satin/static_callee.rs` |
| `rex6/create2_metering_order.rs` | `test_create2_static_zero_length_low_gas_halt_reason` | `tests/satin/static_callee.rs` |
| `rex6/create2_metering_order.rs` | `test_create_static_low_gas_halt_reason` | `tests/satin/static_callee.rs` |
| `rex6/error_paths.rs` | `test_rex6_create2_missing_length_stack_underflow` | — |
| `rex6/error_paths.rs` | `test_rex6_create2_missing_offset_stack_underflow` | — |

## Tests ported by the common execution layer

These 45 rows run in a real test target now, adapted to the Satin API, so the counts above are lower than the inventory's by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `equivalence/evm_state.rs` | the common execution layer (3) | 3 | `tests/satin/state.rs` |
| `mini_rex/db_error.rs` | the common execution layer (4) | 4 | `tests/satin/db_error.rs` |
| `rex4/eip7702_delegation_cycle.rs` | the common execution layer (1) | 1 | `tests/satin/state.rs` |
| `rex5/call_too_deep_guard.rs` | the system contract interceptors (4) | 4 | `tests/satin/synthetic_frame_gas.rs` |
| `rex5/frame_target_updated_dedup.rs` | the common execution layer (5) | 5 | `tests/satin/write_records.rs` |
| `rex6/create_frame_accounting.rs` | the common execution layer (2) | 2 | `tests/satin/write_records.rs` |
| `rex6/self_transfer_account_dedup.rs` | the common execution layer (4) | 4 | `tests/satin/write_records.rs` |
| `src/evm/host.rs` | the common execution layer (10) | 10 | `src/evm/host.rs` |
| `src/evm/mod.rs` | the common execution layer (1) | 1 | `tests/satin/outcome.rs` |
| `src/evm/result.rs` | the common execution layer (4) | 4 | `src/evm/result.rs` |
| `src/limit/frame_limit.rs` | the common execution layer (4) | 4 | `src/limit/frame_limit.rs` |
| `src/limit/mod.rs` | the common execution layer (3) | 3 | `src/limit/mod.rs` |
| **Total** | | **45** | |

## Tests ported by the Satin gas table

These 34 rows run in a real test target now, adapted to the Satin API and to the schedule the decision they cite fixes, so the counts above are lower than the inventory's by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `compute_gas/claims.rs` | the Satin gas table (4) | 4 | `tests/satin/precompile_gas.rs`, `tests/satin/schedule.rs` |
| `mini_rex/contract_size_limit.rs` | the Satin gas table (12) | 12 | `tests/satin/contract_size.rs` |
| `mini_rex/gas.rs` | the Satin gas table (4) | 4 | `tests/satin/intrinsic.rs` |
| `rex5/gas_validation.rs` | the Satin gas table (4) | 4 | `tests/satin/intrinsic.rs` |
| `rex5/precompile_compute_gas.rs` | the Satin gas table (3) | 3 | `tests/satin/precompile_gas.rs` |
| `src/evm/factory.rs` | the Satin gas table (1) | 1 | `src/evm/factory.rs` |
| `src/evm/precompiles.rs` | the Satin gas table (6) | 6 | `src/evm/precompiles.rs` |
| **Total** | | **34** | |

## Tests ported by the block executor

These 53 rows run in a real test target now, adapted to the Satin API, so the counts above are lower than the inventory's by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `block_executor/accessed_block_hashes.rs` | the block executor (1) | 1 | `tests/block/block_hashes.rs` |
| `block_executor/block_limits.rs` | the block executor (12) | 12 | `tests/block/limits.rs` |
| `block_executor/canonical_schedule.rs` | the block executor (1) | 1 | `tests/block/schedule.rs` |
| `block_executor/deposit_da_exemption.rs` | the block executor (4) | 4 | `tests/block/limits.rs` |
| `block_executor/trait_factory_runtime_limits.rs` | the block executor (4) | 4 | `tests/block/factory.rs` |
| `rex6/sequencer_registry_rotation.rs` | the block executor (1) | 1 | `tests/block/schedule.rs` |
| `src/block/chain.rs` | the block executor (5) | 5 | `src/block/chain.rs` |
| `src/block/eips.rs` | the block executor (1) | 1 | `src/block/eips.rs` |
| `src/block/hardfork.rs` | the block executor (12) | 12 | `src/block/hardfork.rs` |
| `src/block/helpers.rs` | the block executor (3) | 3 | `src/block/helpers.rs` |
| `src/block/limit.rs` | the block executor (5) | 5 | `src/block/limit.rs` |
| `src/block/result.rs` | the block executor (2) | 2 | `src/block/result.rs` |
| `src/evm/mod.rs` | the block executor (1) | 1 | `src/evm/mod.rs` |
| `src/evm/state.rs` | the block executor (1) | 1 | `src/evm/state.rs` |
| **Total** | | **53** | |

## Tests ported by SALT pricing

These 67 rows run in a real test target now, adapted to the Satin API and to the pricing hook the decision they cite fixes, so the counts above are lower than the inventory's by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `mini_rex/gas.rs` | SALT pricing (20) | 20 | `tests/satin/salt.rs` |
| `rex/storage_gas.rs` | SALT pricing (15) | 15 | `tests/satin/salt.rs` |
| `rex4/eip7702_delegation_cycle.rs` | SALT pricing (8) | 8 | `tests/satin/salt_delegation.rs` |
| `rex5/callcode_storage_gas.rs` | SALT pricing (3) | 3 | `tests/satin/salt_delegation.rs` |
| `rex5/deposit_create_storage_gas.rs` | SALT pricing (4) | 4 | `tests/satin/salt_deposit.rs` |
| `rex5/eip7702_metering.rs` | SALT pricing (4) | 4 | `tests/satin/salt_delegation.rs` |
| `rex5/sstore_storage_gas_error.rs` | SALT pricing (1) | 1 | `tests/satin/salt_failure.rs` |
| `rex6/create_frame_accounting.rs` | SALT pricing (1) | 1 | `tests/satin/salt_failure.rs` |
| `rex6/error_paths.rs` | SALT pricing (2) | 2 | `tests/satin/salt_failure.rs` |
| `rex6/system_tx_metering_exemption.rs` | SALT pricing (3) | 3 | `tests/block/salt.rs`, `tests/satin/salt.rs` |
| `src/evm/context.rs` | SALT pricing (1) | 1 | `src/evm/context.rs` |
| `src/external/gas.rs` | SALT pricing (5) | 5 | `src/external/gas.rs`, `tests/satin/salt.rs` |
| **Total** | | **67** | |

## Tests ported by the system contract interceptors

These 61 rows run in a real test target now, adapted to the Satin API and to the dispatch the decision they cite fixes, so the counts above are lower than the inventory's by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `compute_gas/claims.rs` | the system contract interceptors (2) | 2 | `tests/system/dispatch.rs` |
| `mini_rex/mega_system_transaction.rs` | the system contract interceptors (15) | 15 | `src/system/tx.rs`, `tests/system/system_tx.rs` |
| `rex3/keyless_deploy.rs` | the system contract interceptors (2) | 2 | `tests/system/keyless.rs` |
| `rex4/limit_control.rs` | the system contract interceptors (5) | 5 | `tests/system/dispatch.rs`, `tests/system/limit_control.rs` |
| `rex5/db_error.rs` | the system contract interceptors (1) | 1 | `tests/system/dispatch.rs` |
| `rex5/deposit_caller_accounting.rs` | the system contract interceptors (7) | 7 | `tests/system/system_tx.rs` |
| `rex5/interceptor_selector_probe.rs` | the system contract interceptors (6) | 6 | `tests/system/dispatch.rs`, `tests/system/oracle.rs` |
| `rex5/keyless_deploy_dispatch_parity.rs` | the system contract interceptors (3) | 3 | `tests/system/keyless.rs` |
| `rex5/system_tx_replay.rs` | the system contract interceptors (12) | 12 | `tests/system/system_tx.rs` |
| `src/system/intercept.rs` | the system contract interceptors (2) | 2 | `tests/system/dispatch.rs`, `tests/system/limit_control.rs` |
| `src/system/tx.rs` | the system contract interceptors (6) | 6 | `src/system/tx.rs` |
| **Total** | | **61** | |

## Tests ported by system contract deployment

These 27 rows run in a real test target now, adapted to the Satin API and to the single-spec deploy, so the counts above are lower than the inventory's by exactly these rows.

| Legacy file | Owner in the inventory | Tests | Now in |
|---|---|---:|---|
| `block_executor/sequencer_registry.rs` | system contract deployment (1) | 1 | `tests/block/deploy.rs` |
| `mini_rex/oracle.rs` | system contract deployment (1) | 1 | `tests/block/deploy.rs` |
| `rex2/keyless_deploy.rs` | system contract deployment (1) | 1 | `tests/block/deploy.rs` |
| `rex4/deployment.rs` | system contract deployment (2) | 2 | `tests/block/deploy.rs` |
| `src/system/control.rs` | system contract deployment (6) | 6 | `tests/system/control.rs`, `tests/system/deploy.rs` |
| `src/system/deploy.rs` | system contract deployment (1) | 1 | `tests/system/deploy.rs` |
| `src/system/keyless_deploy.rs` | system contract deployment (1) | 1 | `tests/system/deploy.rs` |
| `src/system/limit_control.rs` | system contract deployment (3) | 3 | `tests/system/deploy.rs` |
| `src/system/oracle.rs` | system contract deployment (4) | 4 | `tests/system/deploy.rs` |
| `src/system/sequencer_registry.rs` | system contract deployment (7) | 7 | `tests/block/deploy.rs`, `tests/system/deploy.rs` |
| **Total** | | **27** | |

## Tests retired after the inventory

These 19 rows were parked when the inventory was applied and have since been retired: the mechanism that owns them landed and left them nothing to pin, so no later mechanism will port them.
They are not counted above.

| Legacy file | Owner in the inventory | Tests | Why |
|---|---|---:|---|
| `rex5/callcode_storage_gas.rs` | SALT pricing (3) | 3 | Satin's `CALLCODE` cannot reach a state gas pricing site: it sends value to the frame's own account, which exists, so it adds no account leaf and asks for no price. There is no pricing-path account inspection left to fail |
| `src/evm/host.rs` | SALT pricing (1) | 1 | the pricing hook inspects no account: the fork decides whether a target exists and the hook only prices what it is told to, so there is no delegation walk on the pricing path to guard |
| `src/external/gas.rs` | SALT pricing (4) | 4 | these pin a helper at a legacy spec boundary: it is served from one rung and asserts below it. Satin is a single spec and has no gate of its own, so there is no boundary left for them to pin |
| `src/system/control.rs` | system contract deployment (2) | 2 | Satin is a single spec with no per-fork deploy gate: the contract is deployed at every block; Satin does not overwrite foreign code: a system address with different code is an error, not an in-place upgrade |
| `src/system/deploy.rs` | system contract deployment (3) | 3 | Satin has no bytecode upgrade path: a system address with different code is an error, not a storage-preserving upgrade; Satin has no bytecode upgrade path: a system address with different code is an error, not a force-created upgrade; Satin is a single spec and ships one Oracle bytecode; there is no per-fork version table to pin |
| `src/system/keyless_deploy.rs` | system contract deployment (1) | 1 | Satin does not overwrite foreign code: a system address with different code is an error, not an in-place upgrade |
| `src/system/limit_control.rs` | system contract deployment (2) | 2 | Satin is a single spec with no per-fork deploy gate: the contract is deployed at every block; Satin does not overwrite foreign code: a system address with different code is an error, not an in-place upgrade |
| `src/system/oracle.rs` | system contract deployment (3) | 3 | Satin does not overwrite foreign code: a system address with different code is an error, not an in-place upgrade; Satin is a single spec and ships one Oracle bytecode; there is no per-fork version gate to pin |
| **Total** | | **19** | |

## Tests the inventory assigns to the Satin skeleton that are parked under another mechanism

| File | Test | Parked under | Reason |
|---|---|---|---|

## Tests parked under a later mechanism than the inventory named

| File | Test | Parked under | Reason |
|---|---|---|---|
| `src/system/sequencer_registry.rs` | `test_is_apply_pending_changes_due_checks_sequencer_when_system_not_due` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_is_apply_pending_changes_due_no_pending` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_is_apply_pending_changes_due_no_registry` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_is_apply_pending_changes_due_sequencer_due` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_is_apply_pending_changes_due_system_address_due` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_transact_apply_pending_changes_errors_when_registry_reverts` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_transact_apply_pending_changes_respects_30m_floor` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_transact_apply_pending_changes_updates_and_clears_due_roles` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `src/system/sequencer_registry.rs` | `test_transact_apply_pending_changes_uses_block_gas_limit` | the pre-block system calls | the pre-block system calls own transact_apply_pending_changes |
| `block_executor/sequencer_registry.rs` | `test_admin_handoff_via_block_executor` | the pre-block system calls | the pre-block system calls own the two-step admin handoff |
| `block_executor/sequencer_registry.rs` | `test_bootstrap_block_resolves_system_address` | the pre-block system calls | the pre-block system calls own resolving the live system address |
| `block_executor/sequencer_registry.rs` | `test_dual_change_in_same_block` | the pre-block system calls | the pre-block system calls own applying a pending rotation |
| `block_executor/sequencer_registry.rs` | `test_pending_not_yet_due_is_noop` | the pre-block system calls | the pre-block system calls own applying a pending rotation |
| `block_executor/sequencer_registry.rs` | `test_sequencer_change_does_not_affect_system_address` | the pre-block system calls | the pre-block system calls own applying a pending rotation |
| `block_executor/sequencer_registry.rs` | `test_system_address_change` | the pre-block system calls | the pre-block system calls own applying a pending rotation |
| `block_executor/sequencer_registry.rs` | `test_system_tx_uses_resolved_system_address` | the pre-block system calls | the pre-block system calls own resolving the live system address |

## Files

Each cell lists `disposition count (mechanism · decision)`.

| File | Tests | Owners |
|---|---:|---|
| `block_executor/block_limits.rs` | 2 | undecided 2 (the state-growth and KV limits · D46) |
| `block_executor/inspector.rs` | 3 | keep 1 (inspector support); rewrite 2 (inspector support · D39/D41) |
| `block_executor/sequencer_registry.rs` | 7 | keep 7 (the pre-block system calls) |
| `compute_gas/claims.rs` | 8 | rewrite 2 (compute gas · D53 (compute = regular spent; state spill excluded)); undecided 5 (— · D57, open: whether preload-warm addresses (precompile / beneficiary / access-list) are charged cold); rewrite 1 (the data-size limit · D33/D48) |
| `compute_gas/main.rs` | 1 | rewrite 1 (compute gas · D53) |
| `mini_rex/access_beneficiary_balance.rs` | 10 | keep 9 (detention · D08); rewrite 1 (revert-class aborts · D48) |
| `mini_rex/block_env_access_tracking.rs` | 3 | keep 3 (detention) |
| `mini_rex/block_env_gas_limit.rs` | 16 | keep 13 (detention · D08 cap 20M/1M unchanged); rewrite 3 (revert-class aborts · D48 (detention halt -> revert-class)) |
| `mini_rex/compute_gas_limit.rs` | 25 | rewrite 23 (compute gas · D10/D40/D53 (compute derived from Gas; 200M cap)); rewrite 2 (detention · D48) |
| `mini_rex/gas.rs` | 2 | undecided 2 (— · D58, open: 98/100 forwarding, while the design has frames follow EIP-8037 (63/64)) |
| `mini_rex/oracle.rs` | 12 | rewrite 3 (revert-class aborts · D48); keep 5 (detention); keep 4 (the oracle and control contracts) |
| `mini_rex/state_growth_limit.rs` | 4 | rewrite 4 (the state-growth and KV limits · D45 (state-gas limit)) |
| `mini_rex/tx_data_and_kv_update_limit.rs` | 28 | keep 18 (the data-size limit · data-size numbers unchanged); rewrite 4 (revert-class aborts · D48); undecided 6 (the state-growth and KV limits · D46 (the KV count stays as an output because the node consumes it; the limit semantics are undecided)) |
| `rex/oracle.rs` | 3 | rewrite 3 (detention · D08 (mark at actual load: same outcome via SLOAD)) |
| `rex2/keyless_deploy.rs` | 36 | rewrite 13 (native keyless deployment · native CREATE sub-frame; D37/D38); keep 19 (native keyless deployment · validation rules 1-9 unchanged); keep 1 (native keyless deployment · rule 4 (tx nonce == 0) unchanged); rewrite 3 (native keyless deployment · D36) |
| `rex2/oracle_hint.rs` | 6 | keep 6 (the oracle and control contracts) |
| `rex3/oracle_gas_limit.rs` | 8 | keep 7 (detention · D08); rewrite 1 (revert-class aborts · D48) |
| `rex3/system_address.rs` | 2 | keep 2 (detention · D51) |
| `rex4/access_control.rs` | 48 | keep 46 (the oracle and control contracts); rewrite 2 (detention · D07 (rejected volatile read pays static gas)) |
| `rex4/beneficiary_detention.rs` | 13 | keep 12 (detention · D08); rewrite 1 (revert-class aborts · D48) |
| `rex4/create_safety.rs` | 1 | keep 1 (the common execution layer · canonical revm behaviour) |
| `rex4/frame_limits.rs` | 20 | keep 10 (the data-size limit · data-size per-frame 98% kept); rewrite 1 (revert-class aborts · D48); undecided 9 (the state-growth and KV limits · D46) |
| `rex4/gas_detention.rs` | 5 | keep 3 (detention); rewrite 2 (revert-class aborts · D48/D53) |
| `rex4/intrinsic_limit_bypass.rs` | 13 | rewrite 6 (the data-size limit · D48/D49 (overflow outcome shape)); undecided 3 (the state-growth and KV limits · D46); keep 3 (the data-size limit); rewrite 1 (inspector support · D41) |
| `rex4/keyless_deploy.rs` | 2 | keep 2 (native keyless deployment · native sub-frame inherits env) |
| `rex4/limit_control.rs` | 9 | rewrite 9 (the oracle and control contracts · D40 (remaining compute derived from Gas)) |
| `rex4/storage_call_stipend.rs` | 12 | rewrite 12 (history gas · D14 (separated history-only allowance 160 x CPHB; three leak paths)) |
| `rex5/apply_pending_changes_gas_budget.rs` | 4 | rewrite 4 (the pre-block system calls · D51 (system source m = 1; the system-call reservoir split)) |
| `rex5/create2_empty_initcode.rs` | 5 | keep 5 (the common execution layer · canonical revm behaviour) |
| `rex5/create2_resize_gas_metering.rs` | 2 | keep 2 (the common execution layer · canonical behaviour) |
| `rex5/db_error.rs` | 3 | rewrite 3 (native keyless deployment · native path surfaces DB errors) |
| `rex5/eip7702_state_growth.rs` | 8 | rewrite 8 (the state-growth and KV limits · D28/D31/D45 (7702 authorization matrix)) |
| `rex5/keyless_empty_code_logs.rs` | 2 | rewrite 2 (native keyless deployment · native sub-frame keeps logs inherently) |
| `rex5/keyless_fee_free.rs` | 12 | rewrite 8 (native keyless deployment · D16/D37/D38 (GASPRICE native; materialisation explicit)); keep 4 (native keyless deployment · rules unchanged) |
| `rex5/keyless_gas_cap_postcap_recheck.rs` | 3 | rewrite 3 (native keyless deployment · native sub-frame gas handling) |
| `rex5/keyless_replay_barrier.rs` | 3 | rewrite 3 (native keyless deployment · D36 (real nonce increment)) |
| `rex5/oracle_hint_metering.rs` | 9 | keep 7 (the oracle and control contracts · D50 (hint not charged history; data-size metering kept)); rewrite 1 (the oracle and control contracts · D11 intrinsic number); rewrite 1 (revert-class aborts · D48) |
| `rex5/pre_block_system_calls.rs` | 11 | keep 11 (the pre-block system calls) |
| `rex5/sandbox_accounting.rs` | 9 | rewrite 9 (native keyless deployment · native sub-frame: parent tracker sees child directly) |
| `rex5/selfdestruct_beneficiary.rs` | 7 | rewrite 4 (the state-growth and KV limits · D45 (state gas via new-account site)); keep 2 (detention); keep 1 (the data-size limit) |
| `rex5/stipend_accounting.rs` | 6 | rewrite 6 (history gas · D14 (history-only allowance lifecycle)) |
| `rex6/beneficiary_detention.rs` | 16 | keep 13 (detention · D08); keep 2 (the data-size limit · D50 write record 40 B); rewrite 1 (the state-growth and KV limits · D45) |
| `rex6/create2_metering_order.rs` | 11 | keep 11 (the common execution layer · canonical halt reasons; D04 512 KiB boundary) |
| `rex6/eip7702_authority_accounting.rs` | 18 | rewrite 18 (the state-growth and KV limits · D12/D28/D31/D45 (7702 matrix; SALT pricing for the SALT half)) |
| `rex6/error_paths.rs` | 2 | keep 2 (the common execution layer · canonical) |
| `rex6/fee_reward_accounting.rs` | 6 | rewrite 6 (history gas · D50/D56 (tx body constant 310 = 110 + 40 x 5) / D45) |
| `rex6/frame_local_accounting.rs` | 3 | keep 3 (the data-size limit · LOG base 32 unchanged) |
| `rex6/keyless_sandbox_hardening.rs` | 3 | rewrite 1 (native keyless deployment · D44 / EIP-6780 native); keep 2 (native keyless deployment · canonical CREATE rules) |
| `rex6/oracle_hint_volatile_access.rs` | 4 | keep 4 (the oracle and control contracts) |
| `rex6/self_transfer_account_dedup.rs` | 1 | keep 1 (the data-size limit) |
| `rex6/sequencer_registry_rotation.rs` | 5 | keep 5 (system contract deployment) |
| `src/access/volatile.rs` | 4 | keep 4 (detention) |
| `src/evm/mod.rs` | 3 | keep 3 (the pre-block system calls) |
| `src/limit/compute_gas.rs` | 1 | rewrite 1 (detention · D40/D48) |
| `src/limit/data_size.rs` | 2 | keep 2 (the data-size limit) |
| `src/limit/kv_update.rs` | 1 | undecided 1 (the state-growth and KV limits · D46) |
| `src/limit/limit.rs` | 4 | keep 4 (history gas · D51) |
| `src/sandbox/execution.rs` | 2 | keep 1 (native keyless deployment · rule); rewrite 1 (native keyless deployment · D16) |
| `src/system/sequencer_registry.rs` | 17 | keep 8 (system contract deployment · the pre-block system calls for transact_apply_pending_changes); keep 9 (the pre-block system calls · the pre-block system calls for transact_apply_pending_changes) |
