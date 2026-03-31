---
description: MegaETH system contract registry — addresses, whitelisting rules, and protocol-level execution constraints.
spec: Rex3
---

# System Contracts

This page specifies the system-contract registry and the protocol-level rules that apply to system contracts in MegaETH.
It defines the system-contract set.

## Specification

### Distinction from Precompiles

Both [precompiles](../evm/precompiles.md) and system contracts reside at fixed addresses and provide protocol-level functionality, but they differ in a fundamental way:

- **Precompiles** are stateless and pure.
  They perform a deterministic computation on their input and return output without reading or writing on-chain state.
  The EVM handles precompile calls entirely outside normal bytecode execution.
- **System contracts** are stateful.
  They have deployed bytecode, maintain persistent storage, and participate in normal EVM state access (including `SLOAD`, `SSTORE`, and account balance).
  Some of their functions are subject to [call interception](interception.md) for protocol-level side effects, but the contracts themselves exist as ordinary accounts with code and storage.

### Stable Registry

A node MUST recognize the following contracts as system contracts:

| Contract | Address | Since | Purpose |
| -------- | ------- | ----- | ------- |
| [Oracle](oracle.md) | `ORACLE_CONTRACT_ADDRESS` | [MiniRex](../upgrades/minirex.md) | Off-chain data key-value storage |
| [High-Precision Timestamp](high-precision-timestamp.md) | `HIGH_PRECISION_TIMESTAMP_ADDRESS` | [MiniRex](../upgrades/minirex.md) | Sub-second timestamp oracle service |
| [KeylessDeploy](keyless-deploy.md) | `KEYLESS_DEPLOY_ADDRESS` | [Rex2](../upgrades/rex2.md) | Deterministic cross-chain deployment |

### Deployment Semantics

System contracts MUST be available at their specified addresses when the corresponding spec is active.
Their availability is gated by [spec](../hardfork-spec.md) activation.

### Call Interception

Some system-contract functions are handled at the EVM level rather than exclusively by contract bytecode.
The generic interception mechanism — when it fires, how selectors are matched, how calls that do not match fall through to bytecode, and the gas and call-scheme rules — is specified on the [Call Interception](interception.md) page.

Each system-contract page defines which of its functions are intercepted and what each interception does:

- [Oracle — `sendHint`](oracle.md#hint-forwarding): performs a side effect (hint forwarding), then falls through to bytecode.
- [KeylessDeploy — `keylessDeploy`](keyless-deploy.md#interception-scope): intercepted at depth 0; executes deployment in a sandbox instead of on-chain bytecode.

### Backward Compatibility Rule

Any change to system-contract semantics, bytecode-visible interface behavior, or interception rules MUST be introduced by a new spec.
Stable behavior for an already-activated spec MUST remain unchanged.

<details>
<summary>Rex4 (unstable): Additional system contracts</summary>

For Rex4, a node MUST additionally recognize:

| Contract | Address | Purpose |
| -------- | ------- | ------- |
| MegaAccessControl | `MEGA_ACCESS_CONTROL_ADDRESS` | Volatile-data access control |
| MegaLimitControl | `MEGA_LIMIT_CONTROL_ADDRESS` | Query remaining compute-gas budget |

</details>

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `ORACLE_CONTRACT_ADDRESS` | `0x6342000000000000000000000000000000000001` | Stable Oracle system-contract address |
| `HIGH_PRECISION_TIMESTAMP_ADDRESS` | `0x6342000000000000000000000000000000000002` | Stable high-precision timestamp wrapper address |
| `KEYLESS_DEPLOY_ADDRESS` | `0x6342000000000000000000000000000000000003` | Stable KeylessDeploy system-contract address |
| `MEGA_ACCESS_CONTROL_ADDRESS` | `0x6342000000000000000000000000000000000004` | Unstable Rex4 MegaAccessControl address |
| `MEGA_LIMIT_CONTROL_ADDRESS` | `0x6342000000000000000000000000000000000005` | Unstable Rex4 MegaLimitControl address |

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the Oracle and High-Precision Timestamp contracts.
- [Rex2](../upgrades/rex2.md) introduced KeylessDeploy.
- [Rex4](../upgrades/rex4.md) adds unstable MegaAccessControl and MegaLimitControl contracts.
