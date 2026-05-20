---
description: Rex5 network upgrade — SequencerRegistry with dual roles, dynamic system address, Oracle v2.0.0, KeylessDeploy trailing-bytes rejection and sandbox resource accounting, caller-account update deduplication, precompile compute-gas correction and cap, EIP-7702 metering fixes, SELFDESTRUCT beneficiary accounting, system-tx chain-id and nonce validation, deferred final Mega-side gas validation, KeylessDeploy error ABI refactor, deposit-caller new-account accounting, CALL_STACK_LIMIT depth gate for system contract interceptors, oracle hint admission and metering, zero-copy interceptor selector probe, and CALLCODE new-account storage gas metering fix.
---

# Rex5 Network Upgrade

This page is an informative summary of the Rex5 specification.
For the full normative definition, see the Rex5 spec in the mega-evm repository.

## Summary

Rex5 introduces the `SequencerRegistry` system contract, which tracks two independent roles: the **system address** (Oracle/system-tx authority) and the **sequencer** (mini-block signing key).
It also upgrades the Oracle contract to v2.0.0 to read its authority from the registry.

Rex5 also tightens KeylessDeploy validation by rejecting signed inner transaction encodings with trailing bytes.

Rex5 corrects a resource-accounting bug where the caller-account update was overcounted whenever a contract performed multiple value-transferring sub-calls or contract creations from the same call frame.

Rex5 additionally corrects a `CALLCODE` storage-gas metering bug: new-account storage gas is now charged against the caller's storage context rather than the code-source address.

