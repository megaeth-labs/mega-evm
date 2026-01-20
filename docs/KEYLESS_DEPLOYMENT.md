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

## Quick Start

**System Contract Address**: `0x6342000000000000000000000000000000000003`

**Available**: REX2 hardfork onwards

### Basic Usage (Foundry Script)

```solidity
import {IKeylessDeploy} from "mega-evm/interfaces/IKeylessDeploy.sol";

IKeylessDeploy deployer = IKeylessDeploy(0x6342000000000000000000000000000000000003);

// Original keyless deployment transaction (do not modify!)
bytes memory originalTx = hex"f8a58085174876e800830186a08080b853...";

// Deploy with higher gas limit for MegaETH
(uint64 gasUsed, address deployed) = deployer.keylessDeploy(originalTx, 500_000);
```

### Key Points

1. **Fund the signer first** — The signer (recovered from the transaction signature) must have sufficient balance for `gasLimitOverride × gasPrice + value`
2. **Use the original transaction bytes** — Any modification invalidates the signature
3. **Set gasLimitOverride ≥ original gas limit** — The override must be at least as large as the transaction's gas limit

See [Usage Guide](#usage-guide) for detailed deployment procedures and examples.

## Interface Reference

### [Interface](../crates/system-contracts/contracts/interfaces/IKeylessDeploy.sol)

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress, bytes memory errorData);
}
```

**Parameters**:
- `keylessDeploymentTransaction`: The original RLP-encoded pre-EIP-155 signed transaction (unmodified). The signature is verified against these exact fields.
- `gasLimitOverride`: Gas limit to use for execution (must be ≥ the transaction's original gas limit)

**Returns**:
- `gasUsed`: Gas consumed by the deployment
- `deployedAddress`: Address where the contract was deployed (zero if execution failed)
- `errorData`: ABI-encoded error if execution failed, empty bytes on success

### Error Handling

Errors are handled in two different ways:

**Validation errors** — The system contract call **reverts**. No state changes occur, and the signer is not charged.

**Execution errors** — The call **returns normally** with the error ABI-encoded in `errorData`. The signer is charged for gas consumed, and `deployedAddress` is zero.

This distinction ensures the signer can be charged for gas even when deployment fails.

### Error Reference

**Validation errors (call reverts):**

| Error | Cause |
|-------|-------|
| `MalformedEncoding()` | Invalid RLP encoding |
| `NotContractCreation()` | Transaction has a `to` address |
| `NotPreEIP155()` | v is not 27 or 28 (has chain ID) |
| `NoEtherTransfer()` | Ether was sent to system contract |
| `InvalidSignature()` | Cannot recover signer |
| `SignerNonceTooHigh(signerNonce)` | Signer nonce > 1 |
| `InsufficientBalance()` | Signer lacks funds |
| `ContractAlreadyExists()` | Address already has code |
| `GasLimitTooLow(txGasLimit, providedGasLimit)` | Override < transaction's limit |
| `NotIntercepted()` | Called from contract (not top-level) |
| `NoContractCreated()` | Defensive: EVM bug (see [Implementation Notes](#implementation-notes)) |
| `AddressMismatch()` | Defensive: EVM bug (see [Implementation Notes](#implementation-notes)) |
| `InternalError(message)` | Defensive: database/storage failure |

**Execution errors (returned in `errorData`):**

| Error | Cause |
|-------|-------|
| `ExecutionReverted(gasUsed, output)` | Init code reverted |
| `ExecutionHalted(gasUsed)` | Out of gas, stack overflow, etc. |
| `EmptyCodeDeployed(gasUsed)` | Init code returned empty bytecode |

### Transaction Format

The `keylessDeploymentTransaction` must be a pre-EIP-155 legacy transaction:

| Field | Requirement |
|-------|-------------|
| nonce | 0 (required in signed payload) |
| gasPrice | Any value (commonly 100 gwei) |
| gasLimit | Any value (must be ≤ `gasLimitOverride`) |
| to | Empty (must be contract creation) |
| value | ETH to transfer to created contract (see [Value Transfer](#value-transfer)) |
| data | Contract initialization bytecode |
| v | 27 or 28 (pre-EIP-155, no chain ID) |
| r, s | Signature components |

## Usage Guide

This section provides practical guidance for deploying canonical contracts using keyless deployment.

### Obtaining and Verifying Transaction Data

Before calling `keylessDeploy`, you need the original pre-signed transaction bytes.

#### Where to Find Canonical Keyless Transactions

Common sources for well-known keyless deployment transactions:

| Contract | Source |
|----------|--------|
| CREATE2 Factory | [Arachnid/deterministic-deployment-proxy](https://github.com/Arachnid/deterministic-deployment-proxy) |
| EIP-1820 Registry | [EIP-1820 specification](https://eips.ethereum.org/EIPS/eip-1820) |
| Safe Singleton Factory | [safe-global/safe-singleton-factory](https://github.com/safe-global/safe-singleton-factory) |
| Multicall3 | [mds1/multicall](https://github.com/mds1/multicall) |

You can also retrieve deployed transactions from Etherscan:
1. Find the contract's creation transaction
2. Click "More Details" → "View Input As" → "Original"
3. Copy the full transaction input (this is the RLP-encoded signed transaction)

#### Verifying Transaction Data with `cast`

**Decode the transaction and recover the signer:**

```bash
# Decode the RLP-encoded transaction (shows all fields + recovered signer)
cast decode-tx 0xf8a58085174876e800830186a08080b853...

