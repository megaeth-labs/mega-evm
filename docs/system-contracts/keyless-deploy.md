---
description: KeylessDeploy system contract semantics for deterministic deployment via pre-EIP-155 transactions.
spec: Rex3
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

Source: [`KeylessDeploy.sol`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/contracts/KeylessDeploy.sol)

| Version | Code Hash | Since |
| ------- | --------- | ----- |
| `1.0.0` | `0x55020d41649acf7a84add6e628b887f802218d9ac86f142ef0994da43ea5eeb6` | [Rex2](../upgrades/rex2.md) |

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

### Validation Rules

Before starting sandbox execution, the node MUST enforce the following checks:

1. the outer KeylessDeploy call carries zero ETH value,
2. `gasLimitOverride >= inner_tx.gas_limit`,
3. the signer can be recovered from the inner transaction signature,
4. the signer nonce in parent state is at most `1`,
5. the expected deployment address does not already contain code,
6. the signer has sufficient balance to cover `gasLimitOverride × gasPrice + value`,
7. the caller has sufficient remaining compute gas to pay `KEYLESS_DEPLOY_OVERHEAD_GAS`.

The expected deployment address MUST be:

`keccak256(rlp([signer, 0]))[12:]`

### Sandbox Execution Model

If validation succeeds, the node MUST execute the inner deployment transaction inside a sandbox with the following properties:

- caller = recovered signer,
- transaction kind = contract creation,
- nonce = `0`,
- gas limit = `gasLimitOverride`,
- gas price = the inner transaction's gas price,
- input = the inner transaction's initcode,
- value = the inner transaction's value,
- signer nonce in the sandbox view is overridden to `0`.

The sandbox MUST read from the parent transaction's current journal state.
The KeylessDeploy interceptor MUST be disabled inside the sandbox.

### State Merge Semantics

After sandbox execution completes, the sandbox state MUST be merged into the parent context on both sandbox success and sandbox execution failure.
That merged state includes signer balance deduction, signer nonce update, and any resulting deployed code or logs when applicable.

Validation failures MUST NOT merge sandbox state.

### Result Semantics

If sandbox execution succeeds and produces non-empty bytecode at the expected address, the call MUST return successfully with:

- `gasUsed = sandbox_gas_used`,
- `deployedAddress = expected_address`,
- `errorData = empty`.

If sandbox execution reverts, halts, or produces empty deployed bytecode, the outer KeylessDeploy call MUST still return successfully at the EVM level so that merged state persists.
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

The stable execution errors are:

- `ExecutionReverted(uint64 gasUsed, bytes output)`
- `ExecutionHalted(uint64 gasUsed)`
- `EmptyCodeDeployed(uint64 gasUsed)`

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `KEYLESS_DEPLOY_ADDRESS` | `0x6342000000000000000000000000000000000003` | Stable KeylessDeploy system-contract address |
| `KEYLESS_DEPLOY_OVERHEAD_GAS` | 100,000 | Fixed compute-gas overhead charged before sandbox execution |
| `KEYLESS_DEPLOY_VERSION` | `1.0.0` | Stable deployed bytecode version |

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
