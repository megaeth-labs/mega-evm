---
description: MiniRex network upgrade — the foundational MegaEVM spec built on Optimism Isthmus (Ethereum Prague).
---

# MiniRex Network Upgrade

This page is an informative summary of the MiniRex specification.
For the full normative definition, see the MiniRex spec in the mega-evm repository.

## Summary

MiniRex is the foundational MegaEVM spec, building on Optimism Isthmus (Ethereum Prague).
It introduces a **[dual gas model](../evm/dual-gas-model.md)** that separates [compute gas](../glossary.md#compute-gas) from [storage gas](../glossary.md#storage-gas), enabling independent pricing of computational work versus storage burden.
This is critical because MegaETH's extremely low base fees and high gas limits would otherwise make storage operations dramatically underpriced.

MiniRex also adds **[multidimensional resource limits](../evm/resource-limits.md)** (compute gas, data size, KV updates), **[gas detention](../evm/gas-detention.md)** to restrict computation after [volatile data](../glossary.md#volatile-data) access for parallel execution, and a **98/100 gas forwarding rule** to prevent call-depth attacks under high gas limits.
Two system contracts — [Oracle](../system-contracts/oracle.md) and [High-Precision Timestamp](../system-contracts/high-precision-timestamp.md) — provide essential infrastructure services.

## What Changed

### Dual Gas Model

#### Previous behavior
- All gas costs are a single dimension (standard EVM gas).

#### New behavior
- Every transaction's total gas cost is the sum of compute gas and storage gas.
- Compute gas covers standard EVM opcode costs.
- Storage gas is an additional charge for operations that impose persistent storage burden.
- Storage gas costs scale dynamically based on [SALT bucket](../glossary.md#salt-bucket) capacity: [`multiplier`](../glossary.md#multiplier) `= bucket_capacity /` [`MIN_BUCKET_SIZE`](../glossary.md#min_bucket_size).

Storage gas schedule:

| Operation                  | Storage Gas                 |
| -------------------------- | --------------------------- |
| **SSTORE (0 → non-0)**    | 2,000,000 × multiplier     |
| **Account creation**       | 2,000,000 × multiplier     |
| **Contract creation**      | 2,000,000 × multiplier     |
| **Code deposit**           | 10,000/byte                 |
| **LOG topic**              | 3,750/topic                 |
| **LOG data**               | 80/byte                     |
| **Calldata (zero)**        | 40/byte                     |
| **Calldata (non-zero)**    | 160/byte                    |
| **Calldata floor (zero)**  | 100/byte                    |
| **Calldata floor (non-zero)** | 400/byte                 |

### Multidimensional Resource Limits

#### Previous behavior
- A single gas limit constrains all resource types.

#### New behavior
- Three independent resource dimensions are enforced:

| Resource         | Transaction Limit         | Block Limit     |
| ---------------- | ------------------------- | --------------- |
| Compute gas      | 1,000,000,000 (1B)       | Unlimited       |
| Data size        | 3,276,800 (3.125 MB)     | 13,107,200      |
| KV updates       | 125,000                   | 500,000         |

- When any limit is exceeded, the transaction halts with `OutOfGas` and remaining gas is preserved for refund.
- All trackers are [call-frame](../glossary.md#call-frame)-aware: reverted inner calls discard their tracked usage (except compute gas, which is never reverted).

### Gas Detention (Volatile Data Access Control)

#### Previous behavior
- No restriction on computation after accessing shared state.

#### New behavior
- Accessing volatile data triggers a compute gas cap that forces the transaction to terminate quickly.
- Detained gas is refunded at transaction end.

| Category                    | Trigger                                               | Cap   |
| --------------------------- | ----------------------------------------------------- | ----- |
| Block env / [Beneficiary](../glossary.md#beneficiary) | NUMBER, TIMESTAMP, COINBASE, etc. or beneficiary access | 20M  |
| Oracle                      | CALL to [oracle contract](../system-contracts/oracle.md) address | 1M   |

- Block environment opcodes: NUMBER, TIMESTAMP, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, PREVRANDAO, BLOCKHASH, BLOBBASEFEE, BLOBHASH.
- Beneficiary access includes balance reads, code reads, and when the sender or recipient is the beneficiary.
- The most restrictive cap wins when multiple volatile sources are accessed.
- Oracle SLOAD is always forced cold (2100 gas) for deterministic replay.
- Transactions from [`MEGA_SYSTEM_ADDRESS`](../system-contracts/system-tx.md) are exempted from oracle detention.

#### Example

A DeFi contract reads `TIMESTAMP` to check whether a deadline has passed.
After the TIMESTAMP opcode executes, the transaction's remaining compute gas is capped at 20M.
The contract can still perform meaningful logic (up to 20M compute gas), but cannot monopolize execution resources after reading time-sensitive data.
Transactions that never touch volatile data face no cap at all, maximizing parallelism for pure-computation workloads.

### Modified Gas Forwarding (98/100 Rule)

#### Previous behavior
- Standard EVM: subcalls receive at most 63/64 of remaining gas ([EIP-150](https://eips.ethereum.org/EIPS/eip-150)).

#### New behavior
- Subcalls receive at most 98/100 of remaining gas.
- In MiniRex, this applies to CALL and CREATE/CREATE2 only.
- CALLCODE, DELEGATECALL, and STATICCALL are not subject to the 98/100 rule in MiniRex (fixed in Rex).

With MegaETH's high gas limits (up to 10B), the standard 63/64 rule leaves `10^10 × (63/64)^1024 ≈ 991 gas` after 1,024 nested calls — enough to exceed the stack depth limit.
The 98/100 rule reduces this to approximately 10 gas.

### SELFDESTRUCT Disabled

#### Previous behavior
- `SELFDESTRUCT` works per standard EVM semantics.

#### New behavior
- `SELFDESTRUCT` halts execution with `InvalidFEOpcode`.
- Contracts relying on self-destruct patterns must migrate to alternatives.

### Contract Size Limits

#### Previous behavior
- Maximum contract size: 24,576 bytes (24 KB, [EIP-170](https://eips.ethereum.org/EIPS/eip-170)).
- Maximum initcode size: 49,152 bytes (48 KB, [EIP-3860](https://eips.ethereum.org/EIPS/eip-3860)).

#### New behavior
- Maximum contract size: 524,288 bytes (512 KB) — a 21× increase.
- Maximum initcode size: 548,864 bytes (512 KB + 24 KB).

### Precompile Gas Overrides

#### Previous behavior
- Standard Ethereum gas costs for all precompiles.

#### New behavior
- KZG Point Evaluation (`0x0A`): 100,000 gas (2× standard Prague cost).
- ModExp (`0x05`): [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883) gas schedule.

### System Contracts

#### Previous behavior
- No MegaETH-specific system contracts.

#### New behavior
- **[Oracle](../system-contracts/oracle.md)** (`0x6342000000000000000000000000000000000001`) — Off-chain data key-value storage with hint support.
- **[High-Precision Timestamp](../system-contracts/high-precision-timestamp.md)** (`0x6342000000000000000000000000000000000002`) — Sub-second block timestamp.
- Both contracts are deployed idempotently during block execution when MiniRex activates.

## Developer Impact

**Storage gas is a new cost dimension you must account for.**
Total gas cost = compute gas + storage gas.
Local gas estimation tools may be inaccurate due to dynamic [SALT](../glossary.md#salt)-based [multipliers](../glossary.md#multiplier) — use MegaETH's native gas estimation APIs.

**Your transactions must respect three independent limits** (compute gas, data size, KV updates).
Exceeding any one of them halts execution with remaining gas refunded.

**Accessing volatile data triggers compute gas detention.**
If your contract reads block environment fields, the beneficiary's account, or oracle storage, remaining compute gas is capped.
Front-load volatile reads and minimize computation after access.

**Subcalls receive at most 98/100 of remaining gas.**
Contracts depending on precise 63/64 gas forwarding behavior may need adjustment.

**SELFDESTRUCT is disabled** — contracts using it will halt with `InvalidFEOpcode`.

**Contracts can now be up to 512 KB**, enabling significantly larger deployments.

## Safety and Compatibility

MiniRex builds on Optimism Isthmus.
All parent-layer semantics are inherited unless explicitly overridden.

Transaction-level resource violations halt the transaction with `OutOfGas` and refund remaining gas.
The transaction fails (status=0) but is still included in the block.

Gas detention caps are absolute — if compute gas consumed already exceeds the cap when volatile data is accessed, execution halts immediately.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Dual Gas Model](../evm/dual-gas-model.md) — compute gas vs storage gas details
- [Resource Limits](../evm/resource-limits.md) — limit values and enforcement behavior
- [Gas Detention](../evm/gas-detention.md) — volatile data detection and cap mechanics
