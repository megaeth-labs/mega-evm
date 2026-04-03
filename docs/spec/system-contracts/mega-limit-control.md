---
description: MegaLimitControl system contract — runtime query for effective remaining compute gas under detention and call-frame limits.
spec: Rex4
---

# MegaLimitControl

This page specifies the MegaLimitControl system contract.
It defines the address, interface, interception semantics, and the remaining-compute-gas query.

## Motivation

MegaETH's [gas detention](../evm/gas-detention.md) and [per-call-frame resource budgets](../evm/resource-limits.md#per-call-frame-runtime-budgets) both constrain a transaction's effective compute gas below the standard EVM gas limit.
The standard `GAS` opcode returns the remaining total gas, which does not reflect these MegaETH-specific constraints.

Contracts that perform gas-aware logic (e.g., batching operations until a threshold, deciding whether to attempt a costly sub-call) need a way to query their actual effective compute gas budget at runtime.

MegaLimitControl provides a single query that returns the effective remaining compute gas, accounting for both detention and call-frame limits.

## Specification

### Address

The MegaLimitControl system contract MUST exist at `MEGA_LIMIT_CONTROL_ADDRESS`.

### Bytecode

The contract takes no constructor arguments.
A node MUST deploy the bytecode version corresponding to the active spec.

Source: [`MegaLimitControl.sol`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/contracts/MegaLimitControl.sol)

#### Version 1.0.0

Since: [Rex4](../upgrades/rex4.md)

Code hash: `0x3927f2a4803c5e18153ff5742d0fa1acd9ad04538e4e6037cb4a9b28694ca87f`

Deployed bytecode:

```
0x608060405260043610610028575f3560e01c806302be4d841461005a57806354fd4d501461008c575b6040517f1894f07600000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b348015610065575f5ffd5b5061006e6100d7565b60405167ffffffffffffffff90911681526020015b60405180910390f35b348015610097575f5ffd5b50604080518082018252600581527f312e302e3000000000000000000000000000000000000000000000000000000060208201529051610083919061010a565b5f6040517f1894f07600000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f8301168401019150509291505056fea164736f6c634300081e000a
```

### Interface

```solidity
interface IMegaLimitControl {
    error NotIntercepted();
    error NonZeroTransfer();

    function remainingComputeGas() external view returns (uint64 remaining);
}
```

### Interception Scope

The `remainingComputeGas` function participates in [call interception](interception.md).
The node MUST intercept `CALL` and `STATICCALL` to `MEGA_LIMIT_CONTROL_ADDRESS` when the input matches the `remainingComputeGas()` selector.

`DELEGATECALL` and `CALLCODE` to this address MUST NOT be intercepted.
They fall through to the on-chain bytecode, which reverts with `NotIntercepted()`.

Unknown selectors MUST NOT be intercepted and MUST fall through to the on-chain bytecode.

### Value Transfer Policy

All intercepted functions MUST reject calls with non-zero value transfer.
If the call carries a non-zero transferred value, the node MUST revert with `NonZeroTransfer()`.

### `remainingComputeGas`

When intercepted, the node MUST return the effective remaining [compute gas](../glossary.md#compute-gas) for the caller's [call frame](../glossary.md#call-frame) at the time of the call.

The returned value MUST equal:

```
remaining = min(frame_remaining_compute_gas, tx_detained_remaining_compute_gas)
```

Where:

- `frame_remaining_compute_gas` is the caller's per-call-frame compute gas budget minus the compute gas already consumed in that frame.
- `tx_detained_remaining_compute_gas` is the transaction-level effective compute gas limit (after detention, if any) minus the transaction's total compute gas consumed so far.

The returned value is a point-in-time snapshot.
It decreases as execution proceeds.

## Constants

| Constant                     | Value                                        | Description                       |
| ---------------------------- | -------------------------------------------- | --------------------------------- |
| `MEGA_LIMIT_CONTROL_ADDRESS` | `0x6342000000000000000000000000000000000005` | MegaLimitControl contract address |

## Rationale

**Why a system contract instead of an EVM opcode?**
Effective remaining compute gas is a MegaETH-specific concept that combines detention and call-frame budgets.
Using a system contract provides a stable Solidity interface without introducing a non-standard opcode.

**Why return a single value instead of separate detention and frame budgets?**
Contracts that perform gas-aware logic need one number: "how much compute gas can I still use?"
Exposing the two components separately would push the `min()` calculation into every caller, adding complexity without benefit.
The combined value is the only operationally meaningful quantity.

**Why `uint64` instead of `uint256`?**
Compute gas is bounded by the transaction compute gas limit (200,000,000), which fits in `uint64`.
Using `uint64` avoids unnecessary padding and matches the natural width of gas values in the EVM implementation.

## Spec History

- [Rex4](../upgrades/rex4.md) introduced the MegaLimitControl system contract.