# Output includes:
#   "signer": "0x3fab184622dc19b6109349b94811493bf2a45362",
#   "nonce": "0x0",
#   "gasPrice": "0x174876e800",  (100 gwei)
#   "gas": "0x186a0",            (100000)
#   "to": null,                  (contract creation)
#   "value": "0x0",
#   "v": 27,
#   ...
```

**Verify the transaction is valid for keyless deploy:**

| Field | Required | Check |
|-------|----------|-------|
| `to` | null/empty | Must be contract creation |
| `v` | 27 or 28 | Must be pre-EIP-155 (no chain ID) |
| `nonce` | 0 | Required for deterministic address |
| `signer` | recovered | Confirms valid signature |
| signer's on-chain nonce | ≤ 1 | Check with `cast nonce <signer>` |

**Compute the expected deployment address:**

The deployment address is derived from the signer and nonce 0:

```bash
# Using cast to compute CREATE address
cast compute-address <signer_address> --nonce 0
```

Or manually: `keccak256(rlp([signer, 0]))[12:]`

**Calculate the required prefund amount:**

```bash
# Extract gasPrice and gasLimit from decoded transaction, then:
# required = gasLimitOverride × gasPrice + value

# Example for CREATE2 factory (gasPrice=100gwei, value=0):
cast to-wei 100 gwei  # gasPrice in wei
# 100000000000

# If using gasLimitOverride=500000:
echo "scale=18; 500000 * 100000000000 / 10^18" | bc
# 0.05 ETH required (plus any value field)
```

#### Example: Verifying CREATE2 Factory Transaction

```bash
# The canonical CREATE2 factory transaction
TX=0xf8a58085174876e800830186a08080b853604580600e600039806000f350fe7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf31ba02222222222222222222222222222222222222222222222222222222222222222a02222222222222222222222222222222222222222222222222222222222222222

# Decode to see fields
cast decode-tx $TX
# nonce: 0
# gasPrice: 100000000000 (100 gwei)
# gasLimit: 100000
# to: (empty - contract creation)
# value: 0
# v: 27, r: 0x2222..., s: 0x2222...

# The signer recovered from this signature is:
# 0x3fab184622dc19b6109349b94811493bf2a45362

# Verify deployment address
cast compute-address 0x3fab184622dc19b6109349b94811493bf2a45362 --nonce 0
# 0x4e59b44847b379578588920ca78fbf26c0b4956c ✓
```

### Safe Deployment Checklist

Follow this procedure to minimize exposure to [griefing attacks](#griefing-attack-pre-deployment-fund-burning):

#### Pre-Deployment Checks

```bash
# Set your variables
SIGNER=0x3fab184622dc19b6109349b94811493bf2a45362
EXPECTED_ADDR=0x4e59b44847b379578588920ca78fbf26c0b4956c
RPC=https://your-megaeth-rpc.com

# 1. Check signer nonce (must be 0 or 1)
cast nonce $SIGNER --rpc-url $RPC
# If > 1: keyless deploy is permanently disabled for this signer

# 2. Check deployment address has no code
cast code $EXPECTED_ADDR --rpc-url $RPC
# If not "0x": contract already exists, nothing to do

