---
description: Rex6 network upgrade — unified per-opcode gas metering order (storage gas charged before the opcode body, compute gas recorded exactly once after it completes), EIP-7702 authorization accounting consolidated into validation with per-authorization data-size and KV-update charges narrowed to applied authorizations, dynamic SALT account-creation gas for net-new authorities, beneficiary gas detention triggered when an applied authority equals the block beneficiary, CREATE-frame resource accounting corrected (failed-CREATE results no longer rewritten into a limit revert, creator nonce-bump booked to the parent frame, and CREATE state growth recorded only for net-new addresses), KeylessDeploy sandbox hardened (outer sender's unused gas rescued on a transaction-level compute-gas halt, and a self-destructing constructor reported as an empty-code deployment), post-execution fee-reward account materializations counted toward resource accounting, and system-originated transactions exempted from per-transaction resource metering (SALT-scaled storage gas, the four resource-limit dimensions, and gas detention) so protocol-mandated state changes cannot fail as SALT buckets grow.
---

# Rex6 Network Upgrade

This page is an informative summary of the Rex6 specification.
For the full normative definition, see the Rex6 spec in the mega-evm repository.

{% hint style="warning" %}
**Unstable** — Rex6 is under active development.
Its semantics may still change before network activation.
{% endhint %}

## Summary

Rex6 bundles six consensus-visible changes to gas and resource accounting:

1. **Unified per-opcode gas metering order.** Rex6 defines a single, canonical order in which every storage-affecting opcode charges [storage gas](../glossary.md#storage-gas) and records [compute gas](../glossary.md#compute-gas), and brings `CREATE2` under it.
2. **Consolidated EIP-7702 authorization accounting.** Rex6 derives every per-authorization effect from a single applied-authorization scan that runs during transaction validation.
3. **CREATE-frame resource accounting.** Rex6 stops rewriting an already-failed `CREATE` result into a limit `Revert`, and corrects the creator nonce-bump and net-new state-growth accounting on the `CREATE` frame.
4. **KeylessDeploy sandbox hardening.** Rex6 rescues the outer sender's unused gas when a keyless-deploy dispatch hits the transaction-level compute-gas limit, and reports a keyless deploy whose constructor self-destructs as an empty-code deployment rather than a success.
5. **Post-execution fee-reward accounting.** Rex6 counts account materializations performed by the post-execution beneficiary fee-reward step toward resource accounting, closing a window in which they escaped it.
6. **System-originated transaction metering exemption.** Rex6 exempts the protocol's own transactions from MegaETH's per-transaction resource metering, so protocol-mandated state changes cannot be pushed out of gas as SALT buckets grow.

### Unified Gas Metering Order

Under the [dual gas model](../evm/dual-gas-model.md), the compute gas a transaction has consumed is itself a metered resource bounded by the [compute gas limit](../evm/resource-limits.md).
The relative order of charging storage gas and recording compute gas within an opcode is therefore consensus-visible: when an opcode halts partway through, that order determines how much compute gas has been recorded and which limit is reached first.

Prior specs left this order implicit and applied it inconsistently.
Most storage-affecting opcodes already charged storage gas before running their body and recorded compute gas once afterward, but `CREATE2` was an exception: it recorded its memory-expansion gas as a separate, earlier compute-gas entry.
Rex6 makes the order an explicit rule and brings every storage-affecting opcode under it, folding the `CREATE2` memory-expansion gas into the single post-body recording.

Rex6 is behavior-preserving for `SSTORE`, `LOG0`–`LOG4`, `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE`, and `SELFDESTRUCT`: for these opcodes no work consumes EVM gas before the storage-gas charge, so the canonical order records the same compute gas, at the same point, as before.
Only `CREATE2` changes, and only on a halt that lands between its memory expansion and the completion of its body.

### EIP-7702 Authorization Accounting

Rex6 consolidates EIP-7702 authorization accounting into a single applied-authorization scan that runs during transaction validation.
Pre-Rex6, per-authorization effects were split across three handler phases — data-size and KV-update charges in `before_tx_start`, state-growth charges in pre-execution after the caller nonce bump, and beneficiary detention only via opcode access — and used different gating criteria.
This split caused four metering bugs in which skipped authorizations were still charged, applied authorizations missed the beneficiary trigger, dynamic SALT gas for net-new authorities was not enforced against the gas limit, and a value-transfer recipient that an authorization materialized could be double-charged.

Rex6 derives every per-authorization effect — data size, KV update, state growth, dynamic new-account storage gas, and beneficiary detention — from a single journal-aware scan that mirrors revm's authorization application gates exactly.
Charges are now narrowed to authorizations that actually pass the chain-id, nonce, and code gates and therefore write the authority account.

Rex6 also moves authority state-growth resolution from pre-execution to validation, before the gas-limit and fee-affordability checks.
This lets the dynamic SALT account-creation gas for net-new authorities be folded into intrinsic gas and enforced against `gas_limit` and the sender's available balance before the sender is debited or the caller nonce is bumped, mirroring the existing per-`tx.kind` new-account storage-gas treatment.

### CREATE-Frame Resource Accounting

Rex6 corrects three independent accounting errors on the `CREATE` frame lifecycle:

- **Failed-`CREATE` result is no longer rewritten (M-47).** Pre-Rex6, a nested `CREATE` that halts with `OutOfGas` on the code-deposit storage charge had its result rewritten into the frame-local limit `Revert`. Because `Revert` returns the child's unused gas to the caller while `OutOfGas` burns it, an attacker could loop large-code `CREATE`s to dodge the code-deposit burn, and the observable failure class flipped from `OutOfGas` to `Revert`. Rex6 leaves an already-failed `CREATE` result unchanged.
- **Creator nonce-bump write booked to the parent frame (M-16).** The creator's nonce-bump account-info write is recorded in the parent frame's discardable lane (matching revm's nonce-bump revert semantics), instead of the child frame's, so it is no longer dropped when the child `CREATE` reverts.
- **`CREATE` state growth is conditional (M-29).** A `CREATE` records `+1` state growth only when the created address is net-new (empty), mirroring the value-transfer Call arm, instead of unconditionally.

Of the three, M-47 is the substantive fix; M-16 (under-count) and M-29 (over-count) are accounting-completeness corrections in the conservative direction.

### KeylessDeploy Sandbox Hardening

Rex6 closes two gaps in the [KeylessDeploy](../system-contracts/keyless-deploy.md) sandbox execution path:

- **Gas rescue on a transaction-level compute-gas halt.** Pre-Rex6, when a keyless-deploy dispatch exceeded the transaction-level compute-gas limit, it halted with a full-spend out-of-gas and did not rescue the outer sender's unused gas — so the sender lost the entire forwarded gas envelope for a halt that performed little work, unlike every ordinary opcode-dispatch path, which already rescued. Rex6 rescues the unused gas; the receipt still spends the full gas limit for replay stability, and the rescued amount is refunded to the sender.
- **Self-destructing constructor reported as empty-code.** Pre-Rex6, a keyless deploy whose constructor self-destructs (EIP-6780) yet returns non-empty bytecode was reported as a successful deployment, even though the merged on-chain account holds no code — and the signer's replay barrier was consumed. Rex6 classifies this as an empty-code deployment (`deployedAddress = 0x0`), matching the merged on-chain state.

### Post-Execution Fee-Reward Accounting

op-revm credits fee recipients (the priority-fee beneficiary and the base-fee / operator fee vaults) in a post-execution step that runs _after_ MegaETH's `AdditionalLimit` resource trackers are finalized for the transaction.
Pre-Rex6, an account that this step materializes for the first time — a previously non-existent fee recipient — was never counted toward [resource accounting](../evm/resource-accounting.md), because the trackers had already been read out.

Rex6 accounts for these post-execution materializations: a fee recipient that the reward step creates is counted toward state growth and account-update accounting, the same as any other new account. The deposit-mint half of this gap was already closed in Rex5; Rex6 covers the remaining non-deposit fee-credit paths.

### System-Originated Transaction Exemption

Before Rex6, protocol-mandated execution — the pre-block system calls ([EIP-2935](https://eips.ethereum.org/EIPS/eip-2935) block-hash, [EIP-4788](https://eips.ethereum.org/EIPS/eip-4788) beacon-root, and `SequencerRegistry.applyPendingChanges()`) and the sequencer's mega system transactions (such as oracle updates) — was metered exactly like a user transaction.
In particular, their storage writes were charged [SALT-scaled storage gas](../evm/resource-accounting.md) out of the transaction gas limit.
Because the SALT bucket multiplier grows without an upper bound, a sufficiently large bucket would make a single storage write exceed any fixed gas limit, causing the system call to run out of gas.
For the pre-block calls this rejects the entire block; for sequencer system transactions it fails an operation the sequencer assumes always succeeds.
The result is a protocol-level failure driven purely by how full the state has become.

Rex6 removes this failure mode: a system-originated transaction charges its storage writes at the **minimum bucket capacity** (so the cost no longer depends on the bucket), and the four [resource-limit dimensions](../evm/resource-accounting.md) plus [gas detention](../evm/gas-detention.md) are not enforced against it.
The standard EVM `gas_limit` still bounds the work as a runaway guard.

All consensus-visible changes are gated on the Rex6 spec.
Pre-Rex6 specs retain their existing metering order, per-authorization accounting, CREATE-frame accounting, KeylessDeploy sandbox behavior, post-execution fee-reward accounting, and full metering of system transactions unchanged.

## What Changed

### Unified Gas Metering Order

#### Previous behavior

Each storage-affecting opcode applied its own ad hoc ordering of storage-gas charging and compute-gas recording.
The prevailing pattern was: charge storage gas, run the opcode body, then record the body's compute gas once.
`SSTORE`, `LOG0`–`LOG4`, `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE`, and `SELFDESTRUCT` followed this pattern, and there was no specified rule requiring it.

#### New behavior

Every storage-affecting opcode (`SSTORE`, `LOG0`–`LOG4`, `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE`, `CREATE2`, `SELFDESTRUCT`) follows one fixed order:

1. Validate operands; a validation failure halts before any gas is charged or recorded.
2. Charge storage gas; an insufficient budget halts with `OutOfGas` before the body runs.
3. Execute the opcode body, including all standard EVM dynamic costs (memory expansion, account access, child-frame forwarding).
4. Record the opcode's compute gas exactly once, equal to the EVM gas consumed by steps 2–3 minus the storage gas charged in step 2 minus any gas forwarded to a child frame, then enforce the compute gas limit.
5. Apply resource-limit accounting (data size, key-value updates, state growth).

Compute gas is recorded in exactly one step, after the body has fully executed.
If the body does not run to completion, no compute gas is recorded for that opcode, even though any EVM gas consumed before the halt remains deducted from the transaction's gas budget.

### CREATE2 Memory-Expansion Gas

#### Previous behavior

`CREATE2` expands memory to read the initcode for its address computation before running the inner opcode.
The gas for that memory expansion was recorded as a **separate** compute-gas entry, recorded **before** the storage-gas charge and the inner opcode.
Consequently, when `CREATE2` halted between the memory expansion and the completion of its body (for example, on a compute-gas-limit halt, or a storage-gas-budget halt), the memory-expansion gas had already been recorded against the compute-gas limit.

#### New behavior

`CREATE2` records no separate memory-expansion compute-gas entry.
The memory-expansion gas is included in the single compute-gas recording taken after the inner opcode completes, alongside the inner opcode's own compute gas.

On a successful `CREATE2`, the total recorded compute gas is unchanged: the previous split (memory-expansion entry plus inner entry) and the new single entry sum to the same value, and `gas_used` is identical.
The behavior differs only when `CREATE2` halts between its memory expansion and the completion of its body: under the previous behavior the memory-expansion gas was recorded; under Rex6 it is not, because the body did not run to completion.

### Consolidated Applied-Authorization Scan in `validate`

#### Previous behavior (Rex5 and earlier)

Per-authorization effects were resolved in three separate places with different gating:

- Data-size `ACCOUNT_UPDATE_DATA_SIZE` and one KV update were charged in `before_tx_start` for every authorization whose authority address was recoverable — regardless of whether the authorization later passed the chain-id, nonce, or code application gates.
- State growth for net-new authority accounts was charged in pre-execution after the caller nonce bump, by a journal-aware scan that did mirror revm's application gates.
- Beneficiary access for an applied authority that equals the block beneficiary was not detected outside of opcode execution.

#### New behavior (Rex6)

A single journal-aware scan runs in `validate`, before the caller nonce bump and before the gas-limit / fee-affordability checks.
The scan mirrors revm's authorization application rules exactly: for each authorization entry, a node MUST apply the entry only when all of the following hold:

- the authorization chain ID is zero or equals the current chain ID,
- the authorization nonce is not `u64::MAX`,
- the authority address is recoverable from the authorization signature,
- the authority account code is empty or already an EIP-7702 delegation designation,
- the authorization nonce equals the authority account nonce at the point where application checks it.

Repeated authorizations targeting the same authority MUST be evaluated sequentially against a simulated authority nonce; the second and subsequent entries that apply observe the incremented nonce.
A node MUST not warm the authority account during the scan — revm's `apply_eip7702_auth_list` warms each authority immediately afterwards, so the warmed set at execution start is unchanged.

The scan emits the list of applied authorizations and, among those, the subset that materializes a previously non-existent authority account.
Every per-authorization charge described in the sections below is derived from this scan.

### Per-Authorization Charges Narrowed to Applied Authorizations

#### Previous behavior (Rex5 and earlier)

Data-size and KV-update charges for EIP-7702 authority account writes were applied to every authorization with a recoverable authority, including ones that were later skipped by the chain-id, nonce, or code gates.
Specifically, `before_tx_start` charged `ACCOUNT_UPDATE_DATA_SIZE` bytes of data size and one KV update per such recoverable authorization, regardless of whether the authorization ever wrote the authority account.

#### New behavior (Rex6)

A node MUST charge `ACCOUNT_UPDATE_DATA_SIZE` bytes of data size and one KV update only for each _applied_ authorization — one that passes all application gates and therefore writes the authority account.
A node MUST NOT charge data size or KV updates for an authorization that is skipped by any application gate.

When multiple authorizations target the same authority, each applied authorization MUST be charged independently (each one writes the authority account: delegation code plus nonce bump).
The per-record `AUTHORIZATION_DATA_SIZE × authorization_count` contribution is unchanged and continues to count every authorization in the list.

### Dynamic SALT Account-Creation Gas for Net-New Authorities

#### Previous behavior (Rex5 and earlier)

Net-new EIP-7702 authority accounts contributed to state growth (introduced in Rex5) but did not pay dynamic SALT new-account storage gas.
A transaction whose intrinsic + calldata + caller new-account storage gas already fit within `gas_limit` could still apply authorizations that materialized previously non-existent authority accounts at no incremental storage-gas cost, even though those accounts occupied real SALT buckets.

#### New behavior (Rex6)

For each applied EIP-7702 authorization that materializes a previously non-existent authority account, a node MUST charge dynamic new-account storage gas for that authority using the same SALT bucket pricing as other new-account materialization paths.
This charge MUST be folded into the transaction's intrinsic gas so that it is deducted from the top-level call frame's budget before the first call frame begins, and so that it is enforced against the transaction's `gas_limit` and the sender's available balance before the sender is debited.

If a `TxKind::Call(target)` value-transferring call would otherwise charge new-account storage gas for `target` and an applied EIP-7702 authorization in the same transaction also materializes `target`, the node MUST NOT charge the new-account gas twice.
The recipient-side new-account charge MUST be suppressed when `target` appears in the applied-authority net-new set, because the authority-side charge already covers the same account materialization.

### Beneficiary Detention on Applied Authority

#### Previous behavior (Rex5 and earlier)

Beneficiary [gas detention](../evm/gas-detention.md) was triggered only by opcode-level access to the beneficiary account (`BALANCE`, `SELFBALANCE`, `EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH`, transactions whose sender or recipient was the beneficiary, beneficiary access through `DELEGATECALL`) and by `SELFDESTRUCT` targeting the beneficiary.
An EIP-7702 authorization whose authority address equalled the block beneficiary did not trigger beneficiary detention even though applying it mutated the beneficiary's nonce and delegation code.

#### New behavior (Rex6)

A node MUST apply beneficiary gas detention when an applied EIP-7702 authorization — one that passes the chain-id, nonce, and code application gates and therefore writes the authority account — has an authority address equal to the block beneficiary.
The node MUST mark beneficiary access in the volatile-data tracker during the validate-time scan and re-derive the effective compute-gas detention cap before execution begins, even though no opcode in the existing trigger list was executed.

A skipped authorization whose authority equals the beneficiary MUST NOT trigger detention; only an applied authorization mutates beneficiary state.

### System-Originated Transaction Exemption

#### Previous behavior (Rex5 and earlier)

Protocol-mandated transactions were metered exactly like user transactions.
Their storage writes were charged SALT-scaled storage gas out of the transaction gas limit, the four resource-limit dimensions and gas detention were enforced against them, and a sufficiently large SALT bucket could push a single mandatory storage write out of gas — rejecting the block (pre-block system calls) or failing an operation the sequencer assumes always succeeds (mega system transactions).

#### New behavior (Rex6)

A transaction is **system-originated** when either:

- its caller is the EIP-2935 / EIP-4788 system address `0xfffffffffffffffffffffffffffffffffffffffe`, which is how the protocol issues its pre-block system calls; or
- it is a mega system transaction — a transaction from the current system address to a whitelisted system contract, recognized before or after its deposit promotion.

This deliberately excludes ordinary user deposit transactions, which remain fully metered.

Under Rex6, for a system-originated transaction:

- **Storage gas** (`SSTORE` of a zero→non-zero slot, new-account creation, and contract creation) is charged at the minimum bucket capacity (multiplier `1`).
  For the Rex-family storage-gas formula this is `0` additional storage gas, leaving only the standard EVM cost.
- The **data-size**, **KV-update**, **compute-gas**, and **state-growth** per-transaction limits are not enforced.
- **Gas detention** (the volatile-data-access compute-gas cap) is not enforced.

Resource **usage is still recorded** for these transactions; only the per-transaction halt decision is suppressed.
The standard EVM `gas_limit` — for the pre-block system calls, floored at the historical 30,000,000 — remains the only bound that can halt the transaction.

#### Why this is safe

The exempted dimensions either cannot grow unboundedly from external state or are bounded by the protocol:

- Only SALT-scaled storage gas is driven by an external, unbounded input (bucket capacity); charging it at the minimum capacity makes a system call's cost deterministic and independent of how full the state is.
- The four resource-limit dimensions are counts (bytes, slots, accounts) that the protocol controls by construction for its own transactions.
- The standard `gas_limit` continues to bound total work, so a buggy or runaway system contract still cannot consume unbounded resources.

User transactions are unaffected: they remain subject to SALT-scaled storage gas, all four resource-limit dimensions, and gas detention, preserving the anti-state-bloat purpose of SALT pricing.

## Developer Impact

For transactions that succeed, the unified metering order does not change `gas_used` or the compute gas a `CREATE2` records.
A transaction can observe a metering-order difference only if a `CREATE2` halts on a compute-gas-limit or storage-gas-budget boundary that falls between the opcode's memory expansion and the completion of its body; in that narrow case Rex6 records less compute gas for the halted `CREATE2` than prior specs.

Contracts that already account for EIP-7702 authority overhead per applied authorization (rather than per recoverable authority) see no change.
Transactions that previously included recoverable-but-skipped authorizations as deadweight to inflate their effective resource budget will see those skipped entries no longer counted against data size or KV updates; this is a relaxation of the prior charge, not a tightening.

Transactions that materialize new authority accounts via EIP-7702 now pay dynamic SALT account-creation gas for each net-new authority, folded into intrinsic gas.
A transaction that previously passed `gas_limit` validation by a thin margin and authorized one or more net-new authorities may now be rejected as a `CallGasCostMoreThanGasLimit` validation error before any execution begins.
Pre-Rex6 transactions whose authorizations target existing accounts are unaffected by this charge.

A transaction whose recipient is also materialized by one of its applied EIP-7702 authorizations no longer pays the new-account storage gas twice.

A transaction that applies an authorization whose authority is the block beneficiary is now subject to beneficiary gas detention, which caps the compute-gas budget available to the transaction's call frames.
Transactions targeting a known beneficiary as an authority should plan their compute footprint accordingly.

System-originated transactions are not user-constructible, so the exemption does not change any user-facing transaction outcome; it only removes a SALT-driven failure mode from protocol-mandated execution.

## Safety and Compatibility

All consensus-visible changes in Rex6 are gated on `MegaSpecId::REX6`.
Specs Rex5 and earlier are unaffected and produce identical results.

For the metering order, pre-Rex6 opcode handlers retain their exact prior ordering: most storage-affecting opcodes already charged storage gas before the body and recorded compute gas once afterward, and `CREATE2` keeps its separate, earlier memory-expansion compute-gas entry.

For EIP-7702 authorization accounting, pre-Rex6 specs retain their existing per-authorization accounting paths byte-for-byte:

- Pre-Rex6 continues to charge data size and KV updates for every recoverable authority in `before_tx_start` and to record net-new authority state growth in pre-execution.
- Pre-Rex6 does not charge dynamic SALT account-creation gas for net-new EIP-7702 authorities and does not trigger beneficiary detention on authority materialization.

The journal-aware scan that drives Rex6 accounting also serves as the implementation of the pre-Rex6 state-growth scan; the shared scanner is parameterized on whether the caller nonce has already been bumped at the call site (pre-Rex6 runs after the bump in pre-execution, Rex6 runs before the bump in validate).
This sharing is byte-for-byte equivalent to the prior standalone REX5 scanner for the state-growth counts it produces.

For the system-originated transaction exemption, pre-Rex6 specs continue to meter every transaction — including the protocol's own — identically; the exemption applies only when `MegaSpecId::REX6` is active.

Rex6 is the current unstable spec under active development; its semantics may still change before network activation.

## References

- [Dual Gas Model](../evm/dual-gas-model.md) — compute gas, storage gas, and the canonical metering order.
- [Resource Accounting](../evm/resource-accounting.md) — EIP-7702 authority data-size and KV-update narrowing; SALT-scaled storage gas.
- [Resource Limits](../evm/resource-limits.md) — the compute gas limit enforced after each opcode records its compute gas; authority state-growth resolution and dynamic SALT account-creation gas; the four resource-limit dimensions exempted for system transactions.
- [Gas Detention](../evm/gas-detention.md) — beneficiary detention trigger on applied authority; the volatile-data compute-gas cap exempted for system transactions.
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model.
- [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702) — Set Code transaction type.
- [Compute gas](../glossary.md#compute-gas)
- [Storage gas](../glossary.md#storage-gas)
- [mega-evm](https://github.com/megaeth-labs/mega-evm) — reference implementation.
