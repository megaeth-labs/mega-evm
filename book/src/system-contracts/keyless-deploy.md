# Keyless Deployment

## Overview

**Keyless Deployment** (Nick's Method) allows deploying contracts to the same address on every EVM chain — without anyone needing to hold the deployer's private key.

**Address**: `0x6342000000000000000000000000000000000003`

**Available**: Rex2 hardfork onwards

## The Problem

MegaETH's gas model prices operations differently than Ethereum.
Contracts deployed via Nick's Method on Ethereum may run out of gas on MegaETH.

With Nick's Method, **any change to signed transaction fields invalidates the signature**, so the gas limit cannot simply be increased.

## The Solution

The `KeylessDeploy` system contract:

1. Takes the original, unmodified keyless transaction
2. Accepts a separate `gasLimitOverride` parameter
3. Executes the deployment using the override gas limit
4. Deploys to the **same address** as other chains

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

## Quick Start

### CLI Script

```bash
PRIVATE_KEY=<funded-account-key> ./scripts/keyless_deploy.sh <keyless-tx-hex>

# Example: deploy the CREATE2 factory
PRIVATE_KEY=0x... ./scripts/keyless_deploy.sh 0xf8a58085174876e800830186a08080b853...
```

### Foundry Example

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

{% hint style="danger" %}
Do not leave excess funds in the signer address.
The `keylessDeploy` call is permissionless — anyone can call it and drain the signer's balance through repeated failed deployments.
{% endhint %}

## Well-Known Keyless Transactions

| Contract               | Signer                                       | Deployed Address                             |
| ---------------------- | -------------------------------------------- | -------------------------------------------- |
| CREATE2 Factory        | `0x3fab184622dc19b6109349b94811493bf2a45362` | `0x4e59b44847b379578588920ca78fbf26c0b4956c` |
| EIP-1820 Registry      | `0xa990077c3205cbDf861e17Fa532eeB069cE9fF96` | `0x1820a4B7618BdE71Dce8cdc73aAB6C95905faD24` |

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
| `NonZeroTxNonce()`       | Signed transaction nonce is not 0 |
| `SignerNonceTooHigh()`   | Signer nonce > 1                 |
| `InsufficientBalance()`  | Signer lacks funds               |
| `ContractAlreadyExists()`| Address already has code         |
| `GasLimitTooLow()`       | Override < transaction's limit   |
| `InsufficientComputeGas()` | Frame-local compute gas budget is below keyless deploy overhead |

**Execution errors** — The call returns normally with error in `errorData`.
The signer is charged for gas.

| Error                    | Cause                            |
| ------------------------ | -------------------------------- |
| `ExecutionReverted()`    | Init code reverted               |
| `ExecutionHalted()`      | Out of gas, stack overflow, etc. |
| `EmptyCodeDeployed()`    | Init code returned empty bytecode|

**Internal or fallback errors** — The call reverts.
These indicate defensive checks or fallback-to-bytecode behavior.

| Error                    | Cause                            |
| ------------------------ | -------------------------------- |
| `NoContractCreated()`    | CREATE execution succeeded but did not return a created address |
| `AddressMismatch()`      | Actual deployed address differs from expected Nick's Method address |
| `InternalError()`        | Unexpected internal/database failure during sandbox processing |
| `NotIntercepted()`       | Call was not intercepted (for example unknown selector or non-top-level call) |

## Gas Accounting

| Context            | Gas Limit           | Paid By                    |
| ------------------ | ------------------- | -------------------------- |
| Outer transaction  | Caller's tx gas     | Caller (transaction sender)|
| Sandbox execution  | `gasLimitOverride`  | Signer (balance deducted)  |

The signer is charged `gasUsed × legacyTx.gasPrice` after execution, even on failure.
