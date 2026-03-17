# Rex Specification

## Abstract

Rex is the first major upgrade after MiniRex.
It introduces transaction intrinsic storage gas, revised storage gas economics with lower base costs, consistent gas forwarding and oracle detection across all CALL-like opcodes, and adjusted transaction and block resource limits including a new state growth dimension.
All MiniRex semantics are preserved unless explicitly changed below.

## Changes

### 1. Transaction intrinsic storage gas

#### Motivation

MiniRex charges no storage gas at the transaction intrinsic level.
Rex adds a flat storage gas cost to every transaction to account for the per-transaction state overhead imposed on nodes.

#### Semantics

Previous behavior:
- Transaction intrinsic gas is 21,000 (compute gas only, no storage gas component).

New behavior:
- Every transaction MUST pay 39,000 additional storage gas on top of the standard 21,000 compute intrinsic gas.
- Total intrinsic gas becomes 60,000 (21,000 compute + 39,000 storage).

| Spec | Compute Gas | Storage Gas | Total |
| --- | --- | --- | --- |
| **MiniRex** | 21,000 | 0 | 21,000 |
| **Rex** | 21,000 | 39,000 | 60,000 |

### 2. Storage gas economics

#### Motivation

MiniRex uses `base × multiplier` for dynamic storage gas, which charges a full base cost even at the minimum bucket size (multiplier = 1).
Rex changes the formula to `base × (multiplier - 1)` so that operations in uncrowded state regions incur zero storage gas, while operations in crowded regions scale proportionally.

#### Semantics

Previous behavior:
- SSTORE (0→non-0): `2,000,000 × multiplier`
- Account creation: `2,000,000 × multiplier`
- Contract creation: `2,000,000 × multiplier` (same as account creation)

New behavior:
- SSTORE (0→non-0) MUST cost `20,000 × (multiplier - 1)`.
- Account creation MUST cost `25,000 × (multiplier - 1)`.
- Contract creation MUST cost `32,000 × (multiplier - 1)`.
- Contract creation of a new account MUST pay both contract creation gas and account creation gas.
- `multiplier` = `bucket_capacity / MIN_BUCKET_SIZE`.
- At `multiplier = 1`, all three operations MUST cost zero storage gas.
- Unchanged operations: code deposit (10,000/byte), LOG topic (3,750/topic), LOG data (80/byte), calldata zero (40/byte), calldata non-zero (160/byte), calldata floor zero (100/byte), calldata floor non-zero (400/byte).

| Operation | MiniRex | Rex |
| --- | --- | --- |
| **SSTORE (0→non-0)** | 2M × m | 20k × (m-1) |
| **Account creation** | 2M × m | 25k × (m-1) |
| **Contract creation** | 2M × m | 32k × (m-1) |

### 3. Consistent behavior among CALL-like opcodes

#### Motivation

In MiniRex, CALLCODE, DELEGATECALL, and STATICCALL bypass the 98/100 gas forwarding cap.
Additionally, oracle access detection only applies to CALL; STATICCALL bypasses it even when targeting the oracle contract.

#### Semantics

Previous behavior:
- Only CALL enforces 98/100 gas forwarding.
- Only CALL triggers oracle access detection.

New behavior:
- CALLCODE, DELEGATECALL, and STATICCALL MUST enforce the 98/100 gas forwarding cap, consistent with CALL.
- STATICCALL MUST trigger oracle access detection when targeting the oracle contract, consistent with CALL.
- CALLCODE and DELEGATECALL MUST NOT trigger oracle access detection, because their `target_address` equals the caller's address (not the callee's), so they never constitute a direct read of the oracle contract's state.

| Opcode | 98/100 gas forwarding | Oracle access detection |
| --- | --- | --- |
| **CALL** | MiniRex+ | MiniRex+ |
| **STATICCALL** | Rex+ | Rex+ |
| **DELEGATECALL** | Rex+ | Never |
| **CALLCODE** | Rex+ | Never |

### 4. Transaction and block limits

#### Motivation

MiniRex transaction limits for data size and KV updates are 25% of block limits.
Rex raises transaction limits to match block limits, lowers the compute gas limit, and introduces state growth limits to prevent unbounded state expansion.

#### Semantics

Previous behavior:
- Transaction data size limit: 3.125 MB (25% of block).
- Transaction KV update limit: 125,000 (25% of block).
- Transaction compute gas limit: 1,000,000,000 (1B).
- State growth: unlimited.

New behavior:
- Transaction data size limit MUST be 12.5 MB (equal to block limit).
- Transaction KV update limit MUST be 500,000 (equal to block limit).
- Transaction compute gas limit MUST be 200,000,000 (200M).
- Transaction state growth limit MUST be 1,000.
- Block state growth limit MUST be 1,000.
- Block data size limit (12.5 MB), block KV update limit (500,000), and block compute gas limit (unlimited) MUST remain unchanged.
- State growth MUST count: new storage slots (SSTORE 0→non-0) and new accounts created (via CREATE, CREATE2, or CALL with value transfer to empty account per EIP-161).
- Transaction-level state growth exceed MUST halt with `OutOfGas`.
- Block-level state growth enforcement: the last transaction exceeding the limit MUST be included; subsequent transactions MUST be rejected.

| Limit | Level | MiniRex | Rex | Change |
| --- | --- | --- | --- | --- |
| **Data Size** | Transaction | 3.125 MB | **12.5 MB** | 4× increase |
| | Block | 12.5 MB | 12.5 MB | — |
| **KV Updates** | Transaction | 125,000 | **500,000** | 4× increase |
| | Block | 500,000 | 500,000 | — |
| **Compute Gas** | Transaction | 1,000,000,000 | **200,000,000** | 5× decrease |
| | Block | Unlimited | Unlimited | — |
| **State Growth** | Transaction | Unlimited | **1,000** | New limit |
| | Block | Unlimited | **1,000** | New limit |

## Invariants

- `I-1`: Storage gas for SSTORE, account creation, and contract creation MUST be zero when `multiplier = 1`.
- `I-2`: All four CALL-like opcodes (CALL, STATICCALL, DELEGATECALL, CALLCODE) MUST enforce 98/100 gas forwarding.
- `I-3`: DELEGATECALL and CALLCODE MUST NOT trigger oracle access detection.
- `I-4`: Transaction-level state growth exceed MUST halt with `OutOfGas`.
- `I-5`: The max total gas limit (storage + compute gas) for a transaction or block is not limited by the EVM spec; it is a chain-configurable parameter.

## Inheritance

Rex inherits MiniRex except for the deltas defined in `Changes`.
Semantic lineage: `Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague`.

## References

- [MiniRex Specification](MiniRex.md)
- [Rex1 Specification](Rex1.md)
- [Rex Behavior Details (Informative)](impl/Rex-Behavior-Details.md)
- [Rex Implementation References (Informative)](impl/Rex-Implementation-References.md)
- [Dual Gas Model](../docs/DUAL_GAS_MODEL.md)
- [Resource Accounting](../docs/RESOURCE_ACCOUNTING.md)
- [Block and Transaction Limits](../docs/BLOCK_AND_TX_LIMITS.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
- [Mega System Transactions](../docs/MEGA_SYSTEM_TRANSACTION.md)