# 3. Calculate exact prefund amount
# prefund = gasLimitOverride × gasPrice + value
# Example: 500000 gas × 100 gwei + 0 value = 0.05 ETH
```

#### Deployment Procedure

1. **Fund minimally**: Transfer exactly the calculated amount to the signer
   ```bash
   cast send $SIGNER --value 0.05ether --rpc-url $RPC --private-key $YOUR_KEY
   ```

2. **Deploy immediately**: Call `keylessDeploy` as soon as possible (ideally same block)
   ```bash
   # Via Foundry script or direct call
   cast send 0x6342000000000000000000000000000000000003 \
     "keylessDeploy(bytes,uint256)" \
     0xf8a58085174876e800830186a08080b853... \
     500000 \
     --rpc-url $RPC --private-key $YOUR_KEY
   ```

3. **Verify deployment**: Confirm contract exists at expected address
   ```bash
   cast code $EXPECTED_ADDR --rpc-url $RPC
   # Should return the deployed bytecode
   ```

#### Why Minimal Funding Matters

> **Warning**: Do not leave excess funds in the signer address.

Since `keylessDeploy` is permissionless, anyone can call it with your signer's transaction. If the signer has excess balance, an attacker can:
1. Call `keylessDeploy` with intentionally low `gasLimitOverride`
2. Deployment fails, but signer is still charged for gas consumed
3. Repeat until signer balance is drained

By funding only the exact amount needed, you ensure:
- Successful deployment uses all funds productively
- An attacker draining minimal funds wastes their own gas on the outer transaction
- Even if griefed, the loss is bounded to your intended spend

See [Griefing Attack: Pre-Deployment Fund Burning](#griefing-attack-pre-deployment-fund-burning) for details.

### Examples (Foundry Scripts)

#### Deploying the CREATE2 Factory

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

#### Deploying EIP-1820 Registry

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

## How It Works

The system contract acts as a **transaction execution sandbox**. It verifies the signature against the original signed fields from `keylessDeploymentTransaction`, then executes as if it were a standalone transaction, but with these modifications:

- The gas limit is replaced with `gasLimitOverride` (an execution-time parameter, not part of the signed payload)
- Nonce handling follows custom semantics (see [Nonce Semantics](#nonce-semantics-non-standard))
- All other transaction parameters remain unchanged

Nonce changes for other accounts produced during sandbox execution are merged back into the parent context on success.

### Execution Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. VALIDATE transaction format                                  │
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
│ 4. CHECK on-chain preconditions                                 │
│    • Signer nonce ≤ 1                                           │
│    • Signer has sufficient balance                              │
│    • Deployment address has no existing code                    │
├─────────────────────────────────────────────────────────────────┤
│ 5. EXECUTE in sandbox                                           │
│    • msg.sender = recovered signer                              │
│    • nonce = 0 (forced)                                         │
│    • gas limit = gasLimitOverride                               │
├─────────────────────────────────────────────────────────────────┤
│ 6. APPLY sandbox state                                          │
│    • On success: deploy effects + logs                          │
│    • On failure: only gas charges (side effects reverted)       │
│    • Signer nonce becomes 1                                     │
└─────────────────────────────────────────────────────────────────┘
```

## Technical Deep Dives

> **Note**: This section contains detailed implementation information primarily relevant to those building tooling, debugging issues, or understanding the system at a deeper level.

### Nonce Semantics (Non-Standard)

> **Warning**: This is intentionally non-standard EVM behavior. The sandbox uses custom nonce semantics to guarantee deterministic deployment addresses. Tooling that assumes standard EVM nonce rules may need adjustment.

**Precondition**: The signer's on-chain nonce must be ≤ 1, enforced before execution. If nonce > 1, the call reverts with `SignerNonceTooHigh(signerNonce)`.

**Why ≤ 1 (not just 0)?** This accommodates a common scenario: someone attempts to broadcast the original keyless transaction directly on MegaETH before using this system contract. That transaction fails (due to gas differences), but the attempt still increments the signer's nonce from 0 to 1. Allowing nonce ≤ 1 means keyless deploy still works after such a failed direct broadcast.

**During execution**: The sandbox forces nonce = 0 for CREATE address derivation, regardless of the signer's actual on-chain nonce. This guarantees `deployed_address = keccak256(rlp([signer, 0]))[12:]` matches other chains.

**After execution**: The signer's nonce becomes 1 in the parent state (on both success and failure). This marks the signer as "used" and consumes the keyless deploy opportunity.