Rex5 closes additional resource-accounting gaps identified in an external audit.
The most significant change is that [KeylessDeploy](../system-contracts/keyless-deploy.md) sandbox execution now propagates its resource consumption back to the parent transaction, preventing low-cost state bloat via unmetered sandbox work.
Rex5 also corrects [compute gas](../glossary.md#compute-gas) recording for failed precompile calls, adds [state growth](../evm/resource-accounting.md#state-growth) tracking for EIP-7702 authority accounts, uses non-delegating account inspection for [storage gas](../glossary.md#storage-gas) metering, and charges new-account costs when `SELFDESTRUCT` creates a beneficiary account.

Rex5 further hardens several consensus-visible boundaries: precompile calls are now bounded by the remaining compute-gas budget, deposit transactions that materialize the caller account pay the new-account storage gas and contribute to state growth, system-contract interceptor dispatch respects `CALL_STACK_LIMIT`, and oracle hint forwarding requires positive `gas_limit` and meters the hint payload against the data-size budget.

Alongside Rex5, the system-contract interceptor dispatch for `AccessControl`, `LimitControl`, and the selector-prefix of `KeylessDeploy` adopts a zero-copy selector probe (reads only the four selector bytes from shared memory) that applies uniformly across all specs.
This is a host-side allocation change only; admission decisions remain bit-identical to the historical `abi_decode` dispatch on every spec, so consensus-visible behavior for Rex4 and earlier is unchanged.

All consensus-visible changes are gated on the Rex5 spec.
The cross-spec selector probe is the sole carve-out and is explicitly justified in section 19.

## What Changed

### 1. SequencerRegistry System Contract

A new system contract at `0x6342000000000000000000000000000000000006`.
It tracks two independent roles, each with its own change lifecycle.

Key methods:

- `currentSystemAddress()` — returns the current system address (Oracle/system-tx sender).
- `currentSequencer()` — returns the current sequencer (mini-block signing key).
- `systemAddressAt(blockNumber)` / `sequencerAt(blockNumber)` — historical role lookups.
- `scheduleNextSystemAddressChange(...)` / `scheduleNextSequencerChange(...)` — admin schedules a change for either role.
- `applyPendingChanges()` — permissionless; applies both roles atomically as a pre-block system call.
- `admin()` / `pendingAdmin()` / `transferAdmin(newAdmin)` / `acceptAdmin()` — two-step admin handoff. `transferAdmin` only sets `_pendingAdmin`; the new admin must call `acceptAdmin` for the change to take effect, preventing single-step lockouts from a mistyped or phished address.

Initial storage is seeded at deploy time.
The initial system address is fixed to `MEGA_SYSTEM_ADDRESS` and is not configurable on `SequencerRegistryConfig`; the initial sequencer and admin come from the chain's `SequencerRegistryConfig`.
No constructor is executed.

### 2. Dynamic System Address

The system address used for system transaction identification and Oracle gas detention exemption is no longer a hardcoded constant.
It is resolved per block from `SequencerRegistry._currentSystemAddress` after all pre-block changes are committed.

Changing the sequencer does NOT affect the system address, and vice versa.

### 3. Oracle v2.0.0

The Oracle contract's `onlySystemAddress` modifier now reads from `SequencerRegistry.currentSystemAddress()` instead of using a constructor `immutable`.
This enables system address change without redeploying the Oracle.

All other Oracle functionality (`sendHint`, `multiCall`, `getSlot`, `setSlot`, etc.) is preserved from v1.1.0.

From Rex5, in-place Oracle bytecode upgrades no longer mark the Oracle account as newly created, so any Oracle storage accumulated before the upgrade is preserved across the transition.
This differs from pre-Rex5 upgrades, which cleared existing Oracle storage.

### 4. Pre-Block Role Change

Pending role changes are applied during `pre_execution_changes` via a single pre-block EVM system call to `SequencerRegistry.applyPendingChanges()`.
This follows the same pattern as EIP-2935 and EIP-4788.
The system call is only issued when a Rust-side pre-check confirms any role change is due.
Like the REX5-updated EIP-2935 / EIP-4788 pre-block calls, this system call is issued with `max(block.gas_limit, 30_000_000)` instead of revm's upstream-fixed 30M `gas_limit`.
This is required because the role-rotation slot writes are charged by REX dynamic storage gas, so their cost is no longer guaranteed to fit within a fixed 30M budget on activation blocks.

### 5. KeylessDeploy Trailing-Bytes Rejection

**Previous behavior (Rex4 and earlier):**
The `keylessDeploy` interceptor decoded the inner pre-EIP-155 transaction RLP without rejecting trailing bytes after the signed payload.
Encodings with trailing data were accepted as long as the leading bytes formed a valid `TxLegacy`.

**New behavior (Rex5):**
The decoder MUST reject any encoding that contains bytes after the signed RLP payload by reverting with `MalformedEncoding()`.
This tightens validation so that two distinct byte strings cannot both pass as the "same" inner deployment transaction.

### 6. CALLCODE New-Account Storage Gas Metering

**Previous behavior (Rex4 and earlier):**
The storage-gas wrapper for `CALLCODE` charged new-account storage gas against the stack `to` address — i.e. the code-source address.
For `CALLCODE`, however, execution happens in the caller's account context: the stack `to` only selects which code to load, while the storage / account context remains the caller's.
Charging new-account storage gas against the code-source address can therefore charge spuriously when the code-source is empty.

**New behavior (Rex5):**
The new-account emptiness check and the `new_account_storage_gas(...)` charge are performed against the current frame's storage account (`interpreter.input.target_address()` — the caller / executing contract).
The stack `to` continues to be used solely as the code-source for the underlying `CALLCODE` instruction.
Pre-Rex5 specs preserve the (frozen) prior behavior for backward compatibility.
`CALL` semantics are unchanged: the stack `to` is still the value recipient and is the correct address for new-account metering.

### 7. Caller-Account Update Deduplication (Data Size and KV Updates)

**Previous behavior (Rex4 and earlier):**
When a call frame performed a value-transferring `CALL` / `CALLCODE` or a `CREATE` / `CREATE2`, the implementation charged the _caller_ account update to the child frame's discardable budget.
However, the parent frame's `target_updated` flag was never marked after the first charge.
As a result, every subsequent value-transferring sub-call or create from the same parent frame re-charged the caller account, overcounting both data-size bytes and KV-update counts for the caller.

**New behavior (Rex5):**
After charging the first caller-account update within a parent call frame, the frame's `target_updated` flag is marked.
All subsequent value-transferring sub-calls and creates from the same parent frame no longer re-charge the caller account.
Each distinct callee or created account is still counted independently.
The discardable-on-revert mechanic is unchanged: charges recorded inside a child frame that reverts are still dropped.

### 7. KeylessDeploy Sandbox Resource Accounting

#### Previous behavior

- The sandbox created an isolated `AdditionalLimit` tracker.
- After sandbox execution, only `EvmState` was merged into the parent transaction.
- The sandbox's resource consumption (standard gas, compute gas, data size, KV updates, state growth) was discarded.
- Both sandbox success and sandbox execution failure merged state unconditionally.

#### New behavior

- Before sandbox execution starts, the sandbox receives its own `AdditionalLimit` tracker with transaction limits capped to the parent transaction's remaining resource budgets.
- The sandbox's transaction limits are derived from the parent's active `EvmTxRuntimeLimits` so any custom `block_env_access_compute_gas_limit` or `oracle_access_compute_gas_limit` configured on the parent is preserved.
- Before sandbox execution starts, known sandbox intrinsic tx-level usage is preflighted against the parent's remaining resource budgets.
- If that intrinsic usage does not fit, sandbox execution is not started and the outer call reverts with `ParentBudgetExceeded`.
- The sandbox's resource usage is extracted before the sandbox context is dropped.
- The parent transaction's `AdditionalLimit` is updated with the sandbox's usage via `merge_usage`.
- The parent's `gas_limit_override` is capped to the parent's remaining gas before sandbox execution starts.
- In the normal case the sandbox fails internally inside the sandbox envelope rather than succeeding and being rejected later by the parent.
- After sandbox completion, sandbox state is merged and the signer continues to pay sandbox gas through the merged state on both sandbox success and frame-local sandbox failure paths.
- On the residual overflow path, where a single-opcode overshoot at a TX-level compute-gas check or a future un-preflighted tx-level persistent accounting path pushes the merged usage above the parent's envelope, the node charges the sandbox's EVM gas to the parent gas meter, rescues the remaining outer gas for refund, and halts the outer call with `OutOfGas`.
- No sandbox state is merged on this path, matching the standard "halted transactions commit only pre-execution state" revm convention.

### 8. Precompile Compute Gas on Error Paths

#### Previous behavior

- Compute gas for precompile calls was recorded via `output.gas.spent()`.
- On error paths (`PrecompileOOG`, `PrecompileError`), revm does not call `record_cost()`, so `spent()` returned 0.
- The forwarded gas was fully consumed from the parent's EVM gas meter but recorded as 0 compute gas.

#### New behavior

- On error paths, compute gas is recorded as `output.gas.limit()` (the full forwarded amount), matching the EVM gas actually consumed.
- On success and revert paths, compute gas continues to use `output.gas.spent()`.
- **Exception (Mega fixed-cost precompiles).** When the failed precompile is a Mega-overridden fixed-cost precompile and the wrapper's `gas_limit < fixed_cost` pre-check has passed (so upstream verification or computation ran), the recorded compute gas is the fixed cost, not the forwarded `output.gas.limit()`. Today this exception applies only to KZG point evaluation (`0x0a`, fixed cost `KZG_POINT_EVALUATION_GAS_COST = 100_000`). EVM-gas burn on the same path is unchanged: the call still halts with `PrecompileError` and the parent loses the forwarded amount.

### 9. EIP-7702 Authority State-Growth Tracking

#### Previous behavior

- `DataSizeTracker` and `KVUpdateTracker` charged for EIP-7702 authority accounts in `before_tx_start()`.
- `StateGrowthTracker` had no `before_tx_start()` override and did not count fresh authority accounts.

#### New behavior

- `StateGrowthTracker` counts only valid EIP-7702 authorizations whose authority account is a previously non-existent state entry.
- This accounting happens during pre-execution after the authority account has been loaded from the journal and before the authorization list mutates delegation bytecode.
- Existing authority accounts do not count toward state growth.
- Authorization entries skipped by chain ID, nonce, authority recovery, or incompatible existing code do not count toward state growth.

### 10. Non-Delegating Account Inspection for Metering

#### Previous behavior

- CALL and CREATE storage-gas wrappers used `inspect_account_delegated()`, which follows EIP-7702 delegation and returns the delegate's account state.
- When a delegate was empty, the authority was incorrectly charged the new-account storage-gas premium.
- For CREATE, the delegate's nonce was used to compute the created address, causing the SALT bucket lookup to query the wrong address.
- `StateGrowthTracker::before_frame_init` also used delegated inspection for the CALL emptiness check.

#### New behavior

- CALL and CREATE storage-gas wrappers use non-delegating `inspect_account()` to read the authority's own state.
- `StateGrowthTracker::before_frame_init` uses non-delegating inspection for the CALL emptiness check.
- A new `inspect_account` method on the `JournalInspectTr` trait provides non-delegating account inspection.

### 11. SELFDESTRUCT Beneficiary New-Account Metering

#### Previous behavior

- `SELFDESTRUCT` to an empty beneficiary created a new account without any MegaETH-specific charges.
- No storage-gas premium, data size, KV update, or state growth was recorded for the new beneficiary.

#### New behavior

- A `storage_gas_ext::selfdestruct` instruction wrapper charges the new-account storage-gas premium and records data size (+40 bytes), KV update (+1), and state growth (+1) when `SELFDESTRUCT` sends value to an empty beneficiary.
- Zero-balance `SELFDESTRUCT` does not trigger new-account charges (no value transfer means no account creation).

### 12. System Transaction Chain-Id and Nonce Validation

**Previous behavior (Rex4 and earlier):**
A legacy transaction whose signer is `MEGA_SYSTEM_ADDRESS` was promoted to an OP-style deposit transaction before ordinary validation ran, bypassing signature, chain-id, nonce, balance, and fee checks.
A captured raw system transaction could in principle be replayed against any chain configuration that accepted the same byte string.

**New behavior (Rex5):**
Before the deposit promotion, a node MUST validate the system transaction's chain-id and nonce against the same canonical rules ordinary user transactions follow:

- `chain_id` MUST be present and MUST equal the node's configured chain id (subject to `cfg.tx_chain_id_check`).
- `nonce` MUST equal `state.nonce(MEGA_SYSTEM_ADDRESS)` (subject to `cfg.disable_nonce_check`).
- If `MEGA_SYSTEM_ADDRESS` carries code, the EIP-3607 check applies (subject to `cfg.disable_eip3607`).

A failure surfaces as a canonical `InvalidTransaction` variant before any state mutation.
Signature, balance, and fee bypasses are preserved once the checks pass.

The `CfgEnv` toggles are honored so the system-tx validate path stays symmetric with the canonical user-tx validate path for debug, state-test, and replay tooling.
This is correctness recovery, not new defense in depth: OP deposits get away with bypassing these checks because L1 derivation plus per-deposit `source_hash` uniqueness provides higher-layer replay protection that MegaETH system transactions do not have.
See [system-tx.md](../system-contracts/system-tx.md) for the normative specification.

### 13. Final Mega-Side Gas Validation Ordering

**Previous behavior (Rex4 and earlier):**
The intrinsic-gas check (`initial_gas > gas_limit`) fired after MegaETH calldata storage gas was added but before CREATE / new-callee account storage gas.
A transaction whose final Mega-adjusted `initial_gas` exceeded `gas_limit` only after those later storage gas contributions produced an `ExecutionResult::Halt` with `gas_used == gas_limit` — sender debited, nonce bumped, no call effects.

**New behavior (Rex5):**
The check is deferred until after every Mega-side intrinsic and dynamic storage gas contribution has been added.
A transaction that cannot fit its final Mega-side intrinsic or floor gas requirement is rejected as a canonical validation error (`InvalidTransaction::CallGasCostMoreThanGasLimit` or `InvalidTransaction::GasFloorMoreThanGasLimit`) before `pre_execution()` runs, leaving sender balance and nonce untouched.

### 14. KeylessDeploy Error ABI Refactor

**Previous behavior (Rex4 and earlier):**
`IKeylessDeploy::InternalError(string message)` carried a stringified upstream revm/op-revm error as its ABI payload.

**New behavior (Rex5):**

- `InternalError(string)` becomes `InternalError()` (selector-only).
  The `message` field is dropped from the wire; root cause is reported off-chain via node logs.
- A new selector-only error `InvalidTransaction()` is added to the stable validation error set, raised when the sandbox `MegaHandler::validate` rejects the inner transaction before `pre_execution()` runs (typically section 13's final gas check, but structurally any `IsTxError::is_tx_error() == true` outcome, including op-revm's deposit `catch_error` synthesised halts).
  The outer KeylessDeploy call MUST revert with `InvalidTransaction()` and the signer MUST NOT be charged in this path.
- A new error `InitCodeTooLarge(uint64 size, uint64 max)` is added.
  It is raised when the inner transaction's initcode length exceeds `cfg.max_initcode_size()`; the sandbox enforces this because the deposit-style sandbox transaction (see section 23) bypasses op-revm's `validate_env`, where revm's EIP-3860-style size check normally lives.
  `size` is the actual initcode length; `max` is the configured cap.
- A new selector-only error `SignerHasCode()` is added.
  It is raised when the recovered signer's parent-state bytecode is non-empty and is not a valid EIP-7702 delegation designation, unless `cfg.disable_eip3607` is set.
  This re-enforces EIP-3607 because the deposit-style sandbox transaction bypasses op-revm's `validate_account_nonce_and_code`.

`InvalidTransaction()`, `InternalError()`, and `SignerHasCode()` are selector-only because precompile return data is reachable on-chain via `RETURNDATACOPY` → `SSTORE` → state root, so coupling the wire format to a non-stability upstream `Display` impl would pin consensus to those impls.
See [keyless-deploy.md](../system-contracts/keyless-deploy.md) for the normative error list.

### 15. Precompile Compute-Gas Cap

**Previous behavior (Rex4 and earlier):**
A precompile invocation forwarded the caller's `gas_limit` to the precompile unchanged.
Compute gas was recorded post-hoc from the precompile's reported spent gas.
A precompile whose natural cost exceeded the remaining compute-gas budget would still execute fully, and the overshoot was only detected after the fact.

**New behavior (Rex5):**
The gas forwarded into a precompile is capped at `min(call_gas_limit, current_call_remaining_compute_gas)`.
A precompile whose minimum cost exceeds the cap MUST return `PrecompileOOG` without performing the computation.
On successful or reverting precompile returns, the caller-visible `Gas` object is normalized so that the caller's refund accounting reflects the original forwarded `gas_limit` minus the precompile's actual spent.
Halt-class returns (`PrecompileOOG` and `PrecompileError`) preserve the underlying precompile's `Gas` shape unchanged, because halt paths do not consume the `remaining` field in caller refund.

### 16. Deposit Caller New-Account Accounting

**Previous behavior (Rex4 and earlier):**
A deposit-like transaction (Optimism deposit or mega-system deposit-marked legacy) whose caller account was empty at validation time could materialize the caller via `mint` balance increment or pre-execution nonce bump.
The materialization did not pay `new_account_storage_gas(caller)` and did not contribute to `state_growth`.

**New behavior (Rex5):**
For every deposit-like transaction whose caller account is empty at the pre-pre-execution snapshot, a node MUST add `new_account_storage_gas(caller)` to the transaction's intrinsic gas and MUST record exactly one new-account event (`+1`) on the `state_growth` TX intrinsic lane.
The recording is single-shot — a subsequent transaction whose caller is the now-non-empty account incurs no additional charge.
Where the same address appears as both caller and callee on a value-transferring `TxKind::Call` deposit, the existing callee-side `new_account_storage_gas` charge is the single gas charge and the deposit-caller path records only the state-growth event without re-charging the gas.

The `data_size` and `kv_update` lanes are unaffected by this rule.
Their existing `before_tx_start` charges already account for the caller's account-info write on every transaction.

### 17. CALL_STACK_LIMIT Depth Gate for System Contract Interceptors

**Previous behavior (Rex4 and earlier):**
System contract interceptor dispatch in `frame_init` ran before revm's own `depth > CALL_STACK_LIMIT` check, which lives inside `make_call_frame`.
A call to a system contract from a frame whose depth exceeded the limit could therefore receive a synthetic interceptor result instead of `CallTooDeep`.

**New behavior (Rex5):**
For any `FrameInput::Call(call_inputs)` with `scheme ∈ {Call, StaticCall}` whose `frame_init.depth > CALL_STACK_LIMIT`, a node MUST short-circuit `frame_init` with a synthetic `CallTooDeep` frame result and `Gas::new(call_inputs.gas_limit)` (no spend, fully refundable to caller via `erase_cost`) BEFORE consulting any system contract interceptor.
No interceptor side effects (volatile-data tracker mutation, oracle hint forwarding, keyless deploy) MUST be performed in the too-deep case.

A TX-level additional-limit exceed detected by `frame_result_if_exceeding_limit` takes priority over this depth gate.
The `inspect_frame_init` path mirrors the same ordering: exceeded-limit check, then depth gate, then any inspector-provided synthetic output.

`CallCode`, `DelegateCall`, and `FrameInput::Create` are out of scope for this gate.
Their depth checks remain in revm's `make_call_frame` / `make_create_frame`.

### 18. Oracle Hint Admission and Metering

**Previous behavior (Rex4 and earlier):**
The oracle `sendHint(bytes32 topic, bytes data)` interceptor invoked `OracleEnv::on_hint(caller, topic, data)` whenever the calldata decoded successfully, regardless of the call's `gas_limit`.
A call with `gas_limit = 0` would forward the hint to the off-chain oracle backend before the on-chain Oracle frame ran out of gas.
The hint payload was not charged against any transaction-level budget.

**New behavior (Rex5):**
A node MUST forward a hint to the off-chain backend via `OracleEnv::on_hint` only when all of the following hold:

1. The call's `gas_limit > 0`.
2. The leading four bytes of the calldata match `IOracle::sendHintCall::SELECTOR`.
3. The full calldata decodes as a valid `sendHint(bytes32 topic, bytes data)` invocation.
4. The recording of `data.len() + 32` bytes against the TX `data_size` intrinsic lane keeps `data_size_used` within the configured TX `data_size` limit.

When any of (1)–(4) fails, the interceptor MUST NOT invoke `on_hint`.
A failure of (4) MUST cause the transaction to halt with the canonical TX-level data-size `OutOfGas` failure (matching every other data-size overflow); the failure does not introduce a new failure shape.
A failure of (1) or (3) lets the call fall through to the on-chain Oracle bytecode for canonical handling.

The 32-byte addend in the recorded bytes accounts for the fixed `bytes32 topic`, which is a user-controlled value that flows to the off-chain backend on every accepted hint alongside `data`.
The 20-byte caller address that also flows out is not added because it is EVM call-frame metadata, already covered by the transaction's intrinsic data-size cost.

### 19. Zero-Copy Interceptor Selector Probe

**Scope:** Applied uniformly across all specs for `AccessControl`, `LimitControl`, and the selector-prefix of `KeylessDeploy`.
The `OracleHint` interceptor keeps a Rex5-only selector probe because its branch bundles observable behavior changes (see section 18) that must remain spec-gated.

**Previous behavior:**
Each system contract interceptor began with `call_inputs.input.bytes(ctx)`, which for `CallInput::SharedBuffer` copies the full `argsSize` range out of shared memory on every dispatch attempt — including for selectors that ultimately do not match.

**New behavior:**
A node MUST decide whether to admit a system-contract call into the interceptor handler based on the four-byte selector alone.
Only the head four bytes of the calldata MAY be read from shared memory at admission time.
The full calldata MUST be materialized only after the selector matches and the interceptor needs the argument decoding.

Admission rules are NOT tightened.
A call whose calldata is a four-byte known selector followed by arbitrary trailing bytes is still accepted, matching the historical admission semantics of `SolCall::abi_decode` for parameterless calls (`AccessControl`, `LimitControl`) and the parametrized admission for `KeylessDeploy` (selector match + full `abi_decode` of `(bytes, uint64)`).
This change is observable only to off-chain tooling that measures host-side allocation, not to consensus.

**Why this is consensus-safe across stable specs:**
For the three affected interceptors, the selector probe produces bit-identical admission decisions to the historical `abi_decode`-based dispatch.
For parameterless methods (`AccessControl`, `LimitControl`), this relies on alloy's `decode_sequence::<()>` returning `Ok(())` for selector plus any trailing bytes; for `KeylessDeploy` both paths still feed the full payload to the same `abi_decode` after the selector matches.
The alloy behavior is pinned by a CI-time assertion so a future upstream tightening fails before it can break replay determinism on stable specs.

### 20. Value-Transfer CALL/CALLCODE Parent Compute-Gas Attribution

**Previous behavior (Rex4 and earlier):**
The per-opcode compute-gas wrapper subtracted the child frame's full `call_inputs.gas_limit` from the parent's recorded compute gas, including the standard EVM `CALL_STIPEND` (2,300) that revm adds to value-transferring `CALL` and `CALLCODE` child frames.
Because `CALL_STIPEND` is gas the EVM grants to the child without deducting it from the parent, subtracting the full `gas_limit` (which includes the stipend) under-counted the parent's contribution by exactly `CALL_STIPEND` per value-transferring `CALL` / `CALLCODE` invocation.

**New behavior (Rex5):**
For a `NewFrame(FrameInput::Call(call_inputs))` action where `call_inputs.scheme ∈ {Call, CallCode}` and `call_inputs.transfers_value()` is true, a node MUST subtract `call_inputs.gas_limit − CALL_STIPEND` (the parent-contributed forwarded portion) from the parent's recorded compute gas, not the raw `call_inputs.gas_limit`.
`DelegateCall`, `StaticCall`, and any `CALL` / `CALLCODE` with `value == 0` MUST continue to subtract the raw `call_inputs.gas_limit` — those frames receive no `CALL_STIPEND`.
`FrameInput::Create` is unaffected; CREATE / CREATE2 have no value-transfer stipend.

Pre-Rex5 specs MUST continue to subtract the raw `call_inputs.gas_limit` for byte-for-byte replay parity of historical blocks.
The under-counting is intrinsic to the stable-spec behavior and is preserved deliberately.

### 21. `STORAGE_CALL_STIPEND` Separated-Allowance Model

**Previous behavior (Rex4 and earlier):**
Value-transferring internal `CALL` / `CALLCODE` frames receive `STORAGE_CALL_STIPEND` (23,000) added to the child's `gas_limit` so the callee can pay the Mega 10× storage-gas costs (LOG topics/data, new-account materialization, first-time-write SSTORE, CREATE contract-storage, SELFDESTRUCT beneficiary creation) when the caller forwards little or no gas.
The extra gas is restricted to storage-gas usage by a post-hoc per-frame compute-gas cap pinned at the pre-stipend gas limit; unused stipend is burned on frame return by clamping the child's `gas.remaining()` to the pre-inflation limit.

Because the per-frame compute cap is enforced after each opcode finishes, a single expensive opcode or precompile invocation in the child can spend stipend gas as compute and have its full cost recorded into the parent's compute-gas counter before the cap triggers the frame-local revert.
Repeated value-transferring CALLs from the same parent can amplify recorded compute gas beyond what the transaction's compute-gas limit would otherwise allow.

**New behavior (Rex5):**
`STORAGE_CALL_STIPEND` becomes a per-frame allowance internal to the resource tracker.
The child's `call_inputs.gas_limit` MUST NOT be inflated by `STORAGE_CALL_STIPEND` on Rex5.
For each of the five Mega-introduced storage-gas surcharge sites — CALL/CALLCODE empty-account new-account, CREATE/CREATE2 contract-creation, SSTORE first-time-write zero-to-nonzero, LOG topics-and-data, and SELFDESTRUCT empty-beneficiary new-account — a node MUST drain up to `STORAGE_CALL_STIPEND` from the current frame's allowance and charge only the residual `surcharge − drained` against the EVM `gas` object.
Charging sites that surface `Option<u64>` (LOG) MUST preserve the `None` (overflow) arm unchanged so the existing overflow halt behavior is identical to pre-Rex5.

Because the allowance never enters `gas.limit()`, it cannot be spent on compute opcodes by construction.
On frame return there is nothing to burn — `gas.remaining()` already reflects only parent-contributed gas — and the rescued-gas path naturally excludes the allowance with no special arithmetic.

The Mega per-frame compute-gas cap (`cap_current_frame_limit`) is unused on Rex5.
It remains in place under stable specs so the legacy inflation path retains its byte-for-byte behavior, but the Rex5 separated-allowance path bypasses it.

**Scope.** The allowance covers ONLY Mega-introduced storage-gas surcharges, not standard EVM opcode gas.
A child whose forwarded `gas_limit` plus `CALL_STIPEND` does not cover the standard EVM cost of the opcode it executes (SSTORE base 22,100, CREATE2 frame setup, SELFDESTRUCT base) will OOG normally regardless of the allowance balance.
The unique case where end-to-end success is achievable at forwarded `gas = 0` is LOG1 with small payloads, because LOG1's standard EVM cost (~750) fits inside the 2,300 `CALL_STIPEND`.

The allowance applies to internal (depth > 0) value-transferring `CALL` / `CALLCODE` only.
Top-level transactions, DELEGATECALL / STATICCALL, CREATE / CREATE2 (which never grant the value-transfer stipend), and any value-zero CALL do not receive an allowance.

Pre-Rex5 specs MUST retain the legacy inflation, per-frame compute cap, and burn-on-return for byte-for-byte replay parity.

**Cross-reference:**

- The Rex4 introduction of `STORAGE_CALL_STIPEND` is documented in `docs/spec/upgrades/rex4.md` section 5 and `docs/spec/evm/gas-forwarding.md`.
- The Rex5 separated-allowance model refines the same stipend semantically without changing the 23,000 grant amount or the value-transfer admission rule.

### 22. KeylessDeploy Sandbox Volatile-Access Footprint Merge

#### Previous behavior

- The KeylessDeploy sandbox created an isolated `VolatileDataAccessTracker`.
- After sandbox execution, the sandbox's accumulated volatile-access bitmap was discarded along with the sandbox `MegaContext`.
- The parent transaction's `VolatileDataAccessTracker::get_volatile_data_info()` therefore reported only the volatile-access bits set outside the sandbox, even if the sandbox's constructor code accessed `TIMESTAMP`, `COINBASE`, the Oracle contract, the beneficiary balance, or other volatile data.

#### New behavior

- The sandbox's accumulated `volatile_data_accessed: VolatileDataAccess` bitmap is extracted from the sandbox `MegaContext` before that context is dropped.
- Immediately after `merge_sandbox_limit_usage` and before the unused-reservation refund (`refund_unused_sandbox_gas`), the sandbox's bitmap is unioned into the parent's `VolatileDataAccessTracker` via `merge_accesses_from_bitmap`.
- The merge runs on every path where the sandbox actually executed — sandbox success, in-sandbox failure, and the post-merge residual-overflow halt — so the parent transaction's reported footprint reflects what the sandbox accessed regardless of the outer call's final outcome.
- Only the access bitmap is merged. The detention cap (`compute_gas_limit`), the disable-state pointer (`disable_depth`), and the configured per-spec limits (`block_env_access_limit`, `oracle_access_limit`) are deliberately not merged.

#### Footprint-only semantics — no halt-reason remap

The merge propagates the _footprint_ (which volatile data was accessed) but NOT the detention enforcement state. Specifically, the receiver's `AdditionalLimit.compute_gas.detained_limit` is unchanged by this merge — sandbox-internal `set_compute_gas_limit` calls remain sandbox-internal.

`MegaHandler::execution_result` remaps a generic exceeding-limit halt into a volatile-specific variant (e.g. `VolatileDataAccessOutOfGas`) only when the parent's `compute_gas.is_detained_exceed()` returns true. That state is established by `AdditionalLimit::set_compute_gas_limit`, which is invoked from volatile-aware instruction wrappers during the parent frame's own execution. A sandbox-only volatile access therefore does NOT, by itself, cause the parent's residual-overflow halt to be remapped — it remains the generic `ComputeGasLimitExceeded` variant.

This scoping is intentional. Detention is a frame-local enforcement device; for the keyless-deploy depth==0 invariant the parent has no further work after the sandbox returns, so propagating the detention state would have no observable benefit but would surface as a consensus-visible behavior change in halt reasons. The remap path remains available to any future caller that needs volatile-aware halt classification via the standard `set_compute_gas_limit` API.

### 23. KeylessDeploy Sandbox Outer EVM Gas Debit

#### Previous behavior

- The outer keyless-deploy call's `Gas` counter was debited only the fixed dispatch overhead (`KEYLESS_DEPLOY_OVERHEAD_GAS`, 100K).
- The sandbox's `gas_used` was reported only inside the ABI-encoded `IKeylessDeploy::keylessDeployReturn` payload; the outer `Gas` was not `record_cost`d with it.
- Consequence: the outer transaction's receipt `gasUsed` and the block header's `gas_used` field did NOT reflect computation performed inside the sandbox. The multidimensional limits (compute gas, data size, KV updates, state growth) WERE correctly accounted via `merge_usage` after the round-1 sandbox-accounting fix, so block-level multidim caps still bounded sandbox cost; but the legacy EVM-gas channel was blind to it.

#### New behavior

- The sandbox transaction runs under revm's standard message-call gas shape applied to the outer `Gas` counter: pre-debit the capped `gas_limit_override` on entry (`gas.record_cost(gas_limit_override)`), refund the unused portion on exit (`gas.erase_cost(gas_limit_override - sandbox_gas_used)`).
- Net effect on every sandbox-completion path (`SandboxOutcome::Success` and in-sandbox `SandboxOutcome::Failure`): the outer call's `Gas` counter is debited by exactly `sandbox_gas_used`, the same as if revm had run a normal CALL of that cost.
- If the sandbox bails before producing a `SandboxOutcome` (validate-reject / internal error), the full reservation is refunded; the upfront `KEYLESS_DEPLOY_OVERHEAD_GAS` and pre-sandbox materialization charges are retained.
- The inner signer's balance is NOT debited for sandbox gas: the sandbox runs as an OP deposit-like transaction with `gas_price = 0`, so the deposit-path caller balance escrow degenerates to zero (see "Outer-only billing" below).
  The inner signer only ever loses `value` (zero for canonical Nick's-Method deployers).
- Result: the outer transaction's receipt `gasUsed` and the block header `gas_used` now reflect actual computation; the legacy EVM-gas channel is consistent with the multidim channel.

**Outer-only billing.** The sandbox transaction runs as an OP deposit-like transaction: `deposit.source_hash` is set to a sandbox-specific marker (`SANDBOX_TX_SOURCE_HASH`), `gas_price` is forced to zero, and `deposit.mint` is held at `None`.
The source hash makes op-revm treat the tx as a deposit, which disables L1 fee, operator fee, `validate_env` (signature, nonce, configured initcode size limit, chain id, etc.), the caller balance sufficiency check, and `reward_beneficiary` distribution.
The zero gas price is required because the deposit branch of `validate_against_state_and_deduct_caller` still computes a caller balance escrow of `gas_limit * effective_gas_price + additional_cost`; with `additional_cost = 0` (L1/operator skipped) and `gas_price = 0`, the escrow is zero and the inner signer's balance is left untouched.
The outer transaction's own fee model is the sole fee source.
For a normal outer transaction the outer sender pays `(dispatch_overhead + caller_materialization_storage_gas + sandbox_gas_used) * outer_gas_price` via the outer `Gas` counter (a pre-debited reservation that revm-style refunds the unused tail) and the standard EIP-1559 / OP fee split; deposit and system outer transactions inherit their existing fee-free semantics.

**GASPRICE in sandbox.**
A direct consequence of setting the sandbox tx's `gas_price` to zero is that the `GASPRICE` opcode executed inside the constructor / init code observes `0`, regardless of the gas price encoded in the keyless transaction signature.
Initcode authors targeting Rex5+ KeylessDeploy MUST assume `GASPRICE == 0` inside the sandbox.
The canonical Nick's-Method deployers in widespread use (e.g. the Arachnid CREATE2 deployer) do not read `GASPRICE`, so this behavior change has no practical effect on existing deployments.
The change is consensus-observable but intentional: preserving the signed gas price inside the sandbox would re-introduce the caller balance escrow that fee-free mode exists to eliminate.

**Deposit-style validation handling.** op-revm's deposit `catch_error` swallows a tx-validation failure into `Ok(Halt(FailedDeposit, gas_used = gas_limit))` with a nonce bump applied inside the sandbox journal. `process_sandbox_transact_result` detects the `FailedDeposit` halt reason and remaps it to `KeylessDeployError::InvalidTransaction`, preserving the existing contract that a sandbox validate-reject does NOT consume the Nick's-Method replay barrier: the outer keyless-deploy call surfaces as `Revert`, the sandbox state is discarded (so the `catch_error` nonce bump is never merged into the parent), and the pre-debited `gas_limit_override` reservation is fully refunded to the outer gas counter.
The upfront `KEYLESS_DEPLOY_OVERHEAD_GAS` and pre-sandbox caller materialization charges remain debited — paying for the dispatch work and the parent-state read that actually occurred, in the same shape as the dispatch overhead.

**Caller materialization accounting.**
Because the sandbox-internal `validate_against_state_and_deduct_caller` no longer touches the signer under deposit-style fee-free mode, the standard Rex5 deposit-caller storage gas branch in `MegaHandler::validate` is gated off inside the sandbox (`is_inside_sandbox()`).
The materialization charge is performed instead by `charge_caller_materialization_pre_sandbox`, which runs before the sandbox transaction is constructed and reads the parent journal-visible state (preferring the journal cache, then falling back to the backing database — neither path goes through the sandbox's `SandboxDb` nonce override).
When the parent-visible signer is empty, the outer `Gas` counter is debited by `new_account_storage_gas(deploy_signer)` and a deposit-caller state-growth event is recorded.
On retry the parent-visible signer is already non-empty (nonce was bumped by the previous deploy's `make_create_frame`), so the charge does not fire a second time.
The charge is paid upfront — alongside `KEYLESS_DEPLOY_OVERHEAD_GAS` — and is retained regardless of sandbox outcome: sandbox-validate-reject, in-sandbox revert, and post-merge residual overflow all leave the materialization charge in place, mirroring the upfront dispatch overhead.
DB read failure and dynamic storage-gas computation failure return `Err(KeylessDeployError::InternalError)`; the sandbox is not started in that case.

**Relayer pricing impact.**
Pre-Rex5, the relayer's marginal outer-gas cost for invoking KeylessDeploy was `~100K * outer_gas_price` (the dispatch overhead alone).
Under Rex5 the marginal cost becomes `(~100K + sandbox_gas_used + caller_materialization_storage_gas) * outer_gas_price`, where `caller_materialization_storage_gas` is zero on retries and on warm-bucket signers.
For compute-heavy keyless deploys this can be a 10–100× increase in the relayer-side gas budget relative to Rex4.
Relayers MUST size the outer `gas_limit` to cover the worst-case sandbox cost they are willing to underwrite.
The `gas_limit_override` argument to `keylessDeploy` is already capped to the parent's remaining outer gas under Rex5, so the outer caller has full visibility into the upper bound the sandbox can spend.
The inner signer no longer needs a pre-funded balance to cover sandbox gas — only enough to cover the optional `value` transfer encoded in the keyless transaction (typically zero for Nick's-Method deployers).

## Developer Impact

- Contracts that verify mini-block signatures can use `SequencerRegistry.currentSequencer()` to look up the signing authority.
- Contracts that need historical information can use `systemAddressAt(blockNumber)` or `sequencerAt(blockNumber)`.
- The Oracle contract's write methods (`setSlot`, `emitLog`, etc.) now accept calls from the current system address as reported by `SequencerRegistry`, not from a fixed address.
- KeylessDeploy signed inner transaction encodings with trailing bytes now revert with `MalformedEncoding()`.
- Transactions that perform multiple value-transferring sub-calls or creates from the same contract now report lower data-size and KV-update usage than they did under Rex4.
  This only affects usage tracking; it does not change execution semantics, state transitions, or the base transaction gas model.
- Precompile calls in compute-gas-constrained frames now fail-fast with `PrecompileOOG` instead of running and overshooting the budget. Contracts that catch precompile failures see the same `is_ok() == false` signal but may observe the failure earlier in their gas budget.
- Deposit transactions whose `from` address is empty at the time of validation now require additional intrinsic gas equal to `new_account_storage_gas(caller)` to cover the materialization performed by `pre_execution`.
- Off-chain integrations that rely on `OracleEnv::on_hint` for telemetry will no longer receive hints from calls with `gas_limit = 0`, and will see hints from calls that exceed the transaction's data-size budget dropped at the EVM boundary.
  Backends should not assume idempotency of hints already received before such a transaction halts; partial flushes in earlier successful hints are not rolled back.
- Value-transferring `CALLCODE` no longer charges new-account storage gas based on the code-source address.
  Contracts that previously paid spurious new-account storage gas via `CALLCODE` to an empty address see lower gas usage under Rex5.

**KeylessDeploy callers** should be aware that sandbox execution now counts toward the outer transaction's resource budgets.
A keyless deploy that previously succeeded may now fail if the outer transaction has tight resource limits.
The `gasLimitOverride` parameter is capped to the outer transaction's remaining gas.
The typical failure mode for tight additional-limit budgets is now an internal sandbox execution failure with encoded `errorData`.
If the sandbox's pre-frame intrinsic usage alone exceeds the parent's remaining envelope, sandbox execution is not started and the outer call reverts with `ParentBudgetExceeded`.
If an opcode overshoots a TX-level compute-gas check after sandbox execution has run, the outer call instead halts with `OutOfGas` and no sandbox state is merged; the caller is charged the sandbox's EVM gas through the outer gas meter and unspent gas is rescued for refund.

**Contracts using precompiles** are not affected in practice.
The compute-gas correction only changes accounting for failed precompile calls, which do near-zero real computation.

**Contracts using EIP-7702 delegation** may see slightly different gas costs for CALL and CREATE operations targeting authority accounts, because metering now inspects the authority's own state rather than the delegate's.

**Contracts using SELFDESTRUCT** with value transfer to empty addresses will now pay the new-account storage-gas premium and consume resource-limit budget for the new beneficiary account.

## Safety and Compatibility

- Pre-REX5 behavior is unchanged. The legacy `MEGA_SYSTEM_ADDRESS` constant is used for all pre-REX5 specs.
- `SequencerRegistry` does not have an interceptor. It runs normal on-chain bytecode.
- Both `_currentSystemAddress` and `_currentSequencer` are only updated during pre-block system calls, ensuring block-stability.
- Changing one role does not affect the other.
- Rex4 and earlier retain the original KeylessDeploy trailing-bytes behavior unchanged.
- Rex4 and earlier retain the original caller-account overcounting behavior unchanged.
- Transactions executed under Rex4 or earlier specs produce identical results before and after the Rex5 code is deployed.
- The KeylessDeploy sandbox accounting change is the most visible behavioral difference.
  The upfront budget capping and intrinsic preflight ensure that a successful sandbox run normally fits inside the parent's remaining resource envelope before state is merged.
  The residual overflow path rejects the outer call without merging sandbox state so no partial deployment survives a parent-level reject.
- The precompile compute-gas cap, deposit-caller accounting, `CALL_STACK_LIMIT` depth gate, and oracle hint admission/metering are REX5-only. Rex4 and earlier preserve the pre-fix semantics byte-for-byte so that replay of historical blocks continues to produce identical state roots and receipts.
- The zero-copy selector probe for `AccessControl`, `LimitControl`, and the selector-prefix of `KeylessDeploy` is applied uniformly across all specs (their admission decisions are bit-identical to the historical `abi_decode` path; the difference is host-side allocation, which is not consensus-visible).
- Rex4 and earlier retain the original `CALLCODE` new-account storage gas metering behavior unchanged.
- Rex5 is the current unstable spec under active development; its semantics may still change before network activation.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model
- [SequencerRegistry](../system-contracts/sequencer-registry.md) — system contract specification
- [Oracle](../system-contracts/oracle.md) — Oracle v2.0.0 specification
- [KeylessDeploy](../system-contracts/keyless-deploy.md) — KeylessDeploy specification
- [Resource Accounting](../evm/resource-accounting.md) — caller-account update deduplication
- [Resource limits](../evm/resource-limits.md)
- [Compute gas](../glossary.md#compute-gas)
- [Gas detention](../evm/gas-detention.md)
- [Storage gas](../glossary.md#storage-gas)
