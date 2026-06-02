---
description: KeylessDeploy system contract semantics for deterministic deployment via pre-EIP-155 transactions.
spec: Rex4
---

# KeylessDeploy

This page specifies the KeylessDeploy system contract.
It defines the stable address, interception semantics, validation rules, sandbox execution model, and result semantics.

## Motivation

Nick's Method relies on a pre-signed pre-EIP-155 contract-creation transaction whose signer and deployment address are determined by the transaction contents.
On MegaETH, the original gas limit used on other EVM chains may be insufficient because execution cost differs.

The protocol therefore needs a mechanism that preserves the original signer and deployment address while allowing execution under an overridden gas limit.

## Specification

### Address

The KeylessDeploy system contract MUST exist at `KEYLESS_DEPLOY_ADDRESS`.

### Bytecode

A node MUST deploy the bytecode version corresponding to the active spec.

#### Version 1.0.0

Since: [Rex2](../upgrades/rex2.md)

Code hash: `0x55020d41649acf7a84add6e628b887f802218d9ac86f142ef0994da43ea5eeb6`

Deployed bytecode:

```
0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806354fd4d5014610038578063846365d514610080575b5f5ffd5b604080518082018252600581527f312e302e30000000000000000000000000000000000000000000000000000000602082015290516100779190610124565b60405180910390f35b61009361008e36600461013d565b6100a2565b604051610077939291906101af565b5f5f60606040517f1894f07600000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b5f81518084528060208401602086015e5f6020828601015260207fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011685010191505092915050565b602081525f61013660208301846100d8565b9392505050565b5f5f5f6040848603121561014f575f5ffd5b833567ffffffffffffffff811115610165575f5ffd5b8401601f81018613610175575f5ffd5b803567ffffffffffffffff81111561018b575f5ffd5b86602082840101111561019c575f5ffd5b6020918201979096509401359392505050565b67ffffffffffffffff8416815273ffffffffffffffffffffffffffffffffffffffff83166020820152606060408201525f6101ed60608301846100d8565b9594505050505056fea164736f6c634300081e000a
```

### Interception Scope

`keylessDeploy` is subject to [call interception](interception.md).
The call MUST be intercepted: the deployment logic described below executes instead of the on-chain bytecode.

The following preconditions MUST all be true for interception to fire:

- The call targets `KEYLESS_DEPLOY_ADDRESS`.
- The input matches the `keylessDeploy(bytes,uint256)` selector.
- The call is at depth zero (a direct top-level transaction call, not an internal call from another contract).

If any precondition is not met, the interceptor MUST fall through.
Non-intercepted calls MUST proceed to the on-chain bytecode, which MUST revert with `NotIntercepted()`.

### Interface