**Replay prevention**: A second keyless deploy attempt for the same signer would either:
- Find existing code at the deployment address → `ContractAlreadyExists` error
- Find the signer nonce is now 1, but since ≤ 1 is allowed, it proceeds to the code check above
- If the first deploy returned empty code (failure), the signer nonce is still 1, but no code exists. This is why empty code is treated as failure (`EmptyCodeDeployed`) rather than success—it prevents infinite replay of broken deployments.

### Gas Accounting

The keyless deploy system uses **two separate gas contexts** with different payers:

| Context | Gas Limit | Gas Price | Paid By |
|---------|-----------|-----------|---------|
| Outer transaction | Caller's tx gas limit | Caller's tx gas price | Caller (transaction sender) |
| Sandbox execution | `gasLimitOverride` | Legacy tx's `gasPrice` | Signer (balance deducted directly) |

#### Outer Transaction (Caller Pays)

The caller submits a normal transaction calling `keylessDeploy(...)`. This transaction pays for:
- Standard transaction intrinsic costs
- System contract overhead (~100,000 gas fixed cost)
- Any calldata costs

The outer gas price is whatever the caller's transaction specifies—completely independent of the legacy transaction's `gasPrice` field.

#### Sandbox Execution (Signer Pays)

The signer is not actually sending a transaction. Instead, the system contract **explicitly debits the signer's ETH balance** to simulate gas payment:

**Before execution (balance check)**:
```
require(signer.balance >= gasLimitOverride × legacyTx.gasPrice + legacyTx.value)
```
If insufficient, reverts with `InsufficientBalance()`.

**After execution (actual charge)**:
```
signer.balance -= gasUsed × legacyTx.gasPrice + valueTransferred
```
Where `gasUsed` is the actual gas consumed (not the full `gasLimitOverride`), after applying standard EVM gas refunds.

**Refund handling**: Standard EVM refund mechanics apply within the sandbox. Refunds reduce `gasUsed` and thus reduce the amount debited from the signer. Refunds accrue to the signer, not the outer caller.

**On sandbox failure** (revert/halt): The signer is still charged for gas consumed up to the point of failure. Execution side effects are reverted, but the gas charge is applied. This prevents free reverts.

#### Gas Price Semantics

The `gasPrice` field in the signed legacy transaction is interpreted as a simple multiplier for gas costs within the sandbox. It is:
- **Not** validated against any base fee or minimum
- **Not** subject to EIP-1559 mechanics
- **Not** split between L1/L2 fees

This is intentional: the signed transaction was created for other chains and cannot be modified. The gas price is honored as-is to maintain determinism. MegaETH's different gas model is accommodated via `gasLimitOverride`, not by reinterpreting gas price.

### Value Transfer

The keyless deployment system handles ETH value in two distinct contexts:

| Context | Requirement | Purpose |
|---------|-------------|---------|
| Outer call to `keylessDeploy()` | Must be `msg.value == 0` | Prevents accidental ETH loss |
| Inner keyless transaction `value` | Any amount (deducted from signer) | ETH sent to created contract |

**How it works:**

1. **Balance check**: Before execution, the signer must have sufficient balance:
   ```
   signer.balance >= (gasLimitOverride × gasPrice) + innerTx.value
   ```

2. **On successful deployment**: The `value` is transferred from the signer to the newly created contract address. The contract's constructor can use this ETH.

3. **On execution failure** (revert/halt): The value transfer is automatically undone by the EVM (standard behavior). The signer is still charged for gas consumed.

**Example**: If the inner transaction has `value = 1 ETH` and deployment succeeds, the created contract starts with 1 ETH balance, deducted from the signer.

#### Validation Details

Validation occurs in four phases:

**Phase 1: Call Validation**
- `msg.value == 0` — no ether sent to system contract (`NoEtherTransfer`)
- `gasLimitOverride >= tx.gasLimit` — override must be sufficient (`GasLimitTooLow`)

**Phase 2: RLP Decoding (Structural Validation)**
- The input must be valid RLP encoding of a legacy transaction (`MalformedEncoding`)
- EIP-2718 typed envelopes (starting with 0x01, 0x02, etc.) are rejected as `MalformedEncoding`
- The `to` field must be empty (contract creation) (`NotContractCreation`)
- The `v` value must be 27 or 28 (pre-EIP-155, no chain ID) (`NotPreEIP155`)

