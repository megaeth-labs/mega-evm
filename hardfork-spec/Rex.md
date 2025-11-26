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
MiniRex contained a bug where DELEGATECALL, STATICCALL, and CALLCODE incorrectly:

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
| **CALLCODE**     | ✓ Correct (all layers)         | ✓ Same       |
| **DELEGATECALL** | ❌ **Missing forward_gas_ext** | ✓ **Fixed**  |
| **STATICCALL**   | ❌ **Missing forward_gas_ext** | ✓ **Fixed**  |

**Security Implications:**

- **MiniRex vulnerability**: DELEGATECALL and STATICCALL could forward 100% of gas to subcalls, enabling potential gas griefing attacks
- **Rex fix**: All CALL-like opcodes properly enforce 98/100 gas forwarding, preventing call depth attacks
- **MiniRex vulnerability**: DELEGATECALL and STATICCALL to oracle contract didn't trigger compute gas detention
- **Rex fix**: All CALL-like opcodes properly detect and handle oracle contract access

**Compatibility Note:**
Contracts relying on DELEGATECALL or STATICCALL forwarding 100% of gas will behave differently after Rex activation. This is a security fix, not a feature change.

### 2.4 Unchanged MiniRex Features

The following MiniRex features are **inherited without changes**:

- **Contract Size Limits**: 512 KB max contract size, 536 KB max initcode size
- **SELFDESTRUCT Deprecation**: Remains disabled with `InvalidFEOpcode` error
- **Increased Precompile Costs**: KZG Point Evaluation (2× cost), ModExp (EIP-7883)
- **98/100 Gas Forwarding**: Applies to all CALL-like opcodes (now including DELEGATECALL and STATICCALL)
- **Multi-dimensional Resource Limits**:
  - Compute Gas: 1B per tx
  - Data Size: 3.125 MB per tx, 12.5 MB per block
  - KV Updates: 125K per tx, 500K per block
- **Volatile Data Access Control**:
  - Block environment opcodes → 20M compute gas limit
  - Beneficiary account access → 20M compute gas limit
  - Oracle contract access → 1M compute gas limit
- **Oracle Contract**: Deployed at `0x6342000000000000000000000000000000000001`
- **Timestamp Oracle Periphery**: Deployed at `0x6342000000000000000000000000000000000002`

## 3. Specification Mapping

The semantics of Rex spec are inherited and customized from:

- **Rex** → **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

## 4. Migration Impact

### 4.1 From MiniRex to Rex

**Storage Gas Cost Changes:**

Most applications will see **significantly reduced** storage gas costs:

| Operation         | MiniRex (min bucket) | Rex (min bucket) | Savings            |
| ----------------- | -------------------- | ---------------- | ------------------ |
| Simple transfer   | 21,000 gas           | 60,000 gas       | **-39,000** gas    |
| SSTORE (0→non-0)  | 2,000,000 gas        | 0 gas            | **+2,000,000** gas |
| Account creation  | 2,000,000 gas        | 0 gas            | **+2,000,000** gas |
| Contract creation | 2,000,000 gas        | 0 gas            | **+2,000,000** gas |

**Net Effect Examples:**

_Simple ETH transfer:_

- MiniRex: 21,000 gas
- Rex: 60,000 gas
- Impact: **+39,000 gas** (+186%)

_First SSTORE to new slot:_

- MiniRex: 21,000 + 22,100 (EVM) + 2,000,000 (storage) = 2,043,100 gas
- Rex: 60,000 + 22,100 (EVM) + 0 (storage) = 82,100 gas
- Impact: **-1,961,000 gas** (-96%)

_Contract creation with 100-byte code:_

- MiniRex: 21,000 + 32,000 (CREATE) + 2,000,000 (account) + 1,000,000 (code) = 3,053,000 gas
- Rex: 60,000 + 32,000 (CREATE) + 0 (account) + 0 (contract) + 1,000,000 (code) = 1,092,000 gas
- Impact: **-1,961,000 gas** (-64%)

**As buckets grow:**

- Rex costs increase linearly with bucket multiplier
- At multiplier=100 (large bucket): Rex costs approach but remain lower than MiniRex
- Example SSTORE at m=100: MiniRex = 200M gas, Rex = 1.98M gas

**DELEGATECALL and STATICCALL Behavior:**

Contracts using DELEGATECALL or STATICCALL will experience two changes:

1. **98/100 Gas Forwarding (Bug Fix):**

   - These opcodes now properly forward at most 98/100 of remaining gas
   - Contracts expecting to forward 100% of gas will receive less
   - This is a security fix that prevents call depth attacks

2. **Oracle Access Detection (Bug Fix):**
   - DELEGATECALL and STATICCALL to oracle contract now trigger 1M compute gas limit
   - Previously undetected in MiniRex

**Recommended Actions:**

- Review gas estimation for storage-heavy operations (likely reduced costs)
- Update any contracts using precise gas forwarding with DELEGATECALL/STATICCALL
- Re-benchmark transaction costs for common operations
- Test contracts that extensively use DELEGATECALL or STATICCALL

### 4.2 For New Deployments