The KeylessDeploy contract MUST expose the following interface:

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress, bytes memory errorData);
}
```

### Accepted Inner Transaction Format

The `keylessDeploymentTransaction` argument MUST decode as a signed pre-EIP-155 legacy contract-creation transaction.
The following conditions MUST hold:

- the transaction encoding is valid RLP for `TxLegacy`,
- the transaction is a contract creation (`to = null`),
- the transaction has no chain ID,
- the transaction nonce is exactly `0`.

If any of those conditions fail, the call MUST revert with the corresponding validation error.

<details>
<summary>Rex5 (unstable): Trailing-bytes rejection</summary>

In addition to the conditions above, the encoding MUST contain no trailing bytes after the signed RLP payload.
If trailing bytes are present, the call MUST revert with `MalformedEncoding()`.

</details>

### Validation Rules

Before starting sandbox execution, the node MUST enforce the following checks:

1. the outer KeylessDeploy call carries zero ETH value,
2. `gasLimitOverride >= inner_tx.gas_limit`,
3. the signer can be recovered from the inner transaction signature,
4. the signer nonce in parent state is at most `1`,
5. the expected deployment address does not already contain code,
6. the signer has sufficient balance to cover the inner transaction's `value` plus, on pre-Rex5 specs only, `gasLimitOverride × gasPrice`,
7. the caller has sufficient remaining compute gas to pay `KEYLESS_DEPLOY_OVERHEAD_GAS`,
8. on Rex5+ only, the inner transaction's initcode length does not exceed `cfg.max_initcode_size()`,
9. on Rex5+ only and unless `cfg.disable_eip3607` is set, the recovered signer's parent-state bytecode is either empty or a valid EIP-7702 delegation designation.

The expected deployment address MUST be:

`keccak256(rlp([signer, 0]))[12:]`

### Sandbox Execution Model

If validation succeeds, the node MUST execute the inner deployment transaction inside a sandbox with the following properties:

- caller = recovered signer,
- transaction kind = contract creation,
- nonce = `0`,
- gas limit = `gasLimitOverride`,
- gas price = the inner transaction's gas price on pre-Rex5 specs; `0` on Rex5+,
- input = the inner transaction's initcode,
- value = the inner transaction's value,
- signer nonce in the sandbox view is overridden to `0`.

The sandbox MUST read from the parent transaction's current journal state.
The KeylessDeploy interceptor MUST be disabled inside the sandbox.

<details>
<summary>Rex5 (unstable)</summary>

`gasLimitOverride` MUST be capped to the parent transaction's remaining gas before sandbox execution starts.
The sandbox MUST use its own fresh resource trackers.
Those sandbox transaction limits MUST be capped to the parent transaction's remaining resource budgets before execution starts.
The capped budgets are the parent's current-call remaining compute gas and the parent's remaining transaction-level data size, KV updates, and state growth capacity.
Before sandbox execution starts, the node MUST preflight the sandbox transaction's known tx-level intrinsic usage against those capped budgets.
Known intrinsic usage includes intrinsic compute gas, base transaction data size, transaction input data size, caller account update usage, and any future tx-level persistent usage recorded before the first sandbox frame.
The sandbox limits MUST NOT subtract that intrinsic usage because the sandbox tracker records it during normal transaction startup.
If preflight fails, the node MUST NOT start sandbox execution.

The sandbox transaction MUST be processed as an OP deposit-like transaction: `deposit.source_hash` is set to a sandbox-specific marker, `gas_price` is `0`, and `deposit.mint` is `None`.
A consequence of `gas_price = 0` is that the `GASPRICE` opcode executed inside the sandbox constructor / init code returns `0`, regardless of the gas price encoded in the keyless transaction signature.
The deposit-style sandbox transaction MUST NOT mint ETH to the signer.

When op-revm's deposit `catch_error` converts a sandbox tx-validation failure into an `Ok(Halt(FailedDeposit))` result, the node MUST remap it to a sandbox `InvalidTransaction` outcome before merging state, so that the deposit-`catch_error` nonce bump applied inside the sandbox journal is dropped along with the rest of the sandbox state.

</details>

### State Merge Semantics

After sandbox execution completes, the sandbox state MUST be merged into the parent context on both sandbox success and sandbox execution failure, unless a spec-specific rule rejects the outer call before state merge.
That merged state includes the signer nonce update and any resulting deployed code or logs when applicable.
On pre-Rex5 specs, the merged state also includes the sandbox-internal signer balance deduction for gas; on Rex5+ the signer balance is unchanged by sandbox gas (the sandbox runs fee-free) — only the optional `value` transfer to the deployed contract changes the signer balance.

Validation failures MUST NOT merge sandbox state.

<details>
<summary>Rex5 (unstable)</summary>

The sandbox MUST enforce the capped resource budgets internally.
If the sandbox exceeds one of those budgets, the sandbox execution MUST fail inside the sandbox using the normal execution-failure path.

Before constructing the sandbox transaction, the node MUST read the parent-visible signer state (preferring the parent journal cache, falling back to the parent backing database; never going through the sandbox's nonce-overridden view) and, if the signer is unmaterialized, MUST debit the parent EVM gas meter by `new_account_storage_gas(deploy_signer)` and record a deposit-caller state-growth event.
A signer already materialised in parent state (e.g. nonce = 1 from a previous deploy) MUST NOT be charged.
The materialization charge is paid upfront — alongside `KEYLESS_DEPLOY_OVERHEAD_GAS` — and MUST be retained even when the sandbox subsequently validate-rejects or the outer call halts before sandbox state is merged.
DB read failure and dynamic storage-gas computation failure MUST be returned to the outer caller as the sandbox `InternalError` selector and MUST NOT start sandbox execution.

The node MUST then pre-debit the outer EVM gas meter by `gas_limit_override` (the capped sandbox gas envelope) before sandbox execution starts, mirroring revm's standard message-call shape (pre-debit on entry, refund unused on exit).
After sandbox execution completes, the node MUST merge the sandbox's resource usage into the parent transaction's resource trackers before merging sandbox state.
The node MUST also merge the sandbox's volatile-data-access bitmap into the parent `VolatileDataAccessTracker` before any post-sandbox halt decision.
Only the volatile-access footprint is merged; detention state such as the sandbox's compute-gas cap and `disableVolatileDataAccess()` scope MUST remain sandbox-local.
After the footprint merge, the node MUST refund the unused portion of the reservation (`gas_limit_override − sandbox_gas_used`) to the outer EVM gas meter.
In the normal case, sandbox success or frame-local revert, the merged usage fits inside the parent's remaining resource envelope because the sandbox was capped up front.
If the sandbox bails before producing a `SandboxOutcome` (sandbox-validate reject or internal error), the node MUST refund the full reservation; the upfront materialization and dispatch-overhead charges are retained.

Known pre-frame intrinsic overflow MUST be rejected by the preflight check before sandbox execution starts.
Residual edge cases can still exceed the parent's envelope after merge, such as a single-opcode overshoot at a TX-level compute-gas check or a future tx-level persistent accounting path that was not included in the preflight estimator.
In that case the node MUST NOT merge sandbox state.
The outer KeylessDeploy call MUST be rejected by rescuing any remaining outer gas for refund and halting the outer call with `OutOfGas` marked as exceeding the parent's TX-level limit.
This ensures no partial deployment state survives a parent-level reject, matching the ordinary revm convention that halted transactions commit only pre-execution state.

</details>

### Result Semantics

If sandbox execution succeeds and produces non-empty bytecode at the expected address, the call MUST return successfully with:

- `gasUsed = sandbox_gas_used`,
- `deployedAddress = expected_address`,
- `errorData = empty`.

If sandbox execution reverts, halts, or produces empty deployed bytecode, the outer KeylessDeploy call MUST still return successfully at the EVM level so that merged state persists, unless a spec-specific parent-level reject applies before state merge.

<details>
<summary>Rex5 (unstable)</summary>

This success-style return still applies when sandbox execution fails because the capped sandbox resource budgets are exceeded during normal (frame-local) execution.
In that case the sandbox fails internally and returns encoded `errorData`.

On Rex5, `gasUsed` in the KeylessDeploy return payload remains the sandbox's own `sandbox_gas_used`.
The outer transaction's EVM gas usage also includes `sandbox_gas_used`, in addition to the fixed dispatch overhead and ordinary outer-frame costs.
Under the deposit-style fee-free sandbox, the inner signer is NOT debited for sandbox gas: the outer transaction's own fee model is the sole fee source for the sandbox's EVM gas, and the inner signer's balance changes only by the inner transaction's `value` (zero for canonical Nick's-Method deployers).

The preflight check is a validation failure, not an execution failure: if the sandbox's known intrinsic usage alone exceeds the parent's remaining resource budget, no sandbox starts, no state is merged, and the outer call reverts with `ParentBudgetExceeded`.

The success-style return does NOT apply when the sandbox's merged TX-level usage overflows the parent's envelope after sandbox execution has already run.
In that case the outer call halts with `OutOfGas` and no sandbox state is merged.

</details>
In that case it MUST return:

- `gasUsed = sandbox_gas_used`,
- `deployedAddress = 0x0000000000000000000000000000000000000000`,
- `errorData = abi-encoded execution error`.

### Error Classes

Validation errors MUST revert the outer call.
Execution errors MUST return normally with encoded `errorData`.

The stable validation errors are:

- `MalformedEncoding()`
- `NotContractCreation()`
- `NotPreEIP155()`
- `NonZeroTxNonce(uint64 txNonce)`
- `NoEtherTransfer()`
- `InvalidSignature()`
- `InsufficientBalance()`
- `ContractAlreadyExists()`
- `SignerNonceTooHigh(uint64 signerNonce)`
- `GasLimitTooLow(uint64 txGasLimit, uint64 providedGasLimit)`
- `InsufficientComputeGas(uint64 limit, uint64 used)`
- `AddressMismatch()`
- `NoContractCreated()`
- `InternalError(string message)`
- `NotIntercepted()`
- `ParentBudgetExceeded(uint8 kind, uint64 limit, uint64 used)`

The stable execution errors are:

- `ExecutionReverted(uint64 gasUsed, bytes output)`
- `ExecutionHalted(uint64 gasUsed)`
- `EmptyCodeDeployed(uint64 gasUsed)`

<details>
<summary>Rex5 (unstable): Selector-only error refactor, new `InvalidTransaction()`, new `InitCodeTooLarge(uint64,uint64)`, new `SignerHasCode()`</summary>

On Rex5+ the stable validation error set is modified as follows:

- `InternalError(string message)` becomes `InternalError()` (selector-only).
  The `message` field MUST NOT be ABI-encoded.
  Root cause is reported off-chain via node logs.
- A new error `InvalidTransaction()` is added (selector-only).
  It MUST be returned when the sandbox `MegaHandler::validate` rejects the inner transaction before `pre_execution()` runs — typically the final Mega-side intrinsic / floor gas check, but structurally any outcome where `IsTxError::is_tx_error()` returns `true` or where op-revm's deposit `catch_error` synthesises a `FailedDeposit` halt.
  The outer KeylessDeploy call MUST revert with `InvalidTransaction()` and the signer MUST NOT be charged in this path: under Rex5 the sandbox runs fee-free so there is no gas debit to merge, and any `catch_error` nonce bump synthesised inside the sandbox journal MUST be dropped along with the discarded sandbox state.
- A new error `InitCodeTooLarge(uint64 size, uint64 max)` is added.
  It MUST be returned when the keyless transaction's initcode length exceeds `cfg.max_initcode_size()`.
  The sandbox enforces this limit itself because the deposit-style sandbox transaction bypasses op-revm's `validate_env`, where revm's EIP-3860-style check normally lives.
- A new error `SignerHasCode()` is added (selector-only).
  It MUST be returned when the recovered signer's parent-state bytecode is non-empty and is not a valid EIP-7702 delegation designation, unless `cfg.disable_eip3607` is set.
  This re-enforces the EIP-3607 caller-with-code rule that op-revm's deposit path otherwise skips inside `validate_account_nonce_and_code`, keeping the keyless-deploy validation surface aligned with the canonical revm path and with the Mega system-tx validation path.

`InvalidTransaction()`, `InternalError()`, and `SignerHasCode()` are selector-only because precompile return data is reachable on-chain via `RETURNDATACOPY` → `SSTORE` → state root.
Stringifying upstream revm/op-revm error wording into the wire payload would pin consensus to those crates' non-stability error surfaces.

</details>

## Constants

| Constant                      | Value                                        | Description                                                 |
| ----------------------------- | -------------------------------------------- | ----------------------------------------------------------- |
| `KEYLESS_DEPLOY_ADDRESS`      | `0x6342000000000000000000000000000000000003` | Stable KeylessDeploy system-contract address                |
| `KEYLESS_DEPLOY_OVERHEAD_GAS` | 100,000                                      | Fixed compute-gas overhead charged before sandbox execution |
| `KEYLESS_DEPLOY_VERSION`      | `1.0.0`                                      | Stable deployed bytecode version                            |

## Rationale

**Why intercept only at depth 0?**
If nested calls could invoke KeylessDeploy and later revert the outer context, the protocol could allow observation of deployment effects without guaranteeing that the signer remains charged.
Top-level interception prevents that pattern.

**Why merge state even on sandbox execution failure?**
If execution failures discarded state, an attacker could repeatedly trigger expensive keyless deployment attempts without reliably paying the signer-side cost.
Persisting the state effects of completed sandbox execution makes the charging behavior stable.

**Why allow signer nonce ≤ 1 instead of requiring 0?**
The signer may already have nonce `1` if the original keyless transaction was previously attempted directly and failed under MegaETH's gas regime.
Allowing nonce `1` preserves deployability in that case while still preventing arbitrary signer reuse.

## Spec History

- [Rex2](../upgrades/rex2.md) introduced KeylessDeploy and its stable top-level interception model.
- [Rex3](../upgrades/rex3.md) makes the overhead gas count toward compute gas accounting.
- [Rex5](../upgrades/rex5.md) (**unstable**) rejects encodings with trailing bytes after the signed RLP payload by reverting with `MalformedEncoding()`; propagates sandbox resource usage and volatile-access footprint to the parent transaction, charges sandbox EVM gas to the outer gas meter, caps `gasLimitOverride` to remaining gas, caps sandbox resource budgets to the parent's remaining limits before execution, preflights known sandbox intrinsic usage, and rejects the outer call without merging sandbox state on the residual overflow path; refactors `InternalError` to selector-only and adds the new selector-only `InvalidTransaction()` validation error.
- [Rex6](../upgrades/rex6.md) (**unstable**) rescues the outer sender's unused gas when a keyless-deploy dispatch exceeds the transaction-level compute-gas limit (pre-Rex6 halted full-spend without rescue, costing the sender the whole forwarded envelope; the receipt still spends the full gas limit and the rescued amount is refunded); and reports a deploy whose constructor self-destructs (EIP-6780) yet returns non-empty bytecode as an empty-code deployment (`deployedAddress = 0x0`) matching the merged on-chain state, instead of a success that consumed the signer's replay barrier.
