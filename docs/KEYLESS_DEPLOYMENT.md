# Keyless Deployment

## Overview

**Keyless Deployment** (also known as "[Nick's Method](https://weka.medium.com/how-to-send-ether-to-11-440-people-187e332566b7)") allows deploying contracts to the same address on every EVM chain—without anyone needing to hold the deployer's private key.

### How Nick's Method Works

In a normal deployment, the contract address depends on the deployer's address and nonce:

```
contract_address = keccak256(rlp([deployer, nonce]))[12:]
```

Nick's Method uses a clever trick: create a pre-signed transaction where the "deployer" address is recovered from the signature over the transaction hash (`ecrecover(keccak256(rlp(tx_fields)), v, r, s)`). Because the signature is bound to all signed fields, the recovered deployer address is deterministic for that exact payload, and so is the contract address.

```
signature + tx_content  →  deployer_address  →  contract_address
     (fixed)                  (derived)           (deterministic)
```

This allows anyone to broadcast the same pre-signed transaction on any chain to deploy the contract to the same address.

### The Problem on MegaETH

MegaETH's gas model prices many operations differently than Ethereum. Contracts that deploy successfully on Ethereum may run out of gas on MegaETH.

Normally, you'd just increase the gas limit. But with Nick's Method, **any change to the signed transaction fields invalidates the signature**, so the signer can no longer be recovered for that payload. Even if you re-sign, the recovered signer (and therefore contract address) would change:

```
modified_tx  →  different_deployer  →  different_contract_address  ✗
```

This breaks cross-chain address consistency—the whole point of Nick's Method.

### The Solution

MegaETH provides a **KeylessDeploy system contract** that:

1. Takes the original, unmodified keyless transaction (used for signature recovery)
2. Accepts a separate `gasLimitOverride` parameter (used only at execution time)
3. Executes the deployment using the override gas limit
4. Deploys to the **same address** as other chains

## System Contract

**Address**: `0x6342000000000000000000000000000000000003`

**Available**: REX2 hardfork onwards

### [Interface](../crates/system-contracts/contracts/interfaces/IKeylessDeploy.sol)

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress);
}
```

**Parameters**:
- `keylessDeploymentTransaction`: The original RLP-encoded pre-EIP-155 signed transaction (unmodified). The signature is verified against these exact fields.
- `gasLimitOverride`: Gas limit to use for execution (must be ≥ the transaction's original gas limit)

**Returns**:
- `gasUsed`: Gas consumed by the deployment
- `deployedAddress`: Address where the contract was deployed

### Restrictions

- **Top-level calls only**: Must be called directly by a transaction (with `to` = system contract address). Calls from other contracts or EIP-7702 delegated EOAs will revert with `NotIntercepted()`.
- **No ether transfer**: The call must not send any ether value to the system contract.

## How It Works

The system contract acts as a **transaction execution sandbox**. It verifies the signature against the original signed fields from `keylessDeploymentTransaction`, then executes as if it were a standalone transaction, but with these modifications:

- The signer's nonce is forced to `0` (required for deterministic address), even if the signer's nonce is already non-zero in the state.
- The gas limit is replaced with `gasLimitOverride` (an execution-time parameter, not part of the signed payload).
- All other transaction parameters remain unchanged.

Nonce semantics are intentionally custom: the sandbox always uses nonce `0` for the signer for CREATE address derivation and execution, regardless of the signer's current state nonce.
The parent-state's signer nonce is preserved (not incremented) on both success and failure; only the sandbox state uses the overridden nonce.
Nonce changes for other accounts produced during sandbox execution are merged back into the parent context on success.

### Gas Costs

| Cost | Paid By | Description |
|------|---------|-------------|
| Overhead (100,000 gas) | Caller | Fixed cost for system contract processing |
| Deployment execution | Signer | Sandbox execution gas, charged per the rules below |

The signer (derived from `keylessDeploymentTransaction`) must be pre-funded with enough ETH to cover:
```
gasLimitOverride × gasPrice + value
```

If you use a higher `gasLimitOverride`, ensure the signer has proportionally more ETH.

### Charging Rules

The caller pays the fixed overhead gas as part of the outer transaction's normal gas accounting.
The outer caller's gas price is the outer transaction's gas price, and it is unrelated to the signed legacy transaction's `gasPrice`.
The signer must have balance ≥ `gasLimitOverride × gasPrice + value` before sandbox execution, or the call fails with `InsufficientBalance()`.
The sandbox uses `gasLimitOverride` and the legacy transaction's `gasPrice` for execution.
On success, the sandbox state is merged, so the signer's balance is debited for actual gas used (including standard EVM refunds) and any value transfer.
On revert or halt, the sandbox state is still merged to apply gas charges, but all execution side effects remain reverted and no value is transferred.
Any refunds from sandbox execution accrue to the signer, not the outer caller.

### Security Considerations

The keyless deploy call is permissionless, so anyone can submit `keylessDeploy(originalTx, gasLimitOverride)`.
Because the signer is charged for gas used even on inner failure, a malicious caller can repeatedly submit failing calls to burn the signer's funds before the first successful deployment.
Operational guidance: keep the signer balance minimal until you are ready to deploy, fund and deploy promptly (ideally in the same block), and monitor for repeated failing keyless deploy attempts against the same deployment address.

### Transaction Format

The `keylessDeploymentTransaction` must be a pre-EIP-155 legacy transaction:

| Field | Requirement |
|-------|-------------|
| nonce | 0 (required for deterministic address) |
| gasPrice | Any value (commonly 100 gwei) |
| gasLimit | Any value (must be ≤ `gasLimitOverride`) |
| to | Empty (must be contract creation) |
| value | Any value (typically 0) |
| data | Contract initialization bytecode |
| v | 27 or 28 (pre-EIP-155, no chain ID) |
| r, s | Signature components |

The signer's on-chain nonce is **not** required to be `0` for keyless deploy. 
It is ignored in the sandbox and does not affect the deployment address or execution.

### Execution Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. VALIDATE                                                     │
│    • No ether sent to system contract                           │
│    • Valid RLP encoding                                         │
│    • Contract creation (empty `to`)                             │
│    • Pre-EIP-155 (v = 27 or 28)                                 │
│    • gasLimitOverride ≥ tx gas limit                            │
├─────────────────────────────────────────────────────────────────┤
│ 2. RECOVER signer address from signature                        │
├─────────────────────────────────────────────────────────────────┤
│ 3. CALCULATE deployment address = keccak256(rlp([signer, 0]))   │
├─────────────────────────────────────────────────────────────────┤
│ 4. CHECK preconditions                                          │
│    • Signer has sufficient balance                              │
│    • Deployment address has no existing code                    │
│    • Signer state nonce is not checked (sandbox uses nonce=0)    │
├─────────────────────────────────────────────────────────────────┤
│ 5. EXECUTE in sandbox                                           │
│    • msg.sender = recovered signer                              │
│    • nonce = 0 (forced)                                         │
│    • gas limit = gasLimitOverride                               │
├─────────────────────────────────────────────────────────────────┤
│ 6. APPLY sandbox state                                          │
│    • On success: deploy effects + logs                          │
│    • On failure: only gas charges (side effects reverted)       │
│    • Signer nonce is preserved (no increment)                   │
└─────────────────────────────────────────────────────────────────┘
```

