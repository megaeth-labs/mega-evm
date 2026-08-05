# Domain mutator — open questions / future work

## Known equivalent mutants

Survivors that were analysed and found to have no observable effect. Each entry
carries the proof; do not spend test budget on them again.

- `spec_gate:crates/mega-evm/src/limit/state_growth.rs:169:REX4:true` —
  `current_call_remaining`. Only affects pre-REX4 specs, and the sole consumer of that
  value (`sandbox/execution.rs`'s `with_tx_state_growth_limit` for the REX5+ `KeylessDeploy`
  sandbox) is unreachable before REX5, so no pre-REX4 execution ever observes the added
  `min(frame_remaining, tx_remaining)`. Pre-REX4 frames are also pushed with `u64::MAX`
  (state_growth.rs:121, the only push path), so the `min` is a no-op for every state this
  tracker can reach through its own API except a refund-skewed corner with an unlimited TX
  budget — which still has no consumer.
- `spec_gate:crates/mega-evm/src/limit/state_growth.rs:206:REX4:true` — `check_limit`. The
  REX4 branch is a strict prefix of the shared TX-level check: it only adds
  `exceeds_current_frame_limit` before falling through to the same `tx_usage()` vs
  `tx_limit()` comparison the pre-REX4 path performs. Pre-REX4 frames carry a `u64::MAX`
  limit, so `used - refund > limit` can never hold and the added branch can never fire.
  Extensionally identical for every reachable state.
- `spec_gate:crates/mega-evm/src/limit/data_size.rs:74:REX5:true` — `DataSizeTracker`'s
  `rex5_enabled`. Its only read is as a disjunct inside the `debug_assert!` in `check_limit`
  (data_size.rs:193); forcing it `true` only relaxes that assertion. The field is private and
  has no accessor, so nothing else can observe it.
- `storage_call_stipend.rs` `if burn > 0` → `>=` (cargo-mutants operator). `burn == 0` makes
  the guarded call `gas.record_regular_cost(0)`, which subtracts nothing and cannot fail.
- `storage_call_stipend.rs` `if stipend > 0` → `>=` (cargo-mutants operator). With
  `stipend == 0` the taken branch computes `gas.remaining().min(gas.limit() - 0)`, and
  `remaining <= limit` always holds, so it returns `gas.remaining()` — exactly the `else`
  branch.

### Pre-`MINI_REX` additional-limit gates in `evm/execution.rs`

Four gates share one proof, so it is stated once here. Before `MINI_REX` the whole
`AdditionalLimit` subsystem is inert:

- `MegaContext::on_new_tx` gates `reset()` + `before_tx_start()` on `MINI_REX` (`evm/context.rs`),
  so no intrinsic usage is ever recorded.
- `MegaHandler::validate` gates `record_compute_gas(initial_regular_gas)` on `MINI_REX`, so no
  intrinsic compute gas is recorded either.
- `MegaInstructions::new` hands `EQUIVALENCE` revm's stock mainnet table
  (`evm/instructions.rs`), so none of the `compute_gas_ext` / `storage_gas_ext` wrappers — the
  only per-opcode recorders — are installed.
- `EvmTxRuntimeLimits::from_spec(EQUIVALENCE)` is `no_limits()`: every dimension is `u64::MAX`
  (`evm/limit.rs`), so `used > limit` can never hold and `check_limit()` is constant
  `WithinLimit`.

Every dimension therefore stays at 0 with the aggregate latched `WithinLimit`
(pinned by `tests/equivalence/pre_mini_rex_gates.rs::test_equivalence_leaves_additional_limit_dormant`).
Enabling any of these hooks on that side moves nothing observable:

- `spec_gate:crates/mega-evm/src/evm/execution.rs:426:MINI_REX:true` — `before_frame_run`. All four
  sub-tracker `before_frame_run` implementations are the trait's default empty body
  (`limit/frame_limit.rs`, no overrides), so the call reduces to `check_limit()`, which is constant
  `WithinLimit` and returns `None`.
- `spec_gate:crates/mega-evm/src/evm/execution.rs:508:MINI_REX:true` — `after_frame_run`. The
  `gas_remaining_before_process_action` argument is `None` on this side (its producer at line 1365
  is itself `MINI_REX`-gated), so the body reduces to `try_rescue_gas`, which is a no-op unless a
  limit is latched as exceeded.
- `spec_gate:crates/mega-evm/src/evm/execution.rs:988:MINI_REX:true` — `last_frame_result`. The
  sub-tracker pops run against empty frame stacks (`FrameLimitTracker::pop_frame` returns `None`),
  the `LAST_FRAME = true` const argument satisfies the non-empty-stack assertions, `check_limit()`
  is `WithinLimit` so the result is not rewritten, and the trailing
  `gas.erase_cost(rescued_gas)` erases 0.
- `spec_gate:crates/mega-evm/src/evm/execution.rs:1506:MINI_REX:true` — `inspect_frame_init`'s
  `push_empty_frame`. It only pushes frame entries carrying zero usage and a `u64::MAX` derived
  limit; `get_usage()` reports `net_usage()`, which counts recorded usage only, and no
  frame-local check is reachable pre-`REX4`.

### Consumer-discarded values

- `spec_gate:crates/mega-evm/src/evm/execution.rs:1365:MINI_REX:true` and
  `spec_gate:crates/mega-evm/src/evm/execution.rs:1600:MINI_REX:true` — the
  `gas_remaining_before` snapshots in `frame_run` / `inspect_frame_run`. Each value has exactly one
  consumer, `MegaEvm::after_frame_run`, whose first statement returns early on
  `!ctx.spec.is_enabled(MegaSpecId::MINI_REX)` against the same `ctx`. Forcing the predicate `true`
  changes the snapshot only when `MINI_REX` is off, and in that case the consumer discards it
  unread. (The `:false` bodies are real and are killed by
  `tests/rex4/inspector_frame_run_parity.rs`.)

