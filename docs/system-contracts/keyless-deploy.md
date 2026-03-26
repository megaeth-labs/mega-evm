# Keyless Deployment

## Overview

**Keyless Deployment** (Nick's Method) allows deploying contracts to the same address on every EVM chain — without anyone needing to hold the deployer's private key.

**Address**: `0x6342000000000000000000000000000000000003`

**Source**: [`KeylessDeploy.sol`](https://github.com/megaeth-labs/mega-evm/blob/main/crates/system-contracts/contracts/KeylessDeploy.sol)

**Available**: [Rex2](../hardfork-spec.md#rex2) hardfork onwards

## Background: Nick's Method

[Nick's Method](https://medium.com/patronum-labs/nicks-method-ethereum-keyless-execution-168a6659479c) enables deterministic contract deployment across EVM chains:

1. Construct a contract creation transaction (with `to = null`, `nonce = 0`, and a chosen gas limit, gas price, and init code) — but do **not** sign it yet.
2. Generate a **random** signature (`v`, `r`, `s`) with `v = 27` or `28` (pre-EIP-155 format).
3. Recover the signer address from the random signature via `ecrecover` — no one knows this signer's private key.
4. Fund the signer address with exactly enough ETH to cover the transaction fee.
5. Broadcast the pre-signed transaction. It executes as a normal contract creation from the recovered signer.

The deployed contract address is deterministic: `keccak256(rlp([signer, 0]))[12:]` — derived from the signer address and nonce 0.
Because the signer is derived from the transaction content and signature, the same transaction produces the same signer and the same contract address on every EVM chain.
No one can forge another transaction from this signer because no one holds the private key, and the signer's nonce can never be anything other than 0.

## The Problem on MegaETH

MegaETH's [gas model](../evm/dual-gas-model.md) charges [storage gas](../glossary.md#storage-gas) on top of standard [compute gas](../glossary.md#compute-gas), making many operations more expensive than on Ethereum.
Contracts deployed via Nick's Method on Ethereum may run out of gas on MegaETH because the original transaction's gas limit is insufficient.

With Nick's Method, **any change to the signed transaction fields changes the recovered signer**, which changes the deployment address.
The gas limit cannot simply be increased.

## The Solution

The `KeylessDeploy` system contract wraps the original keyless transaction in a sandbox execution with an overridden gas limit, preserving the original signer and deployment address.

## Interface

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress, bytes memory errorData);
}
```

**Parameters**:
- `keylessDeploymentTransaction` — The original RLP-encoded pre-EIP-155 signed transaction (unmodified)
- `gasLimitOverride` — Gas limit for execution (must be >= the transaction's original gas limit)

**Returns**:
- `gasUsed` — Gas consumed by the deployment
- `deployedAddress` — Deployed contract address (zero if failed)
- `errorData` — ABI-encoded error if failed, empty on success

## Transaction Format

The `keylessDeploymentTransaction` must be a pre-[EIP-155](https://eips.ethereum.org/EIPS/eip-155) legacy transaction ([EIP-2718](https://eips.ethereum.org/EIPS/eip-2718) typed envelopes are rejected).
The RLP-encoded bytes must contain the following fields:

| Field    | Requirement                                                |
| -------- | ---------------------------------------------------------- |
| nonce    | Must be 0                                                  |
| gasPrice | Any value (used as-is for signer gas charging)             |
| gasLimit | Any value (must be ≤ `gasLimitOverride`)                   |
| to       | Must be empty (contract creation)                          |
| value    | ETH to transfer to created contract (deducted from signer) |
| data     | Contract initialization bytecode                           |
| v        | 27 or 28 (pre-EIP-155, no chain ID)                        |
| r, s     | Signature components (randomly generated — no private key) |

## Execution Flow

The contract intercepts calls at depth 0 only (direct transaction calls).
DELEGATECALL, CALLCODE, and non-top-level calls fall through to on-chain bytecode, which would revert.

{% hint style="info" %}
**Why depth 0 only?** If a contract could call `keylessDeploy`, observe the result, and then revert the outer call, the signer's gas charge would be rolled back — enabling free spam.
By requiring depth 0, the sandbox result IS the transaction result; there is no outer context that can revert after the signer has been charged.
{% endhint %}

When `keylessDeploy` is called:

1. **Charge overhead** — A fixed 100,000 [compute gas](../glossary.md#compute-gas) overhead is deducted from the caller's gas for RLP decoding, signature recovery, and state filtering.
2. **Reject value transfer** — The `keylessDeploy` call itself must not include ETH value (`msg.value` must be 0). This is separate from the `value` field in the inner keyless transaction — see step 6.
3. **Decode and validate the keyless transaction** — RLP-decode the pre-EIP-155 transaction bytes. Reject if `to` is not null (must be a contract creation), nonce is not 0, or `v` is not 27 or 28.
4. **Validate gas limit override** — `gasLimitOverride` must be ≥ the transaction's original gas limit.
5. **Recover signer and compute deploy address** — Recover the signer via `ecrecover`. Compute the deploy address as `keccak256(rlp([signer, 0]))[12:]`. Reject if the signer's on-chain nonce is > 1 or the deploy address already has code.

{% hint style="info" %}
**Why nonce ≤ 1, not just 0?** Someone may have attempted to broadcast the original keyless transaction directly on MegaETH before using this system contract.
That transaction fails due to higher gas costs, but the attempt still increments the signer's nonce from 0 to 1.
Allowing nonce ≤ 1 means keyless deploy still works after such a failed direct broadcast.
{% endhint %}
6. **Execute in a sandbox** — Launch an isolated EVM execution with:
   - The same block environment and spec as the outer transaction
   - **State**: the sandbox reads from the outer transaction's current journal state (including any changes made earlier in the same transaction), not a snapshot of pre-transaction state. All account state is unmodified except: the signer's nonce is **always overridden to 0** in the sandbox's view, regardless of the signer's actual on-chain nonce. This is required because Nick's Method relies on `nonce = 0` to produce the correct deployment address.
   - **Transaction fields**: `caller` = recovered signer, `nonce` = 0, `gas_limit` = `gasLimitOverride`, `gas_price` = original transaction's gas price (used as-is — not validated against base fee, not subject to EIP-1559, not split between L1/L2 fees), `input` and `value` from the original transaction
   - **Value transfer**: if the original keyless transaction has a non-zero `value` field, that ETH is transferred from the signer to the newly created contract. On execution failure (init code revert or out of gas), the value transfer is automatically undone by the EVM.
   - **Balance check**: the signer must have sufficient balance to cover `gasLimitOverride × gasPrice + value`
   - **Isolation**: the `KeylessDeploy` interceptor is disabled inside the sandbox (no recursive keyless deploys). Accounts accessed by the sandbox do not affect warm/cold tracking in the outer transaction.
7. **Apply state changes** — On both success and failure, all sandbox state changes are merged into the parent journal. This includes: signer balance deduction (`gasUsed × gasPrice`), signer nonce set to 1 (incremented from the overridden 0), and deployed contract code if successful. On success, logs emitted during contract initialization are propagated to the outer transaction context (log addresses reflect the actual emitting contract, ordering is preserved). Because the sandbox always starts from nonce 0, the merged nonce is always 1 — it never accumulates across calls.
8. **Return result** — On success: return `gasUsed` and `deployedAddress`. On execution failure (init code revert, out of gas, or empty bytecode): return the error in `errorData` with `deployedAddress = 0`. Execution failures return success at the EVM level (not revert) so that state changes persist and the signer is always charged.

{% hint style="info" %}
**Why empty bytecode is treated as failure**: If init code returns empty bytecode, the result is `EmptyCodeDeployed` (an execution error, not success).
This prevents infinite replay: `ContractAlreadyExists` only checks for non-empty code, so without this rule an attacker could repeatedly call `keylessDeploy` for a broken deployment that produces empty code — draining the signer's balance indefinitely.
{% endhint %}

## Quick Start

{% hint style="danger" %}
MegaETH's [storage gas](../glossary.md#storage-gas) — especially the [code deposit cost](../evm/dual-gas-model.md#storage-gas-costs) of 10,000 gas per byte — can make keyless deployment transactions significantly more expensive than on Ethereum.
A contract with 24 KB of bytecode incurs ~240M storage gas for the code deposit alone.
If you need to deploy a **well-known** contract via Nick's Method, consider reaching out to the MegaETH team for assistance rather than funding the deployment yourself.
{% endhint %}

To deploy a keyless contract on MegaETH:

1. **Obtain the original signed transaction** — the raw RLP-encoded pre-EIP-155 transaction bytes (publicly available for well-known contracts)
2. **Recover the signer address** — `ecrecover` from the transaction signature
3. **Fund the signer** — transfer exactly `gasLimitOverride × gasPrice + value` to the signer address
4. **Call `keylessDeploy`** — pass the original transaction bytes and a sufficient `gasLimitOverride`

### Foundry Script Example

```solidity
IKeylessDeploy deployer = IKeylessDeploy(0x6342000000000000000000000000000000000003);

// Original CREATE2 factory transaction (do not modify!)
bytes memory originalTx = hex"f8a58085174876e800830186a08080b853604580600e...";

// Deploy with higher gas limit for MegaETH
(uint64 gasUsed, address deployed,) = deployer.keylessDeploy(originalTx, 500_000);
assert(deployed == 0x4e59b44847b379578588920ca78fbf26c0b4956c);
```

## Deployment Checklist

1. **Check preconditions** — Signed transaction nonce must be exactly 0, signer on-chain nonce must be <= 1, and deployment address must have no code
2. **Fund minimally** — Transfer exactly `gasLimitOverride × gasPrice + value` to the signer
3. **Deploy immediately** — Call `keylessDeploy` as soon as possible after funding
4. **Verify** — Confirm contract exists at expected address

## Security Considerations

### The call is permissionless

Anyone can call `keylessDeploy` with a publicly known keyless transaction.
This is safe — the deployment result is deterministic regardless of who triggers it.
The same contract will be deployed to the same address no matter which caller invokes `keylessDeploy`.

### Excess funds are irrecoverable

No one holds the signer's private key.
Any balance remaining in the signer address after deployment is stuck forever.
Always fund with exactly `gasLimitOverride × gasPrice + value` — no more.

### Execution failures charge the signer

If sandbox execution fails (init code reverts or runs out of gas), the signer is still charged `gasUsed × gasPrice`.
Because the sandbox always overrides the signer's nonce to 0, the nonce in the parent state resets to 1 after every call (regardless of how many calls have been made).
This means `keylessDeploy` can be called repeatedly as long as the signer has balance and the deploy address has no code.

{% hint style="danger" %}
**Fund minimally.**
No one holds the signer's private key, so any excess balance is stuck forever.
If the signer has more balance than needed for a single deployment attempt, repeated failed calls can drain it entirely.
{% endhint %}

## Already Deployed

| Contract               | Signer                                       | Deployed Address                             | Reference |
| ---------------------- | -------------------------------------------- | -------------------------------------------- | --------- |
| CREATE2 Factory        | `0x3fab184622dc19b6109349b94811493bf2a45362` | `0x4e59b44847b379578588920ca78fbf26c0b4956c` | [Deterministic Deployment Proxy](https://github.com/Arachnid/deterministic-deployment-proxy) |
| EIP-1820 Registry      | `0xa990077c3205cbDf861e17Fa532eeB069cE9fF96` | `0x1820a4B7618BdE71Dce8cdc73aAB6C95905faD24` | [EIP-1820](https://eips.ethereum.org/EIPS/eip-1820) |


## Error Handling

**Validation errors** — The call reverts.
No state changes are applied from the keyless deploy sandbox.
The signer is not charged.

| Error                    | Cause                            |
| ------------------------ | -------------------------------- |
| `MalformedEncoding()`    | Invalid RLP encoding             |
| `NotContractCreation()`  | Transaction has a `to` address   |
| `NotPreEIP155()`         | v is not 27 or 28               |
| `NoEtherTransfer()`      | `keylessDeploy` call includes non-zero value |
| `InvalidSignature()`     | Cannot recover signer            |
| `NonZeroTxNonce(uint64 txNonce)` | Signed transaction nonce is not 0 |
| `SignerNonceTooHigh(uint64 signerNonce)` | Signer nonce > 1          |
| `InsufficientBalance()`  | Signer lacks funds               |
| `ContractAlreadyExists()`| Address already has code         |
| `GasLimitTooLow(uint64 txGasLimit, uint64 providedGasLimit)` | Override < transaction's limit |
| `InsufficientComputeGas(uint64 limit, uint64 used)` | [Call-frame](../glossary.md#call-frame)-local [compute gas](../glossary.md#compute-gas) budget is below keyless deploy overhead |

**Execution errors** — The call returns normally with error in `errorData`.
The signer is charged for gas.

| Error                    | Cause                            |
| ------------------------ | -------------------------------- |
| `ExecutionReverted(uint64 gasUsed, bytes output)` | Init code reverted |
| `ExecutionHalted(uint64 gasUsed)` | Out of gas, stack overflow, etc. |
| `EmptyCodeDeployed(uint64 gasUsed)` | Init code returned empty bytecode |

**Internal or fallback errors** — The call reverts.
These indicate defensive checks or fallback-to-bytecode behavior.

| Error                    | Cause                            |
| ------------------------ | -------------------------------- |
| `NoContractCreated()`    | CREATE execution succeeded but did not return a created address |
| `AddressMismatch()`      | Actual deployed address differs from expected Nick's Method address |
| `InternalError(string message)` | Unexpected internal/database failure during sandbox processing |
| `NotIntercepted()`       | Call was not intercepted (for example unknown selector or non-top-level call) |

## Gas Accounting

| Context            | Gas Limit           | Paid By                    |
| ------------------ | ------------------- | -------------------------- |
| Outer transaction  | Caller's tx gas     | Caller (transaction sender)|
| Sandbox execution  | `gasLimitOverride`  | Signer (balance deducted)  |

The signer is charged `gasUsed × legacyTx.gasPrice` after execution, even on failure.
