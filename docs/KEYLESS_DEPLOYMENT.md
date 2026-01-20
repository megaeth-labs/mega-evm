# Keyless Deployment

## Overview

**Keyless Deployment** (also known as "[Nick's Method](https://weka.medium.com/how-to-send-ether-to-11-440-people-187e332566b7)") allows deploying contracts to the same address on every EVM chain—without anyone needing to hold the deployer's private key.

### How Nick's Method Works

In a normal deployment, the contract address depends on the deployer's address and nonce:

```
contract_address = keccak256(rlp([deployer, nonce]))[12:]
```

Nick's Method uses a clever trick: create a pre-signed transaction where the "deployer" address is derived from the signature itself. Since the signature is fixed, the deployer address is deterministic, and so is the contract address.

```
signature + tx_content  →  deployer_address  →  contract_address
     (fixed)                  (derived)           (deterministic)
```

This allows anyone to broadcast the same pre-signed transaction on any chain to deploy the contract to the same address.

### The Problem on MegaETH

MegaETH's gas model prices many operations differently than Ethereum. Contracts that deploy successfully on Ethereum may run out of gas on MegaETH.

Normally, you'd just increase the gas limit. But with Nick's Method, **any change to the transaction changes the derived deployer address**, which changes the contract address:

```
modified_tx  →  different_deployer  →  different_contract_address  ✗
```

This breaks cross-chain address consistency—the whole point of Nick's Method.

### The Solution

MegaETH provides a **KeylessDeploy system contract** that:

1. Takes the original, unmodified keyless transaction
2. Accepts a separate `gasLimitOverride` parameter
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
- `keylessDeploymentTransaction`: The original RLP-encoded pre-EIP-155 signed transaction (unmodified)
- `gasLimitOverride`: Gas limit to use for execution (must be ≥ the transaction's original gas limit)

**Returns**:
- `gasUsed`: Gas consumed by the deployment
- `deployedAddress`: Address where the contract was deployed

### Restrictions

- **Top-level calls only**: Must be called directly by a transaction (with `to` = system contract address). Calls from other contracts or EIP-7702 delegated EOAs will revert with `NotIntercepted()`.
- **No ether transfer**: The call must not send any ether value to the system contract.

## How It Works

The system contract acts as a **transaction execution sandbox**. It decodes `keylessDeploymentTransaction` and executes it as if it were a standalone transaction, but with these modifications:

- The signer's nonce is forced to `0` (required for deterministic address)
- The gas limit is replaced with `gasLimitOverride`
- All other transaction parameters remain unchanged

### Gas Costs

| Cost | Paid By | Description |
|------|---------|-------------|
| Overhead (100,000 gas) | Caller | Fixed cost for system contract processing |
| Deployment execution | Signer | Gas for running the init code, same as normal transactions |

The signer (derived from `keylessDeploymentTransaction`) must be pre-funded with enough ETH to cover:
```
gasLimitOverride × gasPrice + value
```

If you use a higher `gasLimitOverride`, ensure the signer has proportionally more ETH.

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
├─────────────────────────────────────────────────────────────────┤
│ 5. EXECUTE in sandbox                                           │
│    • msg.sender = recovered signer                              │
│    • nonce = 0 (forced)                                         │
│    • gas limit = gasLimitOverride                               │
├─────────────────────────────────────────────────────────────────┤
│ 6. APPLY state changes and logs on success                      │
└─────────────────────────────────────────────────────────────────┘
```

### Sandbox Guarantees

- **Atomic**: State changes only apply if deployment succeeds completely
- **Isolated**: Failures don't affect the outer transaction state
- **Transparent**: Logs from deployment are propagated to the outer context

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