**Phase 3: Signature Recovery (Cryptographic Validation)**
- Signature components (r, s) must be valid secp256k1 curve points
- Invalid signatures (corrupted r/s, zero values) fail with `InvalidSignature`
- Alloy's signature recovery is used, which handles standard ECDSA recovery

**Phase 4: On-Chain Preconditions**
- Signer nonce ≤ 1 (`SignerNonceTooHigh`)
- Signer balance ≥ `gasLimitOverride × gasPrice + value` (`InsufficientBalance`)
- No existing code at deployment address (`ContractAlreadyExists`)

**Note on Signature Malleability**: Modifying the signature (r or s values) changes the recovered signer address. This means:
- A different signature produces a different signer identity
- The new signer will likely have insufficient balance
- This is not a security concern—each signature uniquely identifies its signer

**Empty Initcode**: Transactions with empty `data` (initcode) are accepted for decoding but result in `EmptyCodeDeployed` during execution. This error returns success with error data (signer is charged), preventing replay attacks on broken deployments.

### Sandbox Behavior

The "sandbox" is an internal execution context that allows applying partial state changes (gas charges) even when the deployment itself fails. This is **not** standard EVM call semantics.

#### Error Handling: Revert vs. Return

The system contract has two categories of errors with different behaviors:

**Validation errors (revert)**: These revert the entire `keylessDeploy` call. The outer transaction fails (unless caught via try/catch). No state changes occur.
- `MalformedEncoding`, `NotContractCreation`, `NotPreEIP155`, `NoEtherTransfer`
- `InvalidSignature`, `SignerNonceTooHigh`, `InsufficientBalance`
- `ContractAlreadyExists`, `GasLimitTooLow`, `NotIntercepted`

**Execution errors (return with error data)**: These return normally (do not revert), encoding the error in the return data. State changes for gas charges are applied.
- `ExecutionReverted(gasUsed, output)` — init code called `revert`
- `ExecutionHalted(gasUsed)` — out of gas, stack overflow, etc.
- `EmptyCodeDeployed(gasUsed)` — init code returned empty bytecode

This distinction is why the signer can be charged for gas even on deployment failure: execution errors don't revert the outer call.

#### State Application

| Outcome | Contract deployed | Signer charged | Logs emitted | Signer nonce |
|---------|-------------------|----------------|--------------|--------------|
| Success | ✓ | ✓ (gasUsed × gasPrice) | ✓ | → 1 |
| Execution error | ✗ | ✓ (gasUsed × gasPrice) | ✗ | → 1 |
| Validation error | ✗ | ✗ (call reverts) | ✗ | unchanged |

#### Log Propagation

On successful deployment, logs emitted during contract initialization are propagated to the outer transaction context:
- Log addresses reflect the actual emitting contract (the newly deployed contract or contracts it called)
- Topics and data are unchanged
- Log ordering is preserved; logs appear after any logs emitted before the `keylessDeploy` call

#### Empty Code Protection

If init code returns empty bytecode, it is treated as `EmptyCodeDeployed` (execution error, not success). This prevents replay attacks: without this check, an attacker could repeatedly call `keylessDeploy` for a broken deployment (since `ContractAlreadyExists` only checks for non-empty code).

### Security Considerations

#### Spam Protection Guarantees

The keyless deploy system is designed to ensure that **any sandbox execution that starts and completes will always charge the signer for gas consumed**. This is critical for preventing spam attacks where an attacker triggers execution but avoids paying.

**Key protections:**

1. **Top-level calls only (`depth == 0`)**: The sandbox interception only occurs for direct transaction calls to the system contract. Calls from other contracts (depth > 0) are NOT intercepted—they execute the Solidity fallback which reverts with `NotIntercepted()`. This prevents a malicious contract from wrapping the call, observing success, and then reverting to avoid charges.

2. **Execution errors return success**: Execution failures (`ExecutionReverted`, `ExecutionHalted`, `EmptyCodeDeployed`) return normally with error data encoded in the return value, rather than reverting. This ensures state changes (including gas charges) are committed even when deployment fails.

3. **Atomic state application**: Once sandbox execution completes (produces a Success or Failure outcome), `apply_sandbox_state` is called to merge changes into the parent context. This operation always succeeds, so there is no path for the outer transaction to revert after the signer has been charged.

