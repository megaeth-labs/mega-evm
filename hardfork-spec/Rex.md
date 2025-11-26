# Rex Hardfork Specification

## 1. Introduction

The **Rex** hardfork is the second major upgrade to the MegaETH EVM, building upon the foundation established by MiniRex. While MiniRex successfully addressed the fundamental challenges of operating an ultra-low-fee, high-throughput blockchain through its dual gas model and multi-dimensional resource limits, operational experience revealed opportunities for refinement and bug fixes.

Rex maintains MiniRex's core design principles while introducing three key improvements:

1. **Optimized Storage Gas Economics**: Refined storage gas formulas that scale more gradually with SALT bucket growth, reducing costs for operations in minimum-sized buckets while maintaining economic sustainability
2. **Transaction Intrinsic Storage Gas**: Introduction of a 39,000 storage gas for all transactions to ensure baseline cost recovery for transaction processing overhead
3. **Critical Bug Fixes**: Correction of DELEGATECALL and STATICCALL implementations to properly enforce the 98/100 gas forwarding rule and oracle access detection

These changes preserve MiniRex's security guarantees and economic model while improving cost efficiency and fixing critical vulnerabilities in rarely-used opcodes.

## 2. Comprehensive List of Changes

Rex inherits all MiniRex features and modifications (see [MiniRex.md](MiniRex.md)) with the following changes:

### 2.1 Transaction Intrinsic Storage Gas

**New Transaction Intrinsic Cost:**
All transactions pay an additional **39,000 gas** as intrinsic storage gas, charged on top of the base 21,000 intrinsic gas.

**Total Base Transaction Cost:**

- **Compute Gas**: 21,000 gas (standard EVM intrinsic gas)
- **Storage Gas**: 39,000 gas (Rex transaction floor)
- **Total**: 60,000 gas minimum per transaction

**Rationale:**

- Ensures baseline cost recovery for transaction processing, validation, and state propagation
- Prevents ultra-cheap spam transactions that could overwhelm the network

**Comparison with MiniRex:**

- **MiniRex**: No additional intrinsic storage gas, transactions pay only 21,000 base intrinsic gas
- **Rex**: All transactions pay 60,000 total base cost (21,000 compute + 39,000 storage)

### 2.2 Refined Storage Gas Economics

Rex introduces a new storage gas formula that scales more gradually with SALT bucket growth, reducing costs for fresh storage while maintaining economic pressure on heavily-used buckets.

#### 2.2.1 SSTORE Storage Gas

**Formula Change:**

| Spec        | Formula                     | Minimum Bucket (multiplier=1) | Double Bucket (multiplier=2) | 4× Bucket (multiplier=4) |
| ----------- | --------------------------- | ----------------------------- | ---------------------------- | ------------------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000 gas                 | 4,000,000 gas                | 8,000,000 gas            |
| **Rex**     | `20,000 × (multiplier - 1)` | **0 gas**                     | **20,000 gas**               | **60,000 gas**           |

**Key Differences:**

- **Base cost**: 20,000 gas (vs. 2M in MiniRex)
- **Formula**: Uses `(multiplier - 1)` instead of `multiplier`
- **Minimum bucket**: Charges **0 storage gas** when bucket is at minimum size
- **Scaling**: Costs increase linearly as buckets grow

**Applied When:**
SSTORE executes with `0 == original_value == current_value != new_value` (first write to an originally-zero slot in the transaction)

**Rationale:**

- Dramatically reduces costs for storage operations in fresh/lightly-used buckets
- Maintains economic disincentive for state bloat as buckets grow
- More granular pricing allows fine-tuned economic policy
- Zero cost at minimum bucket size reflects minimal incremental storage burden

#### 2.2.2 Account Creation Storage Gas

**Formula Change:**

| Spec        | Formula                     | Minimum Bucket (multiplier=1) | Double Bucket (multiplier=2) | 4× Bucket (multiplier=4) |
| ----------- | --------------------------- | ----------------------------- | ---------------------------- | ------------------------ |
| **MiniRex** | `2,000,000 × multiplier`    | 2,000,000 gas                 | 4,000,000 gas                | 8,000,000 gas            |
| **Rex**     | `25,000 × (multiplier - 1)` | **0 gas**                     | **25,000 gas**               | **75,000 gas**           |

**Key Differences:**

- **Base cost**: 25,000 gas (vs. 2M in MiniRex)
- **Formula**: Uses `(multiplier - 1)` instead of `multiplier`
- **Minimum bucket**: Charges **0 storage gas** when bucket is at minimum size

**Applied When:**

- Creating a new account via value transfer transaction (transaction targeting non-existent account)
- CALL or CALLCODE with non-zero value transfer to an empty account (per EIP-161)
- Note: Contract creation uses a separate, higher cost (see 2.2.3)

**Rationale:**

- Reduces barrier to entry for new accounts in fresh buckets
- Slightly higher base cost than SSTORE reflects account metadata overhead
- Scaling ensures economic pressure for namespace exhaustion as buckets fill

#### 2.2.3 Contract Creation Storage Gas (NEW)

