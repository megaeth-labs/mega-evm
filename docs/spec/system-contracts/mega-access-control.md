---
description: MegaAccessControl system contract — proactive volatile data access restriction for call subtrees.
spec: Rex4
---

# MegaAccessControl

This page specifies the MegaAccessControl system contract.
It defines the address, interface, interception semantics, and the volatile-data access restriction mechanism.

## Motivation

[Gas detention](../evm/gas-detention.md) is reactive — it caps remaining compute gas only after volatile data has already been accessed.
Contracts that call untrusted code have no way to prevent that code from silently triggering detention and tightening the caller's gas budget.

MegaAccessControl provides a proactive mechanism: a contract can disable volatile data access for its entire call subtree before any untrusted code runs.
Attempts to access volatile data while disabled revert immediately, preventing both the access and the detention side effect.

## Specification

### Address

The MegaAccessControl system contract MUST exist at `MEGA_ACCESS_CONTROL_ADDRESS`.

### Bytecode

The contract takes no constructor arguments.
A node MUST deploy the bytecode version corresponding to the active spec.

Source: [`MegaAccessControl.sol`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/contracts/MegaAccessControl.sol)

#### Version 1.0.0

Since: [Rex4](../upgrades/rex4.md)

Code hash: `0x96d0f3ba6b474e9684a97e4388ec8dbb7929818b51e6b4f19f885ccbdf642f9b`

Deployed bytecode:

```
0x60806040526004361061003e575f3560e01c8063143d63fd1461007057806315f952801461008657806354fd4d50146100af578063c62e8daf14610070575b6040517f1894f07600000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b34801561007b575f5ffd5b5061008461003e565b005b348015610091575f5ffd5b5061009a6100fa565b60405190151581526020015b60405180910390f35b3480156100ba575f5ffd5b50604080518082018252600581527f312e302e30000000000000000000000000000000000000000000000000000000602082015290516100a6919061012d565b5f6040517f1894f07600000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f8301168401019150509291505056fea164736f6c634300081e000a
```

### Interface

```solidity
interface IMegaAccessControl {
    enum VolatileDataAccessType {
        BlockNumber,    // 0  — NUMBER opcode
        Timestamp,      // 1  — TIMESTAMP opcode
        Coinbase,       // 2  — COINBASE opcode
        Difficulty,     // 3  — DIFFICULTY opcode
        GasLimit,       // 4  — GASLIMIT opcode
        BaseFee,        // 5  — BASEFEE opcode
        PrevRandao,     // 6  — PREVRANDAO opcode
        BlockHash,      // 7  — BLOCKHASH opcode
        BlobBaseFee,    // 8  — BLOBBASEFEE opcode
        BlobHash,       // 9  — BLOBHASH opcode
        Beneficiary,    // 10 — account access targeting the block beneficiary
        Oracle          // 11 — SLOAD on oracle contract
    }

    error NotIntercepted();
    error NonZeroTransfer();
    error VolatileDataAccessDisabled(VolatileDataAccessType accessType);
    error DisabledByParent();

    function disableVolatileDataAccess() external view;
    function enableVolatileDataAccess() external view;
    function isVolatileDataAccessDisabled() external view returns (bool disabled);
}
```

### Interception Scope

All three functions (`disableVolatileDataAccess`, `enableVolatileDataAccess`, `isVolatileDataAccessDisabled`) participate in [call interception](interception.md).
The node MUST intercept `CALL` and `STATICCALL` to `MEGA_ACCESS_CONTROL_ADDRESS` when the input matches a known selector.

`DELEGATECALL` and `CALLCODE` to this address MUST NOT be intercepted.
They fall through to the on-chain bytecode, which reverts with `NotIntercepted()`.

Unknown selectors MUST NOT be intercepted and MUST fall through to the on-chain bytecode.

### Value Transfer Policy

All intercepted functions MUST reject calls with non-zero value transfer.
If the call carries a non-zero transferred value, the node MUST revert with `NonZeroTransfer()`.

### `disableVolatileDataAccess`

When intercepted, the node MUST disable volatile data access for the caller's [call frame](../glossary.md#call-frame) and all descendant call frames.

While disabled, any volatile data access — block environment reads, beneficiary-targeted account access (including `SELFDESTRUCT` to the beneficiary), and [oracle](oracle.md) storage reads — MUST revert immediately with `VolatileDataAccessDisabled(VolatileDataAccessType accessType)`.

Blocked volatile access MUST NOT update volatile-access tracking and MUST NOT tighten [gas detention](../evm/gas-detention.md).

### `enableVolatileDataAccess`

When intercepted, the node MUST re-enable volatile data access for the caller's call frame and descendant call frames if and only if the restriction was set at the caller's depth or was not active.

If the restriction was set by an ancestor call frame (a parent at a shallower depth), the node MUST revert with `DisabledByParent()`.

### `isVolatileDataAccessDisabled`

When intercepted, the node MUST return `true` if volatile data access is currently disabled for the caller's call frame (whether disabled by the caller or an ancestor call frame), and `false` otherwise.

### Lifetime

The volatile data access restriction automatically ends when the call frame that called `disableVolatileDataAccess` returns.
No explicit cleanup is needed.

## Constants

| Constant                      | Value                                        | Description                        |
| ----------------------------- | -------------------------------------------- | ---------------------------------- |
| `MEGA_ACCESS_CONTROL_ADDRESS` | `0x6342000000000000000000000000000000000004` | MegaAccessControl contract address |

## Rationale

**Why a system contract instead of an EVM opcode?**
Volatile data access control is a MegaETH-specific mechanism with no Ethereum precedent.
Using a system contract avoids polluting the opcode space and provides a stable Solidity interface that existing toolchains can use without modifications.

**Why revert instead of silently blocking?**
Silent blocking would hide bugs.
If a contract expects to read block data but the access is silently suppressed, the contract would receive stale or zero values and proceed with incorrect state.
Reverting makes the restriction visible and forces the caller to handle it explicitly.

**Why prevent descendant re-enabling when a parent disabled access?**
Allowing untrusted child code to re-enable access would defeat the purpose.
The calling contract disables access precisely because it does not trust inner calls to behave correctly with volatile data.
The parent-override rule preserves the caller's intent.

## Spec History

- [Rex4](../upgrades/rex4.md) introduced the MegaAccessControl system contract.
