---
description: KeylessDeploy system contract semantics for deterministic deployment via pre-EIP-155 transactions.
spec: Rex5
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

Deployed bytecode: `0x608060405234801561000f57...` (full bytecode: [`KeylessDeploy-1.0.0.json`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/artifacts/KeylessDeploy-1.0.0.json), `deployedBytecode` field).

To verify the code hash:

```bash
cast keccak $(jq -r .deployedBytecode crates/system-contracts/artifacts/KeylessDeploy-1.0.0.json)
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
- the transaction nonce is exactly `0`,
- the encoding contains no trailing bytes after the signed RLP payload.

If any of those conditions fail, the call MUST revert with the corresponding validation error.
If trailing bytes are present after the signed RLP payload, the call MUST revert with `MalformedEncoding()`.

### Validation Rules

Before starting sandbox execution, the node MUST enforce the following checks:

1. the outer KeylessDeploy call carries zero ETH value,
2. `gasLimitOverride >= inner_tx.gas_limit`,
3. the signer can be recovered from the inner transaction signature,
4. the signer nonce in parent state is at most `1`,
5. the expected deployment address does not already contain code,
6. the signer has sufficient balance to cover the inner transaction's `value` (the sandbox runs fee-free, so no gas component is included),
7. the caller has sufficient remaining compute gas to pay `KEYLESS_DEPLOY_OVERHEAD_GAS`,
8. the inner transaction's initcode length does not exceed the configured maximum initcode size,
9. unless EIP-3607 enforcement is disabled by node configuration, the recovered signer's parent-state bytecode is either empty or a valid EIP-7702 delegation designation.

The expected deployment address MUST be:

`keccak256(rlp([signer, 0]))[12:]`

### Sandbox Execution Model

If validation succeeds, the node MUST execute the inner deployment transaction inside a sandbox with the following properties:

- caller = recovered signer,
- transaction kind = contract creation,
- nonce = `0`,
- gas limit = `gasLimitOverride`, capped to the parent transaction's remaining gas before sandbox execution starts,
- gas price = `0` (the sandbox runs fee-free),
- input = the inner transaction's initcode,
- value = the inner transaction's value,
- signer nonce in the sandbox view is overridden to `0`.

The sandbox MUST read from the parent transaction's current journal state.
The KeylessDeploy interceptor MUST be disabled inside the sandbox.

The sandbox MUST use its own fresh resource trackers.
Those sandbox transaction limits MUST be capped to the parent transaction's remaining resource budgets before execution starts.
The capped budgets are the parent's current-call remaining compute gas and the parent's remaining transaction-level data size, KV updates, and state growth capacity.
Before sandbox execution starts, the node MUST preflight the sandbox transaction's known transaction-level intrinsic usage against those capped budgets.
Known intrinsic usage includes intrinsic compute gas, base transaction data size, transaction input data size, caller account update usage, and any other transaction-level persistent usage recorded before the first sandbox frame.
The sandbox limits MUST NOT subtract that intrinsic usage, because the sandbox tracker records it during normal transaction startup.
If preflight fails, the node MUST NOT start sandbox execution.

The sandbox transaction MUST be processed as a deposit-like transaction that is fee-free: the gas price is `0` and no ETH is minted to the signer.
A consequence of the zero gas price is that the `GASPRICE` opcode executed inside the sandbox constructor or init code returns `0`, regardless of the gas price encoded in the keyless transaction signature.

If a sandbox transaction-validation failure is internally converted into a deposit-style halt result, the node MUST remap it to a sandbox `InvalidTransaction` outcome before merging state, so that any nonce increment applied inside the sandbox journal as part of that conversion is dropped along with the rest of the sandbox state.

### State Merge Semantics

After sandbox execution completes, the sandbox state MUST be merged into the parent context on both sandbox success and sandbox execution failure, unless a spec-specific rule rejects the outer call before state merge.
That merged state includes the signer nonce update and any resulting deployed code or logs when applicable.
The signer balance is unchanged by sandbox gas, because the sandbox runs fee-free; only the optional `value` transfer to the deployed contract changes the signer balance.

Validation failures MUST NOT merge sandbox state.

The sandbox MUST enforce the capped resource budgets internally.
If the sandbox exceeds one of those budgets, the sandbox execution MUST fail inside the sandbox using the normal execution-failure path.

Before constructing the sandbox transaction, the node MUST read the parent-visible signer state without going through the sandbox's nonce-overridden view, and, if the signer is not yet materialized in parent state, MUST charge the parent gas meter the new-account storage gas for the signer and record a state-growth event for the deposit-style caller.
A signer already materialized in parent state (for example, with nonce `1` from a previous deploy attempt) MUST NOT be charged.
This materialization charge is paid upfront, alongside `KEYLESS_DEPLOY_OVERHEAD_GAS`, and MUST be retained even when the sandbox subsequently rejects the inner transaction or the outer call halts before sandbox state is merged.
A failure to read parent signer state or to compute the dynamic storage gas MUST be reported to the outer caller as the `InternalError()` selector and MUST NOT start sandbox execution.

The node MUST then pre-debit the outer gas meter by the capped sandbox gas envelope (`gasLimitOverride`) before sandbox execution starts, and refund the unused portion on exit.
After sandbox execution completes, the node MUST merge the sandbox's resource usage into the parent transaction's resource trackers before merging sandbox state.
The node MUST also merge the sandbox's volatile-data-access footprint into the parent's volatile-data-access tracking before any post-sandbox halt decision.
Only the volatile-access footprint is merged; detention state such as the sandbox's compute-gas cap and any in-sandbox disabling of volatile-data access MUST remain sandbox-local.
After the footprint merge, the node MUST refund the unused portion of the reservation (`gasLimitOverride − sandbox_gas_used`) to the outer gas meter.
In the normal case — sandbox success or a frame-local revert — the merged usage fits inside the parent's remaining resource envelope because the sandbox was capped up front.
If the sandbox bails before producing a result (an inner transaction-validation rejection or internal error), the node MUST refund the full reservation; the upfront materialization and dispatch-overhead charges are retained.

Known pre-frame intrinsic overflow MUST be rejected by the preflight check before sandbox execution starts.
Residual edge cases can still exceed the parent's envelope after merge, such as a single-opcode overshoot at a transaction-level compute-gas check or another transaction-level persistent accounting path not included in the preflight estimate.
In that case the node MUST NOT merge sandbox state.
The outer KeylessDeploy call MUST be rejected by rescuing any remaining outer gas for refund and halting the outer call with `OutOfGas` marked as exceeding the parent's transaction-level limit.
This ensures no partial deployment state survives a parent-level reject, consistent with the convention that halted transactions commit only pre-execution state.

### Result Semantics

If sandbox execution succeeds and produces non-empty bytecode at the expected address, the call MUST return successfully with:

- `gasUsed = sandbox_gas_used`,
- `deployedAddress = expected_address`,
- `errorData = empty`.

If sandbox execution reverts, halts, or produces empty deployed bytecode, the outer KeylessDeploy call MUST still return successfully at the EVM level so that merged state persists, unless a parent-level reject applies before state merge.
In that failure case the call MUST return:

- `gasUsed = sandbox_gas_used`,
- `deployedAddress = 0x0000000000000000000000000000000000000000`,
- `errorData = abi-encoded execution error`.

This success-style return also applies when sandbox execution fails because the capped sandbox resource budgets are exceeded during normal (frame-local) execution: the sandbox fails internally and returns encoded `errorData`.

`gasUsed` in the KeylessDeploy return payload is the sandbox's own `sandbox_gas_used`.
The outer transaction's gas usage also includes `sandbox_gas_used`, in addition to the fixed dispatch overhead and ordinary outer-frame costs.
Under the fee-free sandbox, the inner signer is NOT debited for sandbox gas: the outer transaction's own fee model is the sole fee source for the sandbox's gas, and the inner signer's balance changes only by the inner transaction's `value` (zero for canonical Nick's-Method deployers).

When the inner CREATE succeeds but produces empty runtime bytecode, the node MUST forward the constructor's emitted logs — emitted by the outer call — before returning the `EmptyCodeDeployed(uint64 gasUsed)` result, so that the receipt logs agree with the merged state.

Two cases do NOT use the success-style return:

- If the sandbox's known intrinsic usage alone exceeds the parent's remaining resource budget, the preflight check fails before any sandbox starts: no state is merged, and the outer call reverts with `ParentBudgetExceeded`.
- If the sandbox's merged transaction-level usage overflows the parent's envelope after sandbox execution has already run, the outer call halts with `OutOfGas` and no sandbox state is merged.

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
- `InternalError()`
- `InvalidTransaction()`
- `InitCodeTooLarge(uint64 size, uint64 max)`
- `SignerHasCode()`
- `NotIntercepted()`
- `ParentBudgetExceeded(uint8 kind, uint64 limit, uint64 used)`

The stable execution errors are:

- `ExecutionReverted(uint64 gasUsed, bytes output)`
- `ExecutionHalted(uint64 gasUsed)`
- `EmptyCodeDeployed(uint64 gasUsed)`

`InternalError()` is selector-only: it MUST NOT ABI-encode any message field.
It MUST be returned when the node cannot read parent signer state or cannot compute the dynamic storage gas required before sandbox execution.

`InvalidTransaction()` is selector-only.
It MUST be returned when the sandbox rejects the inner transaction during validation, before execution begins.
The outer KeylessDeploy call MUST revert with `InvalidTransaction()`, and the signer MUST NOT be charged in this path: the sandbox runs fee-free, so there is no gas debit to merge, and any nonce increment synthesized inside the sandbox journal MUST be dropped along with the discarded sandbox state.

`InitCodeTooLarge(uint64 size, uint64 max)` MUST be returned when the keyless transaction's initcode length exceeds the configured maximum initcode size.

`SignerHasCode()` is selector-only.
It MUST be returned when the recovered signer's parent-state bytecode is non-empty and is not a valid EIP-7702 delegation designation, unless EIP-3607 enforcement is disabled by node configuration.
This enforces the EIP-3607 caller-with-code rule, keeping the keyless-deploy validation surface aligned with the canonical transaction validation path.

`InvalidTransaction()`, `InternalError()`, and `SignerHasCode()` are selector-only so the top-level KeylessDeploy error ABI stays stable and does not depend on upstream internal error text.
The KeylessDeploy interceptor is active only at call depth 0; calls from contracts are not intercepted and MUST revert through the on-chain `NotIntercepted()` path, so sandbox validation/internal-error returndata is not observable by an inner caller's `RETURNDATASIZE` or `RETURNDATACOPY` instructions.
This top-level returndata can affect RPC responses, traces, relayer decoders, and exact-output replay tooling, but it is not included in transaction receipts and cannot be copied into contract storage through KeylessDeploy's intercepted path.

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
- [Rex5](../upgrades/rex5.md) rejects encodings with trailing bytes after the signed RLP payload by reverting with `MalformedEncoding()`; propagates sandbox resource usage and volatile-access footprint to the parent transaction, charges sandbox EVM gas to the outer gas meter, caps `gasLimitOverride` to remaining gas, caps sandbox resource budgets to the parent's remaining limits before execution, preflights known sandbox intrinsic usage, and rejects the outer call without merging sandbox state on the residual overflow path; refactors `InternalError` to selector-only and adds the new selector-only `InvalidTransaction()` validation error; and forwards the constructor's logs when an inner deployment succeeds with empty runtime code.
