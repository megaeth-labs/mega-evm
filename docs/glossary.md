# Glossary

## Compute gas

Standard EVM gas — identical to Ethereum (Optimism Isthmus / Ethereum Prague).

Every opcode costs the same compute gas as it does on mainnet Ethereum (e.g., `ADD` = 3 gas, cold `SLOAD` = 2,100 gas).

One of the two components of total gas cost in MegaETH's [dual gas model](evm/dual-gas-model.md).

## Storage gas

Additional gas charged for operations that impose persistent storage burden on nodes (SSTORE, account creation, contract creation, code deposit, LOG, calldata).

The other component of total gas cost.

## Storage gas stipend

Additional 23,000 gas granted to the callee when `CALL` or `CALLCODE` transfers value (value > 0).

Introduced in Rex4 to compensate for the 10× storage gas multiplier on LOG opcodes, which causes LOG events to exceed the standard EVM `CALL_STIPEND` (2,300 gas).

The callee's [compute gas](#compute-gas) limit is not increased — only storage gas operations can consume the extra gas.
Unused storage gas stipend is burned on return to prevent gas leakage.

See [Rex4 Network Upgrade](upgrades/rex4.md) for details.

## SALT

Small Authentication Large Trie.

MegaETH's memory-efficient authenticated key-value store for blockchain state, replacing Ethereum's Merkle Patricia Trie.

SALT organizes state into fixed-size buckets that grow as more entries are added.

The bucket structure is the basis for dynamic storage gas pricing in MegaEVM.

See the [SALT repository](https://github.com/megaeth-labs/salt) for the data structure implementation.

## SALT bucket

The unit of storage in SALT.

Each account and storage slot maps to a bucket based on its key.

A bucket has a fixed capacity that doubles when it fills, triggering a bucket expansion.

Bucket capacity serves as a state-density metric: larger buckets indicate more crowded state regions, and storage gas scales proportionally.

Bucket capacity is determined by on-chain state of the parent block and cannot be predicted from contract code alone.

## MIN_BUCKET_SIZE

The smallest possible SALT bucket capacity, equal to **256** (2⁸).

A bucket at minimum size has `multiplier = 1`, meaning dynamic storage gas (SSTORE, account creation, contract creation) is zero in Rex+ economics.

As state density grows and buckets split, the multiplier increases.

## Multiplier

The ratio `bucket_capacity / MIN_BUCKET_SIZE` for a given SALT bucket.

At `multiplier = 1` (minimum bucket), SSTORE/account/contract creation storage gas is zero (Rex+ formula: `base × (multiplier − 1)`).

At `multiplier > 1`, storage gas scales linearly.

The multiplier is determined per-account and per-storage-slot based on which SALT bucket they reside in.

## Gas detention

A mechanism that caps remaining compute gas after a transaction accesses [volatile data](evm/gas-detention.md).

Forces transactions that read shared state to terminate quickly, reducing parallel execution conflicts.

Detained gas is refunded at transaction end.

## Volatile data

Block environment fields (NUMBER, TIMESTAMP, COINBASE, etc.), the block beneficiary's account state, and oracle contract storage.

These are frequently accessed by many transactions and are a major source of parallel execution conflicts.

## Detained limit

The effective compute gas cap imposed by [gas detention](evm/gas-detention.md).

An absolute cap on total compute gas for the transaction; if the transaction has already consumed more gas than the cap when the volatile access occurs, execution halts immediately.

> **Rex4 (unstable)**: Changes this to a relative cap — `current_usage + cap` at the time of volatile access.
> See the [Rex4 upgrade page](upgrades/rex4.md) for details.

## Beneficiary

The block coinbase address (the account that receives block rewards and priority fees).

Accessing the beneficiary's account triggers gas detention.

Not to be confused with the SELFDESTRUCT target address.

## Resource dimension

One of four independent limits enforced per transaction: compute gas, data size, KV updates, and state growth.

See [resource limits](evm/resource-limits.md).

## Call frame

A single execution context within a transaction, corresponding to a message call (CALL, STATICCALL, DELEGATECALL, CALLCODE) or a contract creation (CREATE, CREATE2) as defined in the [Ethereum Yellow Paper](https://ethereum.github.io/yellowpaper/paper.pdf).

The top-level transaction itself is also a call frame.

Call frames nest: each CALL or CREATE within a transaction creates a child call frame.

Resource trackers (data size, KV updates, state growth) are call-frame-aware — usage within a child call frame is discarded if the child reverts.

## Call-frame-local exceed

*(Rex4, unstable)* — When a call frame exceeds its per-call-frame resource budget, the call frame **reverts** with `MegaLimitExceeded(uint8 kind, uint64 limit)`.

The parent call frame can continue executing.

Distinct from a transaction-level exceed, which **halts** the entire transaction.

Per-call-frame resource budgets are introduced in Rex4.

## Spec (`MegaSpecId`)

A set of MegaEVM behaviors: what the EVM does at a given stage.

Captures only execution-layer semantics.

Progression: `EQUIVALENCE → MINI_REX → REX → REX1 → REX2 → REX3 → REX4`.

See [Hardforks and Specs](hardfork-spec.md).

## Hardfork (`MegaHardfork`)

A network upgrade event: when changes are activated on the chain.

A hardfork may include protocol-level changes beyond MegaEVM (e.g., networking, state sync, RPC behavior).

Multiple hardforks can map to the same spec (e.g., MiniRex1 → EQUIVALENCE, MiniRex2 → MINI_REX).
