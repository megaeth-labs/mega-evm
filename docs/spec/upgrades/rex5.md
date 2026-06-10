---
description: Rex5 network upgrade — SequencerRegistry with dual roles, dynamic system address, Oracle v2.0.0, KeylessDeploy trailing-bytes rejection and sandbox resource accounting, caller-account update deduplication, precompile compute-gas correction and cap, EIP-7702 metering fixes, SELFDESTRUCT beneficiary accounting, system-tx chain-id and nonce validation, deferred final Mega-side gas validation, KeylessDeploy error ABI refactor, deposit-caller new-account accounting, CALL_STACK_LIMIT depth gate for system contract interceptors, oracle hint admission and metering, CALLCODE new-account storage gas metering fix, top-level CREATE storage-gas address source, CREATE code-deposit compute-gas atomicity, EIP-2935/EIP-4788 pre-block gas floor with fail-closed block rejection, CREATE2 empty-initcode short-circuit, and KeylessDeploy empty-code constructor log forwarding.
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

Rex5 closes additional resource-accounting gaps.
The most significant change is that [KeylessDeploy](../system-contracts/keyless-deploy.md) sandbox execution now propagates its resource consumption back to the parent transaction, preventing low-cost state bloat via unmetered sandbox work.
Rex5 also corrects [compute gas](../glossary.md#compute-gas) recording for failed precompile calls, adds [state growth](../evm/resource-accounting.md#state-growth) tracking for EIP-7702 authority accounts, uses non-delegating account inspection for [storage gas](../glossary.md#storage-gas) metering, and charges new-account costs when `SELFDESTRUCT` creates a beneficiary account.

Rex5 further hardens several consensus-visible boundaries: precompile calls are now bounded by the remaining compute-gas budget, deposit transactions that materialize the caller account pay the new-account storage gas and contribute to state growth, system-contract interceptor dispatch respects `CALL_STACK_LIMIT`, and oracle hint forwarding requires positive `gas_limit` and meters the hint payload against the data-size budget.

All consensus-visible changes are gated on the Rex5 spec.

## What Changed

### 1. SequencerRegistry System Contract

#### Previous behavior

No on-chain registry for the system address or the sequencer existed.
The system address was the fixed `MEGA_SYSTEM_ADDRESS` constant, and the sequencer role was not tracked by any system contract.
Neither role could be rotated or queried on-chain, and there was no admin lifecycle for managing them.

#### New behavior

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

#### Previous behavior

The system address used for system transaction identification and Oracle gas detention exemption was the hardcoded compile-time constant `MEGA_SYSTEM_ADDRESS`.
It could not change without a code change.

#### New behavior

The system address used for system transaction identification and Oracle gas detention exemption is no longer a hardcoded constant.
It is resolved per block from the `SequencerRegistry`'s current system address after all pre-block changes are committed.

Changing the sequencer does NOT affect the system address, and vice versa.

### 3. Oracle v2.0.0

#### Previous behavior

The Oracle contract's `onlySystemAddress` modifier read the authorized system address from a value fixed at construction time (a constructor `immutable`).
Changing the system address required redeploying the Oracle, and an in-place Oracle bytecode upgrade marked the Oracle account as newly created, clearing existing Oracle storage.

#### New behavior

The Oracle contract's `onlySystemAddress` modifier now reads from `SequencerRegistry.currentSystemAddress()` instead of a value fixed at construction time.
This enables system address change without redeploying the Oracle.

All other Oracle functionality (`sendHint`, `multiCall`, `getSlot`, `setSlot`, etc.) is preserved from v1.1.0.

From Rex5, in-place Oracle bytecode upgrades no longer mark the Oracle account as newly created, so any Oracle storage accumulated before the upgrade is preserved across the transition.
This differs from pre-Rex5 upgrades, which cleared existing Oracle storage.

### 4. Pre-Block Role Change

#### Previous behavior

No pre-block system call for applying role changes existed, because no on-chain registry of the system address or sequencer existed to update.
Neither role was rotated during pre-block execution.

#### New behavior

Pending role changes are applied during pre-block execution via a single pre-block EVM system call to `SequencerRegistry.applyPendingChanges()`.
This follows the same pattern as EIP-2935 and EIP-4788.
The system call is only issued when a pre-check confirms any role change is due.
Like the REX5-updated EIP-2935 / EIP-4788 pre-block calls, this system call is issued with a gas limit of `max(block.gas_limit, 30_000_000)` instead of the previously fixed 30M gas limit.
This is required because the role-rotation slot writes are charged by REX dynamic storage gas, so their cost is no longer guaranteed to fit within a fixed 30M budget on activation blocks.

### 5. KeylessDeploy Trailing-Bytes Rejection

#### Previous behavior

The `keylessDeploy` interceptor decoded the inner pre-EIP-155 transaction RLP without rejecting trailing bytes after the signed payload.
Encodings with trailing data were accepted as long as the leading bytes formed a valid `TxLegacy`.

#### New behavior

The decoder MUST reject any encoding that contains bytes after the signed RLP payload by reverting with `MalformedEncoding()`.
This tightens validation so that two distinct byte strings cannot both pass as the "same" inner deployment transaction.

### 6. CALLCODE New-Account Storage Gas Metering

#### Previous behavior

The storage-gas wrapper for `CALLCODE` charged new-account storage gas against the stack `to` address — i.e. the code-source address.
For `CALLCODE`, however, execution happens in the caller's account context: the stack `to` only selects which code to load, while the storage / account context remains the caller's.
Charging new-account storage gas against the code-source address can therefore charge spuriously when the code-source is empty.

#### New behavior

The new-account emptiness check and the new-account storage-gas charge are performed against the current frame's storage account (the caller / executing contract).
The stack `to` continues to be used solely as the code-source for the underlying `CALLCODE` instruction.
Pre-Rex5 specs preserve the (frozen) prior behavior for backward compatibility.
`CALL` semantics are unchanged: the stack `to` is still the value recipient and is the correct address for new-account metering.

### 7. Caller-Account Update Deduplication (Data Size and KV Updates)

#### Previous behavior

When a call frame performed a value-transferring `CALL` / `CALLCODE` or a `CREATE` / `CREATE2`, the _caller_ account update was charged to the child frame's discardable budget.
However, the parent frame did not record that its caller account had already been charged.
As a result, every subsequent value-transferring sub-call or create from the same parent frame re-charged the caller account, overcounting both data-size bytes and KV-update counts for the caller.

#### New behavior

After charging the first caller-account update within a parent call frame, that parent frame records that its caller account has been charged.
All subsequent value-transferring sub-calls and creates from the same parent frame no longer re-charge the caller account.
Each distinct callee or created account is still counted independently.
The discardable-on-revert mechanic is unchanged: charges recorded inside a child frame that reverts are still dropped.

### 8. KeylessDeploy Sandbox Resource Accounting

#### Previous behavior

- The sandbox tracked its resource limits in isolation.
- After sandbox execution, only the resulting state was merged into the parent transaction.
- The sandbox's resource consumption (standard gas, compute gas, data size, KV updates, state growth) was discarded.
- Both sandbox success and sandbox execution failure merged state unconditionally.

#### New behavior

- Before sandbox execution starts, the sandbox receives its own resource-limit budgets capped to the parent transaction's remaining resource budgets.
- The sandbox's transaction limits are derived from the parent's active runtime limits so any custom block-environment-access or oracle-access compute-gas limit configured on the parent is preserved.
- Before sandbox execution starts, known sandbox intrinsic transaction-level usage is preflighted against the parent's remaining resource budgets.
- If that intrinsic usage does not fit, sandbox execution is not started and the outer call reverts with `ParentBudgetExceeded`.
- The sandbox's resource usage is extracted before the sandbox context is dropped.
- The parent transaction's resource usage is updated with the sandbox's usage.
- The parent's gas-limit override is capped to the parent's remaining gas before sandbox execution starts.
- In the normal case the sandbox fails internally inside the sandbox envelope rather than succeeding and being rejected later by the parent.
- After sandbox completion, sandbox state is merged and the signer continues to pay sandbox gas through the merged state on both sandbox success and frame-local sandbox failure paths.
- On the residual overflow path, where a single-opcode overshoot at a TX-level compute-gas check or a future un-preflighted tx-level persistent accounting path pushes the merged usage above the parent's envelope, the node charges the sandbox's EVM gas to the parent gas meter, rescues the remaining outer gas for refund, and halts the outer call with `OutOfGas`.
- No sandbox state is merged on this path, matching the standard convention that halted transactions commit only their pre-execution state.

### 9. Precompile Compute Gas on Error Paths

#### Previous behavior

- Compute gas for precompile calls was recorded from the precompile's reported spent gas.
- On error paths (`PrecompileOOG`, `PrecompileError`), no spent gas was reported, so the recorded compute gas was 0.
- The forwarded gas was fully consumed from the parent's EVM gas meter but recorded as 0 compute gas.

#### New behavior

- On error paths, compute gas is recorded as the full forwarded gas amount, matching the EVM gas actually consumed.
- On success and revert paths, compute gas continues to use the precompile's reported spent gas.
- **Exception (Mega fixed-cost precompiles).** When the failed precompile is a Mega-overridden fixed-cost precompile and the call's forwarded gas was at least the fixed cost (so verification or computation ran), the recorded compute gas is the fixed cost, not the full forwarded amount. Today this exception applies only to KZG point evaluation (`0x0a`, fixed cost `KZG_POINT_EVALUATION_GAS_COST = 100_000`). EVM-gas burn on the same path is unchanged: the call still halts with `PrecompileError` and the parent loses the forwarded amount.

### 10. EIP-7702 Authority State-Growth Tracking

#### Previous behavior

- The `data_size` and `kv_update` lanes charged for EIP-7702 authority accounts at the start of the transaction.
- The `state_growth` lane did not count fresh authority accounts.

#### New behavior

- The `state_growth` lane counts only valid EIP-7702 authorizations whose authority account is a previously non-existent state entry.
- This accounting happens during pre-execution after the authority account has been loaded from the journal and before the authorization list mutates delegation bytecode.
- Existing authority accounts do not count toward state growth.
- Authorization entries skipped by chain ID, nonce, authority recovery, or incompatible existing code do not count toward state growth.

### 11. Non-Delegating Account Inspection for Metering

#### Previous behavior

- New-account storage-gas metering for `CALL` and `CREATE` inspected the target account by following EIP-7702 delegation, returning the delegate's account state.
- When a delegate was empty, the authority was incorrectly charged the new-account storage-gas premium.
- For `CREATE`, the delegate's nonce was used to compute the created address, causing the SALT bucket lookup to query the wrong address.
- The `state_growth` lane's `CALL` emptiness check also followed delegation.

#### New behavior

- New-account storage-gas metering for `CALL` and `CREATE` inspects the authority's own state without following EIP-7702 delegation.
- The `state_growth` lane's `CALL` emptiness check inspects the authority's own state without following delegation.

### 12. SELFDESTRUCT Beneficiary New-Account Metering

#### Previous behavior

- `SELFDESTRUCT` to an empty beneficiary created a new account without any MegaETH-specific charges.
- No storage-gas premium, data size, KV update, or state growth was recorded for the new beneficiary.

#### New behavior

- `SELFDESTRUCT` that sends value to an empty beneficiary charges the new-account storage-gas premium and records data size (+40 bytes), KV update (+1), and state growth (+1).
- Zero-balance `SELFDESTRUCT` does not trigger new-account charges (no value transfer means no account creation).

### 13. System Transaction Chain-Id and Nonce Validation

#### Previous behavior

A legacy transaction whose signer is `MEGA_SYSTEM_ADDRESS` was promoted to an OP-style deposit transaction before ordinary validation ran, bypassing signature, chain-id, nonce, balance, and fee checks.
A captured raw system transaction could in principle be replayed against any chain configuration that accepted the same byte string.

#### New behavior

Before the deposit promotion, a node MUST validate the system transaction's chain-id and nonce against the same canonical rules ordinary user transactions follow:

- The chain id MUST be present and MUST equal the node's configured chain id (subject to the chain-id check toggle).
- The nonce MUST equal the current state nonce of `MEGA_SYSTEM_ADDRESS` (subject to the nonce-check toggle).
- If `MEGA_SYSTEM_ADDRESS` carries code, the EIP-3607 check applies (subject to the EIP-3607 toggle).

A failure surfaces as a canonical `InvalidTransaction` variant before any state mutation.
Signature, balance, and fee bypasses are preserved once the checks pass.

The same configuration toggles that govern user-transaction validation are honored so the system-tx validate path stays symmetric with the canonical user-tx validate path for debug, state-test, and replay tooling.
This is correctness recovery, not new defense in depth: OP deposits get away with bypassing these checks because L1 derivation plus per-deposit `source_hash` uniqueness provides higher-layer replay protection that MegaETH system transactions do not have.
See [system-tx.md](../system-contracts/system-tx.md) for the normative specification.

### 14. Final Mega-Side Gas Validation Ordering

#### Previous behavior

The intrinsic-gas check (intrinsic gas exceeding the transaction's gas limit) fired after MegaETH calldata storage gas was added but before CREATE / new-callee account storage gas.
A transaction whose final Mega-adjusted intrinsic gas exceeded the gas limit only after those later storage gas contributions halted with gas used equal to the full gas limit — sender debited, nonce bumped, no call effects.

#### New behavior

The check is deferred until after every Mega-side intrinsic and dynamic storage gas contribution has been added.
A transaction that cannot fit its final Mega-side intrinsic or floor gas requirement is rejected as a canonical validation error (`InvalidTransaction::CallGasCostMoreThanGasLimit` or `InvalidTransaction::GasFloorMoreThanGasLimit`) before pre-execution runs, leaving sender balance and nonce untouched.

### 15. KeylessDeploy Error ABI Refactor

#### Previous behavior

`IKeylessDeploy::InternalError(string message)` carried a stringified internal error as its ABI payload.

#### New behavior

- `InternalError(string)` becomes `InternalError()` (selector-only).
  The `message` field is dropped from the wire; root cause is reported off-chain via node logs.
- A new selector-only error `InvalidTransaction()` is added to the stable validation error set, raised when the sandbox rejects the inner transaction during validation, before pre-execution runs (typically section 14's final gas check, but structurally any transaction-validation failure, including the synthesized halt from a deposit-style validation failure).
  The outer KeylessDeploy call MUST revert with `InvalidTransaction()` and the signer MUST NOT be charged in this path.
- A new error `InitCodeTooLarge(uint64 size, uint64 max)` is added.
  It is raised when the inner transaction's initcode length exceeds the configured maximum initcode size; the sandbox enforces this because the deposit-style sandbox transaction (see section 23) bypasses the standard environment validation where the EIP-3860 size check normally lives.
  `size` is the actual initcode length; `max` is the configured cap.
- A new selector-only error `SignerHasCode()` is added.
  It is raised when the recovered signer's parent-state bytecode is non-empty and is not a valid EIP-7702 delegation designation, unless the EIP-3607 check is disabled.
  This re-enforces EIP-3607 because the deposit-style sandbox transaction bypasses the standard account nonce-and-code validation.

`InvalidTransaction()`, `InternalError()`, and `SignerHasCode()` are selector-only because precompile return data is reachable on-chain via `RETURNDATACOPY` → `SSTORE` → state root, so coupling the wire format to a non-stable internal error string would pin consensus to it.
See [keyless-deploy.md](../system-contracts/keyless-deploy.md) for the normative error list.

### 16. Precompile Compute-Gas Cap

#### Previous behavior

A precompile invocation forwarded the caller's full call gas limit to the precompile unchanged.
Compute gas was recorded post-hoc from the precompile's reported spent gas.
A precompile whose natural cost exceeded the remaining compute-gas budget would still execute fully, and the overshoot was only detected after the fact.

#### New behavior

The gas forwarded into a precompile is capped at the minimum of the call's gas limit and the current call's remaining compute-gas budget.
A precompile whose minimum cost exceeds the cap MUST return `PrecompileOOG` without performing the computation.
On successful or reverting precompile returns, the caller's gas accounting is normalized so that the caller's refund reflects the original forwarded gas limit minus the precompile's actual spent gas.
Halt-class returns (`PrecompileOOG` and `PrecompileError`) preserve the underlying precompile's gas shape unchanged, because halt paths do not consume the remaining-gas field in caller refund.

### 17. Deposit Caller New-Account Accounting

#### Previous behavior

A deposit-like transaction (Optimism deposit or mega-system deposit-marked legacy) whose caller account was empty at validation time could materialize the caller via a minted balance increment or a pre-execution nonce bump.
The materialization did not pay the new-account storage gas for the caller and did not contribute to state growth.

#### New behavior

For every deposit-like transaction whose caller account is empty before pre-execution, a node MUST add the caller's new-account storage gas to the transaction's intrinsic gas and MUST record exactly one new-account event (`+1`) on the `state_growth` transaction-intrinsic lane.
The recording is single-shot — a subsequent transaction whose caller is the now-non-empty account incurs no additional charge.
Where the same address appears as both caller and callee on a value-transferring deposit call transaction, the existing callee-side new-account storage-gas charge is the single gas charge and the deposit-caller path records only the state-growth event without re-charging the gas.

The `data_size` and `kv_update` lanes are unaffected by this rule.
Their existing transaction-start charges already account for the caller's account-info write on every transaction.

### 18. CALL_STACK_LIMIT Depth Gate for System Contract Interceptors

#### Previous behavior

System contract interceptor dispatch ran before the standard EVM call-depth check (call depth exceeding `CALL_STACK_LIMIT`).
A call to a system contract from a frame whose depth exceeded the limit could therefore receive a synthetic interceptor result instead of a call-too-deep failure.

#### New behavior

For any `CALL` or `STATICCALL` whose call depth exceeds `CALL_STACK_LIMIT`, a node MUST short-circuit frame initialization with a synthetic call-too-deep result that spends no gas (the full call gas limit is refundable to the caller) BEFORE consulting any system contract interceptor.
No interceptor side effects (volatile-data tracker mutation, oracle hint forwarding, keyless deploy) MUST be performed in the too-deep case.

A transaction-level additional-limit exceed takes priority over this depth gate.
The inspector-driven frame-initialization path mirrors the same ordering: exceeded-limit check, then depth gate, then any inspector-provided synthetic output.

`CALLCODE`, `DELEGATECALL`, and the `CREATE` / `CREATE2` paths are out of scope for this gate.
Their depth checks remain in the standard EVM frame-construction path.

### 19. Oracle Hint Admission and Metering

#### Previous behavior

The oracle `sendHint(bytes32 topic, bytes data)` interceptor forwarded the hint to the off-chain oracle backend whenever the calldata decoded successfully, regardless of the call's gas limit.
A call with a gas limit of 0 would forward the hint to the off-chain oracle backend before the on-chain Oracle frame ran out of gas.
The hint payload was not charged against any transaction-level budget.

#### New behavior

A node MUST forward a hint to the off-chain backend only when all of the following hold:

1. The call's gas limit is greater than 0.
2. The leading four bytes of the calldata match the `sendHint` selector.
3. The full calldata decodes as a valid `sendHint(bytes32 topic, bytes data)` invocation.
4. Recording the full raw calldata byte length of the call — the entire call input, including the four-byte selector and the ABI envelope (offset and length words plus padding) around `topic` and `data` — against the TX `data_size` intrinsic lane keeps total `data_size` usage within the configured TX `data_size` limit.

When any of (1)–(4) fails, the interceptor MUST NOT forward the hint.
A failure of (4) MUST cause the transaction to halt with the canonical TX-level data-size `OutOfGas` failure (matching every other data-size overflow); the failure does not introduce a new failure shape.
A failure of (1) or (3) lets the call fall through to the on-chain Oracle bytecode for canonical handling.

The full call input is charged (not just the decoded `data`) so a caller cannot force unmetered host-side materialization work via a selector-matching but malformed or oversized payload.
The charge is therefore the byte length of the calldata as received, which is known before any decode.

The data-size charge is applied before the calldata is decoded and is charged regardless of whether decoding later succeeds.
A call whose leading four bytes match the `sendHint` selector but whose remaining calldata fails to decode as a valid `sendHint(bytes32 topic, bytes data)` invocation MUST still charge the full calldata byte length against the TX `data_size` lane.
Such a call MUST NOT forward the hint; it falls through to the on-chain Oracle bytecode for canonical handling.
This closes a prior gap in which a selector-matching but malformed payload performed host-side materialization work without being charged.

### 20. Value-Transfer CALL/CALLCODE Parent Compute-Gas Attribution

#### Previous behavior

The per-opcode compute-gas accounting subtracted the child frame's full gas limit from the parent's recorded compute gas, including the standard EVM `CALL_STIPEND` (2,300) that the EVM adds to value-transferring `CALL` and `CALLCODE` child frames.
Because `CALL_STIPEND` is gas the EVM grants to the child without deducting it from the parent, subtracting the full child gas limit (which includes the stipend) under-counted the parent's contribution by exactly `CALL_STIPEND` per value-transferring `CALL` / `CALLCODE` invocation.

#### New behavior

For a value-transferring `CALL` or `CALLCODE`, a node MUST subtract the child's gas limit minus `CALL_STIPEND` (the parent-contributed forwarded portion) from the parent's recorded compute gas, not the raw child gas limit.
`DELEGATECALL`, `STATICCALL`, and any `CALL` / `CALLCODE` with zero value MUST continue to subtract the raw child gas limit — those frames receive no `CALL_STIPEND`.
`CREATE` / `CREATE2` are unaffected; they have no value-transfer stipend.

Pre-Rex5 specs MUST continue to subtract the raw child gas limit for byte-for-byte replay parity of historical blocks.
The under-counting is intrinsic to the stable-spec behavior and is preserved deliberately.

### 21. `STORAGE_CALL_STIPEND` Separated-Allowance Model

#### Previous behavior

Value-transferring internal `CALL` / `CALLCODE` frames receive `STORAGE_CALL_STIPEND` (23,000) added to the child's `gas_limit` so the callee can pay the Mega 10× storage-gas costs (LOG topics/data, new-account materialization, first-time-write SSTORE, CREATE contract-storage, SELFDESTRUCT beneficiary creation) when the caller forwards little or no gas.
The extra gas is restricted to storage-gas usage by a post-hoc per-frame compute-gas cap pinned at the pre-stipend gas limit; unused stipend is burned on frame return by clamping the child's remaining gas to the pre-inflation limit.

Because the per-frame compute cap is enforced after each opcode finishes, a single expensive opcode or precompile invocation in the child can spend stipend gas as compute and have its full cost recorded into the parent's compute-gas counter before the cap triggers the frame-local revert.
Repeated value-transferring CALLs from the same parent can amplify recorded compute gas beyond what the transaction's compute-gas limit would otherwise allow.

#### New behavior

`STORAGE_CALL_STIPEND` becomes a per-frame allowance internal to the resource tracker.
The child's gas limit MUST NOT be inflated by `STORAGE_CALL_STIPEND` on Rex5.
For each of the five Mega-introduced storage-gas surcharge sites — CALL/CALLCODE empty-account new-account, CREATE/CREATE2 contract-creation, SSTORE first-time-write zero-to-nonzero, LOG topics-and-data, and SELFDESTRUCT empty-beneficiary new-account — a node MUST drain up to `STORAGE_CALL_STIPEND` from the current frame's allowance and charge only the residual surcharge (the surcharge minus the amount drained) against the EVM gas counter.
Charging sites where the surcharge can overflow (LOG) MUST preserve the overflow arm unchanged so the existing overflow halt behavior is identical to pre-Rex5.

Because the allowance never enters the frame's gas limit, it cannot be spent on compute opcodes by construction.
On frame return there is nothing to burn — the remaining gas already reflects only parent-contributed gas — and the rescued-gas path naturally excludes the allowance with no special arithmetic.

The Mega per-frame compute-gas cap is unused on Rex5.
It remains in place under stable specs so the legacy inflation path retains its byte-for-byte behavior, but the Rex5 separated-allowance path bypasses it.

**Scope.** The allowance covers ONLY Mega-introduced storage-gas surcharges, not standard EVM opcode gas.
A child whose forwarded gas plus `CALL_STIPEND` does not cover the standard EVM cost of the opcode it executes (SSTORE base 22,100, CREATE2 frame setup, SELFDESTRUCT base) will OOG normally regardless of the allowance balance.
The unique case where end-to-end success is achievable at forwarded `gas = 0` is LOG1 with small payloads, because LOG1's standard EVM cost (~750) fits inside the 2,300 `CALL_STIPEND`.

The allowance applies to internal (depth > 0) value-transferring `CALL` / `CALLCODE` only.
Top-level transactions, DELEGATECALL / STATICCALL, CREATE / CREATE2 (which never grant the value-transfer stipend), and any value-zero CALL do not receive an allowance.

Pre-Rex5 specs MUST retain the legacy inflation, per-frame compute cap, and burn-on-return for byte-for-byte replay parity.

**Cross-reference:**

- The Rex4 introduction of `STORAGE_CALL_STIPEND` is documented in [Rex4](rex4.md) section 5 and [Gas Forwarding](../evm/gas-forwarding.md).
- The Rex5 separated-allowance model refines the same stipend semantically without changing the 23,000 grant amount or the value-transfer admission rule.

### 22. KeylessDeploy Sandbox Volatile-Access Footprint Merge

#### Previous behavior

- The KeylessDeploy sandbox tracked volatile-data access in isolation.
- After sandbox execution, the sandbox's accumulated volatile-access record was discarded along with the sandbox context.
- The parent transaction's reported volatile-access footprint therefore reflected only the volatile accesses made outside the sandbox, even if the sandbox's constructor code accessed `TIMESTAMP`, `COINBASE`, the Oracle contract, the beneficiary balance, or other volatile data.

#### New behavior

- The sandbox's accumulated volatile-access record is extracted from the sandbox context before that context is dropped.
- Immediately after the sandbox's resource usage is merged and before the unused-reservation refund, the sandbox's volatile-access record is unioned into the parent transaction's volatile-access footprint.
- The merge runs on every path where the sandbox actually executed — sandbox success, in-sandbox failure, and the post-merge residual-overflow halt — so the parent transaction's reported footprint reflects what the sandbox accessed regardless of the outer call's final outcome.
- Only the access record is merged. The detention cap, the disable-state, and the configured per-spec limits are deliberately not merged.

#### Footprint-only semantics — no halt-reason remap

The merge propagates the _footprint_ (which volatile data was accessed) but NOT the detention enforcement state. Specifically, the parent's detained compute-gas limit is unchanged by this merge — the sandbox's internal detention adjustments remain sandbox-internal.

A generic exceeding-limit halt is remapped into a volatile-specific variant (e.g. `VolatileDataAccessOutOfGas`) only when the parent's own compute-gas limit was detained by a volatile access. That detention is established by volatile-aware opcode handling during the parent frame's own execution. A sandbox-only volatile access therefore does NOT, by itself, cause the parent's residual-overflow halt to be remapped — it remains the generic `ComputeGasLimitExceeded` variant.

This scoping is intentional. Detention is a frame-local enforcement device; for the keyless-deploy depth-zero invariant the parent has no further work after the sandbox returns, so propagating the detention state would have no observable benefit but would surface as a consensus-visible behavior change in halt reasons. The remap path remains available to any future caller that needs volatile-aware halt classification through the parent's own detention mechanism.

### 23. KeylessDeploy Sandbox Outer EVM Gas Debit

#### Previous behavior

- The outer keyless-deploy call's gas counter was debited only the fixed dispatch overhead (`KEYLESS_DEPLOY_OVERHEAD_GAS`, 100K).
- The sandbox's gas used was reported only inside the ABI-encoded `keylessDeploy` return payload; the outer gas counter was not charged with it.
- Consequence: the outer transaction's receipt `gasUsed` and the block header's `gas_used` field did NOT reflect computation performed inside the sandbox. The multidimensional limits (compute gas, data size, KV updates, state growth) WERE correctly accounted, so block-level multidim caps still bounded sandbox cost; but the legacy EVM-gas channel was blind to it.

#### New behavior

- The sandbox transaction runs under the standard message-call gas shape applied to the outer gas counter: pre-debit the capped gas-limit override on entry, refund the unused portion (the override minus the sandbox gas used) on exit.
- Net effect on every sandbox-completion path (sandbox success and in-sandbox failure): the outer call's gas counter is debited by exactly the sandbox gas used, the same as if a normal CALL of that cost had run.
- If the sandbox bails before producing an outcome (validate-reject / internal error), the full reservation is refunded; the upfront `KEYLESS_DEPLOY_OVERHEAD_GAS` and pre-sandbox materialization charges are retained.
- The inner signer's balance is NOT debited for sandbox gas: the sandbox runs as an OP deposit-like transaction with a zero gas price, so the deposit-path caller balance escrow degenerates to zero (see "Outer-only billing" below).
  The inner signer only ever loses the optional value transfer (zero for canonical Nick's-Method deployers).
- Result: the outer transaction's receipt `gasUsed` and the block header `gas_used` now reflect actual computation; the legacy EVM-gas channel is consistent with the multidim channel.

**Outer-only billing.** The sandbox transaction runs as an OP deposit-like transaction: its deposit source hash is set to a sandbox-specific marker, its gas price is forced to zero, and its mint amount is held at none.
The source hash makes the transaction be treated as a deposit, which disables the L1 fee, the operator fee, the standard environment validation (signature, nonce, configured initcode size limit, chain id, etc.), the caller balance sufficiency check, and beneficiary reward distribution.
The zero gas price is required because the deposit branch of caller validation still computes a caller balance escrow of gas limit times effective gas price plus any additional cost; with the additional cost at zero (L1/operator skipped) and the gas price at zero, the escrow is zero and the inner signer's balance is left untouched.
The outer transaction's own fee model is the sole fee source.
For a normal outer transaction the outer sender pays (dispatch overhead + caller-materialization storage gas + sandbox gas used) times the outer gas price via the outer gas counter (a pre-debited reservation that refunds the unused tail) and the standard EIP-1559 / OP fee split; deposit and system outer transactions inherit their existing fee-free semantics.

**GASPRICE in sandbox.**
A direct consequence of setting the sandbox transaction's gas price to zero is that the `GASPRICE` opcode executed inside the constructor / init code observes `0`, regardless of the gas price encoded in the keyless transaction signature.
Initcode authors targeting Rex5+ KeylessDeploy MUST assume `GASPRICE == 0` inside the sandbox.
The canonical Nick's-Method deployers in widespread use (e.g. the Arachnid CREATE2 deployer) do not read `GASPRICE`, so this behavior change has no practical effect on existing deployments.
The change is consensus-observable but intentional: preserving the signed gas price inside the sandbox would re-introduce the caller balance escrow that fee-free mode exists to eliminate.

**Deposit-style validation handling.** The deposit validation path swallows a transaction-validation failure into a `FailedDeposit` halt (gas used equal to the gas limit) with a nonce bump applied inside the sandbox journal. The sandbox result handling detects the `FailedDeposit` halt reason and remaps it to `KeylessDeployError::InvalidTransaction`, preserving the existing contract that a sandbox validate-reject does NOT consume the Nick's-Method replay barrier: the outer keyless-deploy call surfaces as a revert, the sandbox state is discarded (so the deposit-failure nonce bump is never merged into the parent), and the pre-debited gas-limit-override reservation is fully refunded to the outer gas counter.
The upfront `KEYLESS_DEPLOY_OVERHEAD_GAS` and pre-sandbox caller materialization charges remain debited — paying for the dispatch work and the parent-state read that actually occurred, in the same shape as the dispatch overhead.

**Caller materialization accounting.**
Because the sandbox-internal caller validation no longer touches the signer under deposit-style fee-free mode, the standard Rex5 deposit-caller storage gas charge is gated off inside the sandbox.
The materialization charge is performed instead before the sandbox transaction is constructed, reading the parent-visible state (preferring the journal cache, then falling back to the backing database — neither path goes through the sandbox's nonce override).
When the parent-visible signer is empty, the outer gas counter is debited by the signer's new-account storage gas and a deposit-caller state-growth event is recorded.
On retry the parent-visible signer is already non-empty (its nonce was bumped by the previous deploy's contract-creation frame), so the charge does not fire a second time.
The charge is paid upfront — alongside `KEYLESS_DEPLOY_OVERHEAD_GAS` — and is retained regardless of sandbox outcome: sandbox-validate-reject, in-sandbox revert, and post-merge residual overflow all leave the materialization charge in place, mirroring the upfront dispatch overhead.
A database read failure or a dynamic storage-gas computation failure surfaces as `KeylessDeployError::InternalError`; the sandbox is not started in that case.

**Relayer pricing impact.**
Pre-Rex5, the relayer's marginal outer-gas cost for invoking KeylessDeploy was about `100K × outer_gas_price` (the dispatch overhead alone).
Under Rex5 the marginal cost becomes about `(100K + sandbox_gas_used + caller_materialization_storage_gas) × outer_gas_price`, where the caller-materialization storage gas is zero on retries and on warm-bucket signers.
For compute-heavy keyless deploys this can be a 10–100× increase in the relayer-side gas budget relative to Rex4.
Relayers MUST size the outer gas limit to cover the worst-case sandbox cost they are willing to underwrite.
The `gasLimitOverride` argument to `keylessDeploy` is already capped to the parent's remaining outer gas under Rex5, so the outer caller has full visibility into the upper bound the sandbox can spend.
The inner signer no longer needs a pre-funded balance to cover sandbox gas — only enough to cover the optional value transfer encoded in the keyless transaction (typically zero for Nick's-Method deployers).

### 24. Top-Level CREATE Storage-Gas Address Source

#### Previous behavior

For a top-level contract-creation transaction, the created-contract address used for the intrinsic storage-gas charge and the SALT bucket lookup was derived from the transaction's `nonce` field.

#### New behavior

The created-contract address used for the intrinsic storage-gas charge and SALT bucket lookup MUST be derived from the sender account's current state nonce — the nonce actually consumed to create the contract.
These two values can differ when prior in-block activity by the sender, or EIP-7702 authorizations that advance the sender's nonce, leave the account's state nonce ahead of the transaction's `nonce` field.
Using the state nonce ensures the charged address and bucket match the address the deployment actually produces.

### 25. CREATE Code-Deposit Compute-Gas Atomicity

#### Previous behavior

The code-deposit compute gas for a contract creation (`code_length × CODEDEPOSIT`) was recorded via the post-execution compute-gas record, separate from the deployment commit.

#### New behavior

The code-deposit compute gas for any contract creation — `CREATE`, `CREATE2`, or a contract-creation transaction, at any call depth — MUST be charged atomically with the deployment commit.
It MUST be pre-charged exactly when the deployment's pre-commit success conditions hold (the creation returned successfully and passes the runtime code-validity and size checks that gate the commit), and it MUST NOT be double-counted by the post-execution compute-gas record.
A contract creation whose pre-commit conditions do not hold does not incur the code-deposit compute gas on this path.

### 26. EIP-2935 / EIP-4788 Pre-Block System Call Gas Floor

#### Previous behavior

The EIP-2935 history-storage and EIP-4788 beacon-roots pre-block system calls used a fixed gas limit of 30,000,000.

#### New behavior

These pre-block system calls MUST use a gas limit of `max(block_gas_limit, SYSTEM_CALL_GAS_LIMIT_FLOOR)`, where `SYSTEM_CALL_GAS_LIMIT_FLOOR = 30,000,000`.
The fixed 30,000,000 budget is no longer sufficient because the slot write is charged by MegaETH dynamic storage gas, whose cost can exceed a fixed 30M.
Additionally, these pre-block system calls are fail-closed: a non-successful call MUST cause the block to be rejected.

`SYSTEM_CALL_GAS_LIMIT_FLOOR` (value `30,000,000`) is the lower bound on the gas limit used for these pre-block system calls.

### 27. CREATE2 Empty-Initcode Short-Circuit

#### Previous behavior

`CREATE2` evaluated the memory offset operand unconditionally, charging memory-expansion gas (and, under specs that track it, compute gas) even when the init-code length was zero.
A `CREATE2` with zero length and a very large offset operand could spuriously halt out-of-gas on the offset conversion.

#### New behavior

A `CREATE2` with zero-length init code MUST short-circuit after validating the salt operand, using the empty-code hash as the init-code hash.
It MUST NOT convert the offset operand, expand memory, or hash any memory region.
A zero-length `CREATE2` with an arbitrarily large finite offset therefore neither halts on offset conversion nor charges memory-expansion gas.

### 28. KeylessDeploy Empty-Code Constructor Log Forwarding

#### Previous behavior

When the inner `CREATE` performed by KeylessDeploy succeeded but returned empty runtime bytecode, the constructor's emitted logs were dropped.
The merged state and the receipt logs therefore disagreed: state reflected the constructor's effects, but no logs were recorded.

#### New behavior

When the inner `CREATE` succeeds with empty runtime bytecode, the constructor's emitted logs MUST be forwarded — emitted by the outer call — before the outer call returns the `EmptyCodeDeployed(uint64 gasUsed)` result.
This keeps receipt logs consistent with the merged state.
Inner `CREATE` outcomes that revert or halt continue to drop their logs, because the reverted frame's logs are rolled back inside the sandbox.

## Developer Impact

- Contracts that verify mini-block signatures can use `SequencerRegistry.currentSequencer()` to look up the signing authority.
- Contracts that need historical information can use `systemAddressAt(blockNumber)` or `sequencerAt(blockNumber)`.
- The Oracle contract's write methods (`setSlot`, `emitLog`, etc.) now accept calls from the current system address as reported by `SequencerRegistry`, not from a fixed address.
- KeylessDeploy signed inner transaction encodings with trailing bytes now revert with `MalformedEncoding()`.
- Transactions that perform multiple value-transferring sub-calls or creates from the same contract now report lower data-size and KV-update usage than they did under Rex4.
  This only affects usage tracking; it does not change execution semantics, state transitions, or the base transaction gas model.
- Precompile calls in compute-gas-constrained frames now fail-fast with `PrecompileOOG` instead of running and overshooting the budget. Contracts that catch precompile failures see the same failure signal but may observe the failure earlier in their gas budget.
- Deposit transactions whose sender address is empty at the time of validation now require additional intrinsic gas equal to the sender's new-account storage gas to cover the materialization performed during pre-execution.
- Off-chain integrations that rely on the oracle hint forwarding for telemetry will no longer receive hints from calls with a zero gas limit, and will see hints from calls that exceed the transaction's data-size budget dropped at the EVM boundary.
  Backends should not assume idempotency of hints already received before such a transaction halts; partial flushes in earlier successful hints are not rolled back.
- Value-transferring `CALLCODE` no longer charges new-account storage gas based on the code-source address.
  Contracts that previously paid spurious new-account storage gas via `CALLCODE` to an empty address see lower gas usage under Rex5.

**KeylessDeploy callers** should be aware that sandbox execution now counts toward the outer transaction's resource budgets.
A keyless deploy that previously succeeded may now fail if the outer transaction has tight resource limits.
The `gasLimitOverride` parameter is capped to the outer transaction's remaining gas.
The typical failure mode for tight additional-limit budgets is now an internal sandbox execution failure with encoded error data.
If the sandbox's pre-frame intrinsic usage alone exceeds the parent's remaining envelope, sandbox execution is not started and the outer call reverts with `ParentBudgetExceeded`.
If an opcode overshoots a TX-level compute-gas check after sandbox execution has run, the outer call instead halts with `OutOfGas` and no sandbox state is merged; the caller is charged the sandbox's EVM gas through the outer gas meter and unspent gas is rescued for refund.

**Contracts using precompiles** are not affected in practice.
The compute-gas correction only changes accounting for failed precompile calls, which do near-zero real computation.

**Contracts using EIP-7702 delegation** may see slightly different gas costs for CALL and CREATE operations targeting authority accounts, because metering now inspects the authority's own state rather than the delegate's.

**Contracts using SELFDESTRUCT** with value transfer to empty addresses will now pay the new-account storage-gas premium and consume resource-limit budget for the new beneficiary account.

## Safety and Compatibility

- Pre-REX5 behavior is unchanged. The legacy `MEGA_SYSTEM_ADDRESS` constant is used for all pre-REX5 specs.
- `SequencerRegistry` does not have an interceptor. It runs normal on-chain bytecode.
- Both the current system address and the current sequencer are only updated during pre-block system calls, ensuring block-stability.
- Changing one role does not affect the other.
- Rex4 and earlier retain the original KeylessDeploy trailing-bytes behavior unchanged.
- Rex4 and earlier retain the original caller-account overcounting behavior unchanged.
- Transactions executed under Rex4 or earlier specs produce identical results before and after the Rex5 code is deployed.
- The KeylessDeploy sandbox accounting change is the most visible behavioral difference.
  The upfront budget capping and intrinsic preflight ensure that a successful sandbox run normally fits inside the parent's remaining resource envelope before state is merged.
  The residual overflow path rejects the outer call without merging sandbox state so no partial deployment survives a parent-level reject.
- The precompile compute-gas cap, deposit-caller accounting, `CALL_STACK_LIMIT` depth gate, and oracle hint admission/metering are REX5-only. Rex4 and earlier preserve the pre-fix semantics byte-for-byte so that replay of historical blocks continues to produce identical state roots and receipts.
- Rex4 and earlier retain the original `CALLCODE` new-account storage gas metering behavior unchanged.
- All Rex5 behavior is gated on the Rex5 spec; pre-Rex5 specs execute with identical semantics before and after the Rex5 code is deployed.

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