**Invariant**: If sandbox execution starts and produces an outcome, the signer WILL be charged for gas consumed. The only way for the outer transaction to fail after sandbox execution begins is an internal error (e.g., database failure), but in that case no sandbox state was produced—there's nothing to commit or roll back.

#### Griefing Attack: Pre-Deployment Fund Burning

**The attack**: The `keylessDeploy` call is permissionless—anyone can submit it. Since the signer is charged for gas even on failure, an attacker can grief by:

1. Monitoring for funded keyless signers (known addresses from public keyless transactions)
2. Calling `keylessDeploy(originalTx, gasLimitOverride)` with a `gasLimitOverride` that is:
   - High enough to pass the `≥ tx gas limit` check
   - Too low to complete deployment (causes `ExecutionHalted` / out-of-gas)
3. Repeating until the signer's balance is drained

**Why this matters**: This attack window exists *before* the first successful deployment—exactly when users are trying to bootstrap canonical contracts. After successful deployment, the `ContractAlreadyExists` check prevents further calls, but by then the attack window has closed naturally.

**Why we don't prevent this at the protocol level**: Any mitigation that restricts who can call `keylessDeploy` would break the permissionless nature that makes keyless deployment useful (anyone should be able to deploy these canonical contracts). The signer address is derived from a public transaction, so there's no secret to protect.

#### Recommended Mitigations

Operators deploying via keyless deploy should:

1. **Minimize exposure window**: Fund the signer and deploy in the same block if possible.

2. **Fund minimally**: Calculate the exact required balance (`gasLimitOverride × gasPrice + value`) and fund only that amount. An attacker draining a minimal balance wastes their own gas on the outer transaction.

3. **Use sufficient gas override**: Choose a `gasLimitOverride` high enough to guarantee success. A failed legitimate attempt still costs the signer and reopens the griefing window.

4. **Increase the signer's nonce**: If you've already deployed the contract via another method or don't need keyless deploy on MegaETH, send any transaction from the signer to increment its nonce above 1. This permanently disables `keylessDeploy` for that signer (`SignerNonceTooHigh`), preventing further griefing.

### Restrictions

- **Direct transaction only (`depth == 0`)**: The system contract intercepts calls only when `call_depth == 0`, meaning the transaction's `to` address must be the system contract itself. Internal calls from other contracts are NOT intercepted—they execute the Solidity fallback which reverts with `NotIntercepted()`.

  **Why this restriction?** It prevents wrap-and-revert attacks. If a contract could call `keylessDeploy`, observe the result, and then revert, the signer's gas charge would be rolled back—enabling free spam. By requiring `depth == 0`, the sandbox result IS the transaction result; there's no outer context that can revert.

  | Scenario | Intercepted? | Result |
  |----------|--------------|--------|
  | EOA sends tx to system contract | ✅ Yes | Sandbox executes |
  | Contract calls system contract | ❌ No | Reverts with `NotIntercepted()` |
  | EIP-7702 EOA sends direct tx | ✅ Yes | Sandbox executes (EIP-7702 delegation doesn't affect outgoing txs) |
  | EIP-7702 delegated code calls system contract | ❌ No | Reverts with `NotIntercepted()` |

- **No ether to system contract**: The outer call must have `msg.value == 0`. This is separate from the inner keyless transaction's `value` field—see [Value Transfer](#value-transfer).

## Implementation Notes

> **Note**: This section is for implementers and those debugging low-level issues. It is not relevant for normal usage.

### Implementation Invariants

The implementation includes defensive checks that should never trigger under normal operation. These exist to catch EVM bugs rather than user errors:

| Internal Error | Invariant Being Checked | If Triggered |
|----------------|------------------------|--------------|
| `NoContractCreated()` | CREATE returns an address on success | EVM returned success without an address—indicates EVM bug |
| `AddressMismatch()` | CREATE address = `keccak256(rlp([signer, 0]))[12:]` | EVM computed wrong address—indicates EVM bug |
| `InternalError(msg)` | Database operations succeed | Storage/database failure during execution |

**If you encounter any of these errors, please report to the MegaETH team**—they indicate a bug in the EVM implementation, not a problem with your transaction.

### Why uint64 for gasUsed?

The return type uses `uint64` for `gasUsed` because:
1. It matches the EVM's native gas accounting type
2. Maximum value (~18 exagas) far exceeds any realistic gas limit
3. More efficient ABI encoding than uint256

Callers needing uint256 can simply cast: `uint256(gasUsed)`.