**New Category:**
Rex introduces a **separate storage gas cost** specifically for contract creation, distinct from general account creation.

**Formula:**

| Spec        | Formula                     | Minimum Bucket (multiplier=1) | Double Bucket (multiplier=2) | 4× Bucket (multiplier=4) |
| ----------- | --------------------------- | ----------------------------- | ---------------------------- | ------------------------ |
| **MiniRex** | Same as account creation    | 2,000,000 gas                 | 4,000,000 gas                | 8,000,000 gas            |
| **Rex**     | `32,000 × (multiplier - 1)` | **0 gas**                     | **32,000 gas**               | **96,000 gas**           |

**Key Differences:**

- **Separate cost**: Contract creation now uses its own formula instead of reusing account creation cost
- **Base cost**: 32,000 gas (higher than account creation's 25,000 gas)
- **Formula**: Uses `(multiplier - 1)` like other Rex storage gas

**Applied When:**

- CREATE or CREATE2 opcode execution
- Contract creation transaction
- Charged regardless of whether contract creation succeeds (initcode is still executed)

**Total Contract Creation Cost:**
Contract creation pays both:

1. **Contract creation storage gas**: 32,000 × (multiplier - 1)
2. **Account creation storage gas**: 25,000 × (multiplier - 1) (if creating new account)

**Rationale:**

- Contract creation imposes higher storage burden than EOA creation (code storage, additional metadata)
- Separate cost category allows independent tuning of contract vs. EOA creation economics
- Higher base cost reflects the more complex state transitions involved

#### 2.2.7 Storage Gas Summary Table

Complete comparison of all storage gas costs:

| Operation                 | MiniRex Formula          | Rex Formula       | Change                    |
| ------------------------- | ------------------------ | ----------------- | ------------------------- |
| **Transaction Intrinsic** | N/A                      | 39,000 gas (flat) | **NEW**                   |
| **SSTORE (0→non-0)**      | 2M × m                   | 20k × (m-1)       | ✓ **Reduced**             |
| **Account Creation**      | 2M × m                   | 25k × (m-1)       | ✓ **Reduced**             |
| **Contract Creation**     | 2M × m (same as account) | 32k × (m-1)       | ✓ **Reduced + Separated** |
| **Code Deposit**          | 10k/byte                 | 10k/byte          | Same                      |
| **LOG Topic**             | 3,750/topic              | 3,750/topic       | Same                      |
| **LOG Data**              | 80/byte                  | 80/byte           | Same                      |
| **Calldata (zero)**       | 40/byte                  | 40/byte           | Same                      |
| **Calldata (non-zero)**   | 160/byte                 | 160/byte          | Same                      |
| **Floor (zero)**          | 100/byte                 | 100/byte          | Same                      |
| **Floor (non-zero)**      | 400/byte                 | 400/byte          | Same                      |

_Note: `m` = multiplier = `bucket_capacity / MIN_BUCKET_SIZE`_

### 2.3 Bug Fixes: DELEGATECALL, STATICCALL, and CALLCODE

**Critical Bug in MiniRex:**
MiniRex contained a bug where CALLCODE, DELEGATECALL, and STATICCALL incorrectly:

1. Bypass the 98/100 gas forwarding cap
2. Skip oracle contract access detection

**Rex Fixes:**
All CALL-like opcodes now properly enforce:

- 98/100 gas forwarding cap (prevents forwarding 100% of gas to subcalls)
- Oracle contract access detection (triggers 1M compute gas limit when accessing oracle)

**Impact of Fix:**

| Opcode           | MiniRex Behavior               | Rex Behavior |
| ---------------- | ------------------------------ | ------------ |
| **CALL**         | ✓ Correct (all layers)         | ✓ Same       |
| **CALLCODE**     | ❌ **Missing forward_gas_ext** | ✓ **Fixed**  |
| **DELEGATECALL** | ❌ **Missing forward_gas_ext** | ✓ **Fixed**  |
| **STATICCALL**   | ❌ **Missing forward_gas_ext** | ✓ **Fixed**  |

**Security Implications:**

- **MiniRex vulnerability**: CALLCODE, DELEGATECALL and STATICCALL could forward 100% of gas to subcalls, enabling potential gas griefing attacks
- **Rex fix**: All CALL-like opcodes properly enforce 98/100 gas forwarding, preventing call depth attacks
- **MiniRex vulnerability**: CALLCODE, DELEGATECALL and STATICCALL to oracle contract didn't trigger compute gas detention
- **Rex fix**: All CALL-like opcodes properly detect and handle oracle contract access

**Compatibility Note:**
Contracts relying on DELEGATECALL or STATICCALL forwarding 100% of gas will behave differently after Rex activation. This is a security fix, not a feature change.

## 3. Specification Mapping

The semantics of Rex spec are inherited and customized from:

- **Rex** → **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

## 4. References

- [MiniRex Specification](MiniRex.md)
- [Dual Gas Model](../docs/DUAL_GAS_MODEL.md)
- [Resource Accounting](../docs/RESOURCE_ACCOUNTING.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
- [Mega System Transactions](../docs/MEGA_SYSTEM_TRANSACTION.md)
