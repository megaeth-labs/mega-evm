---
description: Rex6 network upgrade — EIP-7702 authorization accounting consolidated into validation, per-authorization data-size and KV-update charges narrowed to applied authorizations, dynamic SALT account-creation gas for net-new authorities, and beneficiary gas detention triggered when an applied authority equals the block beneficiary.
---

# Rex6 Network Upgrade

This page is an informative summary of the Rex6 specification.
For the full normative definition, see the Rex6 spec in the mega-evm repository.

{% hint style="warning" %}
**Unstable** — Rex6 is under active development.
Its semantics may still change before network activation.
{% endhint %}

## Summary

Rex6 consolidates EIP-7702 authorization accounting into a single applied-authorization scan that runs during transaction validation.
Pre-Rex6, per-authorization effects were split across three handler phases — data-size and KV-update charges in `before_tx_start`, state-growth charges in pre-execution after the caller nonce bump, and beneficiary detention only via opcode access — and used different gating criteria.
This split caused four metering bugs in which skipped authorizations were still charged, applied authorizations missed the beneficiary trigger, dynamic SALT gas for net-new authorities was not enforced against the gas limit, and a value-transfer recipient that an authorization materialized could be double-charged.

Rex6 derives every per-authorization effect — data size, KV update, state growth, dynamic new-account storage gas, and beneficiary detention — from a single journal-aware scan that mirrors revm's authorization application gates exactly.
Charges are now narrowed to authorizations that actually pass the chain-id, nonce, and code gates and therefore write the authority account.

Rex6 also moves authority state-growth resolution from pre-execution to validation, before the gas-limit and fee-affordability checks.
This lets the dynamic SALT account-creation gas for net-new authorities be folded into intrinsic gas and enforced against `gas_limit` and the sender's available balance before the sender is debited or the caller nonce is bumped, mirroring the existing per-`tx.kind` new-account storage-gas treatment.

All consensus-visible changes are gated on the Rex6 spec.
Pre-Rex6 specs retain their existing per-authorization accounting unchanged.

## What Changed

### 1. Consolidated Applied-Authorization Scan in `validate`

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

### 2. Per-Authorization Charges Narrowed to Applied Authorizations

#### Previous behavior (Rex5 and earlier)

Data-size and KV-update charges for EIP-7702 authority account writes were applied to every authorization with a recoverable authority, including ones that were later skipped by the chain-id, nonce, or code gates.
Specifically, `before_tx_start` charged `ACCOUNT_UPDATE_DATA_SIZE` bytes of data size and one KV update per such recoverable authorization, regardless of whether the authorization ever wrote the authority account.

#### New behavior (Rex6)

A node MUST charge `ACCOUNT_UPDATE_DATA_SIZE` bytes of data size and one KV update only for each _applied_ authorization — one that passes all application gates and therefore writes the authority account.
A node MUST NOT charge data size or KV updates for an authorization that is skipped by any application gate.

When multiple authorizations target the same authority, each applied authorization MUST be charged independently (each one writes the authority account: delegation code plus nonce bump).
The per-record `AUTHORIZATION_DATA_SIZE × authorization_count` contribution is unchanged and continues to count every authorization in the list.

### 3. Dynamic SALT Account-Creation Gas for Net-New Authorities

#### Previous behavior (Rex5 and earlier)

Net-new EIP-7702 authority accounts contributed to state growth (introduced in Rex5) but did not pay dynamic SALT new-account storage gas.
A transaction whose intrinsic + calldata + caller new-account storage gas already fit within `gas_limit` could still apply authorizations that materialized previously non-existent authority accounts at no incremental storage-gas cost, even though those accounts occupied real SALT buckets.

#### New behavior (Rex6)

For each applied EIP-7702 authorization that materializes a previously non-existent authority account, a node MUST charge dynamic new-account storage gas for that authority using the same SALT bucket pricing as other new-account materialization paths.
This charge MUST be folded into the transaction's intrinsic gas so that it is deducted from the top-level call frame's budget before the first call frame begins, and so that it is enforced against the transaction's `gas_limit` and the sender's available balance before the sender is debited.

If a `TxKind::Call(target)` value-transferring call would otherwise charge new-account storage gas for `target` and an applied EIP-7702 authorization in the same transaction also materializes `target`, the node MUST NOT charge the new-account gas twice.
The recipient-side new-account charge MUST be suppressed when `target` appears in the applied-authority net-new set, because the authority-side charge already covers the same account materialization.

### 4. Beneficiary Detention on Applied Authority

#### Previous behavior (Rex5 and earlier)

Beneficiary [gas detention](../evm/gas-detention.md) was triggered only by opcode-level access to the beneficiary account (`BALANCE`, `SELFBALANCE`, `EXTCODECOPY`, `EXTCODESIZE`, `EXTCODEHASH`, transactions whose sender or recipient was the beneficiary, beneficiary access through `DELEGATECALL`) and by `SELFDESTRUCT` targeting the beneficiary.
An EIP-7702 authorization whose authority address equalled the block beneficiary did not trigger beneficiary detention even though applying it mutated the beneficiary's nonce and delegation code.

#### New behavior (Rex6)

A node MUST apply beneficiary gas detention when an applied EIP-7702 authorization — one that passes the chain-id, nonce, and code application gates and therefore writes the authority account — has an authority address equal to the block beneficiary.
The node MUST mark beneficiary access in the volatile-data tracker during the validate-time scan and re-derive the effective compute-gas detention cap before execution begins, even though no opcode in the existing trigger list was executed.

A skipped authorization whose authority equals the beneficiary MUST NOT trigger detention; only an applied authorization mutates beneficiary state.

## Developer Impact

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
Pre-Rex6 specs (Rex5 and earlier) retain their existing per-authorization accounting paths byte-for-byte:

- Pre-Rex6 continues to charge data size and KV updates for every recoverable authority in `before_tx_start` and to record net-new authority state growth in pre-execution.
- Pre-Rex6 does not charge dynamic SALT account-creation gas for net-new EIP-7702 authorities and does not trigger beneficiary detention on authority materialization.

The journal-aware scan that drives Rex6 accounting also serves as the implementation of the pre-Rex6 state-growth scan; the shared scanner is parameterized on whether the caller nonce has already been bumped at the call site (pre-Rex6 runs after the bump in pre-execution, Rex6 runs before the bump in validate).
This sharing is byte-for-byte equivalent to the prior standalone REX5 scanner for the state-growth counts it produces.

Rex6 is the current unstable spec under active development; its semantics may still change before network activation.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Hardforks and Specs](../hardfork-spec.md) — spec progression and backward-compatibility model
- [Resource Accounting](../evm/resource-accounting.md) — EIP-7702 authority data-size and KV-update narrowing
- [Resource Limits](../evm/resource-limits.md) — authority state-growth resolution and dynamic SALT account-creation gas
- [Gas Detention](../evm/gas-detention.md) — beneficiary detention trigger on applied authority
- [EIP-7702](https://eips.ethereum.org/EIPS/eip-7702) — Set Code transaction type
- [Compute gas](../glossary.md#compute-gas)
- [Storage gas](../glossary.md#storage-gas)
