---
description: Rex6 network upgrade — unified per-opcode gas metering order (storage gas charged before the opcode body, compute gas recorded exactly once after it completes), plus EIP-7702 authorization accounting consolidated into validation, per-authorization data-size and KV-update charges narrowed to applied authorizations, dynamic SALT account-creation gas for net-new authorities, and beneficiary gas detention triggered when an applied authority equals the block beneficiary.
---

# Rex6 Network Upgrade

This page is an informative summary of the Rex6 specification.
For the full normative definition, see the Rex6 spec in the mega-evm repository.

{% hint style="warning" %}
**Unstable** — Rex6 is under active development.
Its semantics may still change before network activation.
{% endhint %}

## Summary

Rex6 bundles two consensus-visible changes to gas and resource accounting:

1. **Unified per-opcode gas metering order.** Rex6 defines a single, canonical order in which every storage-affecting opcode charges [storage gas](../glossary.md#storage-gas) and records [compute gas](../glossary.md#compute-gas), and brings `CREATE2` under it.
2. **Consolidated EIP-7702 authorization accounting.** Rex6 derives every per-authorization effect from a single applied-authorization scan that runs during transaction validation.

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

All consensus-visible changes are gated on the Rex6 spec.
Pre-Rex6 specs retain their existing metering order and per-authorization accounting unchanged.

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

## Safety and Compatibility

All consensus-visible changes in Rex6 are gated on `MegaSpecId::REX6`.
Specs Rex5 and earlier are unaffected and produce identical results.

For the metering order, pre-Rex6 opcode handlers retain their exact prior ordering: most storage-affecting opcodes already charged storage gas before the body and recorded compute gas once afterward, and `CREATE2` keeps its separate, earlier memory-expansion compute-gas entry.

For EIP-7702 authorization accounting, pre-Rex6 specs retain their existing per-authorization accounting paths byte-for-byte:

- Pre-Rex6 continues to charge data size and KV updates for every recoverable authority in `before_tx_start` and to record net-new authority state growth in pre-execution.
- Pre-Rex6 does not charge dynamic SALT account-creation gas for net-new EIP-7702 authorities and does not trigger beneficiary detention on authority materialization.

The journal-aware scan that drives Rex6 accounting also serves as the implementation of the pre-Rex6 state-growth scan; the shared scanner is parameterized on whether the caller nonce has already been bumped at the call site (pre-Rex6 runs after the bump in pre-execution, Rex6 runs before the bump in validate).
This sharing is byte-for-byte equivalent to the prior standalone REX5 scanner for the state-growth counts it produces.

Rex6 is the current unstable spec under active development; its semantics may still change before network activation.

## References

- [Dual Gas Model](../evm/dual-gas-model.md) — compute gas, storage gas, and the canonical metering order.
- [Resource Accounting](../evm/resource-accounting.md) — EIP-7702 authority data-size and KV-update narrowing.
- [Resource Limits](../evm/resource-limits.md) — the compute gas limit enforced after each opcode records its compute gas; authority state-growth resolution and dynamic SALT account-creation gas.
- [Gas Detention](../evm/gas-detention.md) — beneficiary detention trigger on applied authority.
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model.
- [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702) — Set Code transaction type.
- [Compute gas](../glossary.md#compute-gas)
- [Storage gas](../glossary.md#storage-gas)
- [mega-evm](https://github.com/megaeth-labs/mega-evm) — reference implementation.
