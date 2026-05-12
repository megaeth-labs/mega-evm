---
description: Rex5 network upgrade — SequencerRegistry with dual roles, dynamic system address, Oracle v2.0.0, KeylessDeploy trailing-bytes rejection and sandbox resource accounting, caller-account update deduplication, precompile compute-gas correction, EIP-7702 metering fixes, SELFDESTRUCT beneficiary accounting, system-tx chain-id and nonce validation, deferred final Mega-side gas validation, and KeylessDeploy error ABI refactor.
---

# Rex5 Network Upgrade

This page is an informative summary of the Rex5 specification.
For the full normative definition, see the Rex5 spec in the mega-evm repository.

## Summary

Rex5 introduces the `SequencerRegistry` system contract, which tracks two independent roles: the **system address** (Oracle/system-tx authority) and the **sequencer** (mini-block signing key).
It also upgrades the Oracle contract to v2.0.0 to read its authority from the registry.

Rex5 also corrects a resource-accounting bug where the caller-account update was overcounted whenever a contract performed multiple value-transferring sub-calls or contract creations from the same call frame.

Rex5 closes additional resource-accounting gaps identified in an external audit.
The most significant change is that [KeylessDeploy](../system-contracts/keyless-deploy.md) sandbox execution now propagates its resource consumption back to the parent transaction, preventing low-cost state bloat via unmetered sandbox work.
Rex5 also corrects [compute gas](../glossary.md#compute-gas) recording for failed precompile calls, adds [state growth](../evm/resource-accounting.md#state-growth) tracking for EIP-7702 authority accounts, uses non-delegating account inspection for [storage gas](../glossary.md#storage-gas) metering, and charges new-account costs when `SELFDESTRUCT` creates a beneficiary account.

All changes are gated on the Rex5 spec.
Behavior for Rex4 and earlier specs is unchanged.

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
Unlike EIP-2935 / EIP-4788, which carry the upstream-fixed 30M `gas_limit`, this system call is issued with `max(block.gas_limit, 30_000_000)`.
This is required because the role-rotation slot writes are charged by REX dynamic storage gas, so their cost is no longer guaranteed to fit within a fixed 30M budget on activation blocks.

### 5. KeylessDeploy Trailing-Bytes Rejection

**Previous behavior (Rex4 and earlier):**
The `keylessDeploy` interceptor decoded the inner pre-EIP-155 transaction RLP without rejecting trailing bytes after the signed payload.
Encodings with trailing data were accepted as long as the leading bytes formed a valid `TxLegacy`.

**New behavior (Rex5):**
The decoder MUST reject any encoding that contains bytes after the signed RLP payload by reverting with `MalformedEncoding()`.
This tightens validation so that two distinct byte strings cannot both pass as the "same" inner deployment transaction.

### 6. Caller-Account Update Deduplication (Data Size and KV Updates)

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

- `InternalError(string)` becomes `InternalError()` (selector-only). The `message` field is dropped from the wire; root cause is reported off-chain via node logs.
- A new selector-only error `InvalidTransaction()` is added to the stable validation error set, raised when the sandbox `MegaHandler::validate` rejects the inner transaction before `pre_execution()` runs (typically section 13's final gas check, but structurally any `IsTxError::is_tx_error() == true` outcome).
  The outer KeylessDeploy call MUST revert with `InvalidTransaction()` and the signer MUST NOT be charged in this path.

Both errors are selector-only because precompile return data is reachable on-chain via `RETURNDATACOPY` → `SSTORE` → state root, so coupling the wire format to a non-stability upstream `Display` impl would pin consensus to those impls.
See [keyless-deploy.md](../system-contracts/keyless-deploy.md) for the normative error list.

## Developer Impact

- Contracts that verify mini-block signatures can use `SequencerRegistry.currentSequencer()` to look up the signing authority.
- Contracts that need historical information can use `systemAddressAt(blockNumber)` or `sequencerAt(blockNumber)`.
- The Oracle contract's write methods (`setSlot`, `emitLog`, etc.) now accept calls from the current system address as reported by `SequencerRegistry`, not from a fixed address.
- Transactions that perform multiple value-transferring sub-calls or creates from the same contract now report lower data-size and KV-update usage than they did under Rex4.
  This only affects usage tracking; it does not change execution semantics, state transitions, or the base transaction gas model.

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
- Rex4 and earlier retain the original caller-account overcounting behavior unchanged.
- Transactions executed under Rex4 or earlier specs produce identical results before and after the Rex5 code is deployed.
- The KeylessDeploy sandbox accounting change is the most visible behavioral difference.
  The upfront budget capping and intrinsic preflight ensure that a successful sandbox run normally fits inside the parent's remaining resource envelope before state is merged.
  The residual overflow path rejects the outer call without merging sandbox state so no partial deployment survives a parent-level reject.
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