### Unreachable-state gates

- `spec_gate:crates/mega-evm/src/evm/execution.rs:1412:REX4:true` — `frame_return_result`'s
  `enable_access_if_returning`. It is a no-op unless `VolatileDataAccessTracker::disable_depth` is
  `Some`. The only production writer is `disable_access`, called from `AccessControlInterceptor`,
  whose `ACTIVATION_SPEC` is `REX4`; the `MegaAccessControl` contract is also only deployed from
  `REX4` on. `on_new_tx` additionally clears the field via `reset_volatile_data_access` at every
  transaction start, so on the pre-`REX4` side `disable_depth` is `None` for the whole frame
  lifetime and the hook cannot observe anything. (The field is reachable from a test through the
  `pub` tracker API, so this is a reachability argument, not a pure extensional one.)
- `spec_gate:crates/mega-evm/src/evm/context.rs:700:MINI_REX:true` — `on_new_block`'s
  `DynamicGasCost::on_new_block`, which is `reset(parent_block)`: clear the bucket-multiplier cache
  and store the parent block number. Both fields are read only by the SALT gas helpers
  (`new_account_gas`, `create_contract_gas`, `sstore_set_gas`, `get_bucket_ids`), and every caller
  of those is `MINI_REX`-gated — `MegaContext::{new_account_storage_gas,
  create_contract_storage_gas, sstore_set_storage_gas}` each carry
  `debug_assert!(spec.is_enabled(MegaSpecId::MINI_REX))` and are reached only from the
  `MINI_REX`-gated instruction wrappers and the `MINI_REX` branch of `validate`. Pre-`MINI_REX` the
  cache is therefore always empty and `parent_block` is never read.

### cargo-mutants operators in `evm/execution.rs`

- `EvmTr::frame_stack` returning a leaked fresh `FrameStack` (both bodies). The method has three
  call sites reachable from this workspace — `revm-handler-20.0.3` `handler.rs` in
  `execution_result` and `catch_error`, and `MegaHandler::execution_result`'s sandbox branch — and
  all three are `evm.frame_stack().clear()`. `FrameStack::clear` only assigns `index = None`
  (`revm-context-interface-19.0.3` `local.rs`), and every subsequent use of the real stack starts
  at `FrameStack::start_init`, which assigns `index = None` as well. Redirecting the reference to a
  fresh stack therefore changes no state any later read can observe. Confirmed by a full
  `cargo test -p mega-evm` run under the mutation.
- `record_rex5_eip7702_authority_state_growth`'s `if authority_creations > 0` → `>=`. With
  `authority_creations == 0` the guarded call is
  `on_rex5_eip7702_authority_creations(0)`, which records zero state growth and then runs
  `check_limit()`. `check_limit()` mutates only `has_exceeded_limit` and is sticky — it
  short-circuits on a non-`WithinLimit` aggregate, so it can neither downgrade an existing latch
  nor introduce one that the resource-limit check protocol has not already latched at the mutation
  site that recorded the usage. Confirmed by a full `cargo test -p mega-evm` run under the
  mutation.

## Out of scope for v0

- `spec.is_enabled(SomeType::ACTIVATION_SPEC)` style gates (associated constants,
  not a literal `MegaSpecId::X` path). Appears in `system/intercept.rs` for
  interceptor activation.
- Flipping `is_enabled` *arguments* (e.g. `REX5` → `REX4`) — a stronger operator
  already referenced by a comment in `limit/limit.rs`; not implemented here.
- Equivalent gates written as `matches!` / ordinal comparisons.
- Constants outside `constants.rs` (inline magic numbers, re-exported revm gas
  names under `equivalence`).
- Parallel mutant execution / incremental test selection.

## Known heuristics / footguns

- Receiver walk for `.is_enabled` stops at whitespace and operators; unusual
  formatting (receiver split across lines with a line break *before* the `.`)
  would be missed. Current tree has no such sites.
- String-literal detection is single-line and double-quote only.
- Gas-const occurrence keys on the exact RHS text (`500_000`, `1000`, …).
  If a comment ever contains the same digit form, occurrence indexing could
  target the wrong span — review exclusions if that appears.
- Wrapping multi-line RHS as `(expr) ± 1` collapses formatting to one line
  (still valid Rust).

## Oracle

- L2 (`mega-state-test`) may be sparse relative to MegaETH-specific gates;
  most kills are expected at L1.
- No compile-only fast path yet: a type-error mutant still pays full `cargo test`
  startup (rustc fails early inside the same invocation).
- Consider an optional L0 `cargo check -p mega-evm` for faster kill on syntax
  breakage only — not added to keep the oracle definition simple.

## Campaign ops

- Full inventory (~274 bodies post M4 exclusions × ~1 min) is multi-hour; use `--resume`.
- State JSON is written atomically (temp + `os.replace`); binding validates HEAD /
  inventory hash / oracle on resume (M4).
- Crash journal + regular restore share compare-and-restore; concurrent-edit
  mismatch refuses restore, keeps journal, aborts campaign (M5).
- Full-tree `git status --porcelain` after baseline and each mutant restore;
  unexpected dirt aborts with scene preserved (M5).
  Whitelist narrowed to runtime artifacts only (reports/logs/state/journal);
  harness sources dirty → abort; rename both-sides checked (M6).
- **TODO (optional hardening)**: isolate each mutant in a temporary git worktree
  so product files in the main tree never go dirty even mid-apply. Journal restore
  is the v0 simple path; worktree isolation would remove the remaining race with
  concurrent human edits.