**Advantages of Rex Storage Model:**

- **Lower entry costs**: Creating new accounts and contracts in fresh buckets costs near-zero storage gas
- **Predictable scaling**: Storage costs increase linearly and gradually as buckets grow
- **Economic sustainability**: High multipliers still create economic pressure against state bloat

**Considerations:**

- **Transaction intrinsic cost**: All transactions pay minimum 60,000 gas (vs. 21,000 in standard EVM)
- **Bucket dynamics**: Storage gas costs depend on bucket capacity, which grows over time
- **Gas estimation**: Use MegaETH's native gas estimation APIs for accurate predictions

## 5. Economic Rationale

### 5.1 Why Change the Storage Gas Formula?

**MiniRex's Conservative Approach:**

- Used flat 2M gas cost per operation to ensure economic sustainability
- Successfully prevented state bloat but created high barriers to entry
- Made experimentation and development expensive even in fresh state

**Rex's Refined Model:**

- Zero storage gas in minimum buckets reflects minimal incremental cost
- Linear scaling with `(multiplier - 1)` provides gradual economic pressure
- Transaction intrinsic storage gas (39,000 gas) ensures baseline cost recovery
- Still prevents state bloat through dynamic bucket-based pricing

### 5.2 Why Separate Contract Creation?

**Different Economic Characteristics:**

- Contract creation involves more state changes (code storage, additional metadata)
- Contracts typically have longer lifetime than EOAs
- Higher base cost (32k vs. 25k) reflects greater infrastructure burden

**Independent Tuning:**

- Allows separate economic policy for contract vs. EOA creation
- Can adjust contract creation costs without affecting general account creation
- Provides flexibility for future optimizations

### 5.3 Transaction Intrinsic Storage Gas Rationale

**Fixed Overhead Costs:**

- Signature verification (ECDSA recovery)
- Transaction validation and decoding
- State propagation to replicas
- Nonce verification and sender account updates

**Economic Balance:**

- 39,000 intrinsic storage gas prevents ultra-cheap spam
- Still dramatically lower than traditional networks (consider total tx cost)
- Combined with reduced storage costs, net effect favors legitimate usage

## 6. Implementation Notes

### 6.1 Dynamic Gas Calculation

Storage gas costs are calculated dynamically based on SALT bucket capacity:

```rust
// SSTORE storage gas
fn sstore_set_gas(bucket_capacity: u64) -> u64 {
    let multiplier = bucket_capacity / MIN_BUCKET_SIZE;
    20_000 * (multiplier - 1)
}

// Account creation storage gas
fn new_account_gas(bucket_capacity: u64) -> u64 {
    let multiplier = bucket_capacity / MIN_BUCKET_SIZE;
    25_000 * (multiplier - 1)
}

// Contract creation storage gas
fn create_contract_gas(bucket_capacity: u64) -> u64 {
    let multiplier = bucket_capacity / MIN_BUCKET_SIZE;
    32_000 * (multiplier - 1)
}
```

### 6.2 Bucket Multiplier Caching

The implementation caches bucket multipliers during transaction execution to avoid redundant lookups:

```rust
pub struct DynamicGasCost {
    bucket_cost_multipliers: HashMap<BucketId, u64>,
    // ... other fields
}
```

### 6.3 Instruction Table Overrides

Rex modifies the MiniRex instruction table to fix DELEGATECALL and STATICCALL:

```rust
let mut table = mini_rex::instruction_table();

// Fix MiniRex bugs
table[CALLCODE as usize] = forward_gas_ext::call_code;
table[DELEGATECALL as usize] = forward_gas_ext::delegate_call;
table[STATICCALL as usize] = forward_gas_ext::static_call;
```

## 7. Testing and Validation

### 7.1 Test Coverage

Rex implementation includes comprehensive tests:

- **Storage gas tests** ([`tests/rex/storage_gas.rs`](../crates/mega-evm/tests/rex/storage_gas.rs)): 813 lines covering SSTORE, account creation, contract creation with various bucket sizes
- **Intrinsic gas tests** ([`tests/rex/intrinsic_gas.rs`](../crates/mega-evm/tests/rex/intrinsic_gas.rs)): Transaction floor gas validation
- **Opcode-level tests**: Direct testing of CREATE, CREATE2, CALL, CALLCODE, DELEGATECALL, STATICCALL

### 7.2 Key Test Scenarios

1. **Minimum bucket (multiplier=1)**: Verify 0 storage gas for SSTORE, accounts, contracts
2. **Growing buckets**: Validate linear cost increase with multiplier
3. **Transaction floor**: Confirm all transactions pay 60,000 base cost
4. **Gas forwarding**: Ensure DELEGATECALL and STATICCALL respect 98/100 rule
5. **Oracle access**: Verify all CALL-like opcodes detect oracle contract

## 8. References

- [MiniRex Specification](MiniRex.md)
- [Dual Gas Model](../docs/DUAL_GAS_MODEL.md)
- [Resource Accounting](../docs/RESOURCE_ACCOUNTING.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
- [Mega System Transactions](../docs/MEGA_SYSTEM_TRANSACTION.md)