### Sandbox Guarantees

- **Atomic**: Deployment side effects apply only on success
- **Isolated**: Failures do not apply deployment side effects, but gas charges still apply
- **Transparent**: Logs from deployment are propagated to the outer context on success

## Error Reference

| Error | Cause |
|-------|-------|
| `MalformedEncoding()` | Invalid RLP encoding |
| `NotContractCreation()` | Transaction has a `to` address |
| `NotPreEIP155()` | v is not 27 or 28 (has chain ID) |
| `NoEtherTransfer()` | Ether was sent to system contract |
| `InvalidSignature()` | Cannot recover signer |
| `InsufficientBalance()` | Signer lacks funds |
| `ContractAlreadyExists()` | Address already has code |
| `GasLimitTooLow(txGasLimit, providedGasLimit)` | Override < transaction's limit |
| `ExecutionReverted(gasUsed, output)` | Init code reverted |
| `ExecutionHalted(gasUsed)` | Out of gas, stack overflow, etc. |
| `NotIntercepted()` | Called from contract (not top-level) |
| `NoContractCreated()` | Internal error: no address returned |
| `AddressMismatch()` | Internal error: address mismatch |
| `InternalError(message)` | Unexpected error |

## Examples (Foundry Scripts)

### Deploying the CREATE2 Factory

The [deterministic deployment proxy](https://github.com/Arachnid/deterministic-deployment-proxy) (CREATE2 factory) uses Nick's Method:

| | Address |
|-|---------|
| Signer | `0x3fab184622dc19b6109349b94811493bf2a45362` |
| Contract | `0x4e59b44847b379578588920ca78fbf26c0b4956c` |

```solidity
IKeylessDeploy deployer = IKeylessDeploy(0x6342000000000000000000000000000000000003);

// Original CREATE2 factory deployment transaction (do not modify!)
bytes memory originalTx = hex"f8a58085174876e800830186a08080b853604580600e...";

// Deploy with higher gas limit for MegaETH
(uint64 gasUsed, address deployed) = deployer.keylessDeploy(originalTx, 500_000);

assert(deployed == 0x4e59b44847b379578588920ca78fbf26c0b4956c);
```

### Deploying EIP-1820 Registry

The [EIP-1820](https://eips.ethereum.org/EIPS/eip-1820) universal registry also uses Nick's Method:

| | Address |
|-|---------|
| Signer | `0xa990077c3205cbDf861e17Fa532eeB069cE9fF96` |
| Contract | `0x1820a4B7618BdE71Dce8cdc73aAB6C95905faD24` |

```solidity
IKeylessDeploy deployer = IKeylessDeploy(0x6342000000000000000000000000000000000003);

// Original EIP-1820 deployment transaction (do not modify!)
bytes memory originalTx = hex"f90a388085174876e800830c35008080b909e5608060405234801561001057...";

// Deploy with sufficient gas for MegaETH
(uint64 gasUsed, address deployed) = deployer.keylessDeploy(originalTx, 2_000_000);

assert(deployed == 0x1820a4B7618BdE71Dce8cdc73aAB6C95905faD24);
```
