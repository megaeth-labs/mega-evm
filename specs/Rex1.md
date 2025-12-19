# Rex1 Specification

## 1. Introduction

Rex1 is the first patch to the Rex hardfork. It addresses a bug in the compute gas limit handling between transactions within the same block.

**Key Change:**
- **Compute Gas Limit Reset**: The transaction's compute gas limit is now properly reset between transactions, preventing the lowered limit from volatile data access in one transaction from affecting subsequent transactions.

## 2. Background

### 2.1 The Problem

In Rex (and earlier specs), when a transaction accesses volatile data such as the oracle contract, the compute gas limit is lowered to prevent excessive computation after accessing time-sensitive data. For example:

- Oracle contract access lowers compute gas limit to 1,000,000 (1M)
- Block environment access lowers compute gas limit to 20,000,000 (20M)

However, a bug existed where this lowered limit would persist to subsequent transactions executed on the same EVM instance within a block. This caused unexpected behavior:

1. **TX1**: Accesses oracle contract → compute gas limit lowered to 1M
2. **TX2**: Normal transaction requiring >1M compute gas → **fails unexpectedly**

TX2 would fail with `ComputeGasLimitExceeded` even though it never accessed any volatile data, because it inherited the lowered limit from TX1.

### 2.2 The Fix

Rex1 ensures that the compute gas limit is reset to its original configured value at the start of each transaction. This means:

1. **TX1**: Accesses oracle contract → compute gas limit lowered to 1M for this transaction only
2. **TX2**: Starts fresh with full compute gas limit (e.g., 10M) → executes normally

## 3. Specification

### 3.1 Compute Gas Limit Reset

| Spec       | Behavior |
| ---------- | -------- |
| **Pre-Rex1** | Compute gas limit persists across transactions within the same block |
| **Rex1**   | Compute gas limit resets to configured value at the start of each transaction |

### 3.2 What Gets Reset

At the start of each transaction (in the `reset()` method of `AdditionalLimit`), the following are reset when Rex1 is enabled:

- `compute_gas_limit`: Reset to the configured transaction compute gas limit
- `compute_gas_used`: Reset to 0

### 3.3 What Remains Unchanged

Rex1 does not change any other behavior from Rex:

- Storage gas economics remain the same
- Transaction intrinsic storage gas remains 39,000
- Transaction and block limits remain the same
- DELEGATECALL/STATICCALL behavior remains consistent with Rex
- All other volatile data access detection and limiting behavior remains the same

## 4. Specification Mapping

The semantics of Rex1 spec are inherited from:

- **Rex1** → **Rex** → **MiniRex** → **Optimism Isthmus** → **Ethereum Prague**

## 5. Migration Impact

### 5.1 For Node Operators

Rex1 is a bug fix that ensures correct isolation between transactions. After Rex1 activation:

- Transactions that previously failed due to inherited compute gas limits will now succeed
- Block production becomes more predictable as transactions are properly isolated

### 5.2 For Applications

No application changes are required. Rex1 only fixes a bug that caused unexpected failures. Applications that were working around this issue by avoiding oracle access in early block transactions may now remove such workarounds.

## 6. References

- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
