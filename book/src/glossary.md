# Glossary

**Compute gas** — The gas cost for pure computational work (opcode execution, memory expansion, precompiles).
Standard Optimism EVM (Isthmus) gas costs.
One of the two components of total gas cost in MegaETH's [dual gas model](evm/dual-gas-model.md).

**Storage gas** — Additional gas charged for operations that impose persistent storage burden on nodes (SSTORE, account creation, contract creation, code deposit, LOG, calldata).
The other component of total gas cost.

**SALT bucket** — A state-density metric used by MegaETH to price storage operations dynamically.
Each account and storage slot maps to a bucket.
As a bucket grows (more state entries), its capacity increases and storage gas costs scale proportionally.
Bucket capacity is determined by on-chain state and cannot be predicted from contract code alone.

**Multiplier** — The ratio `bucket_capacity / MIN_BUCKET_SIZE` for a given SALT bucket.
At `multiplier = 1` (minimum bucket), SSTORE/account/contract creation storage gas is zero.
At `multiplier > 1`, storage gas scales linearly.

**Gas detention** — A mechanism that caps remaining compute gas after a transaction accesses [volatile data](evm/gas-detention.md).
Forces transactions that read shared state to terminate quickly, reducing parallel execution conflicts.
Detained gas is refunded at transaction end.

**Volatile data** — Block environment fields (NUMBER, TIMESTAMP, COINBASE, etc.), the block beneficiary's account state, and oracle contract storage.
These are frequently accessed by many transactions and are a major source of parallel execution conflicts.

**Detained limit** — The effective compute gas cap imposed by gas detention.
In Rex4 (relative cap): `current_usage + cap` at the time of volatile access.
Pre-Rex4 (absolute cap): `cap` total, halting if already exceeded.

**Beneficiary** — The block coinbase address (the account that receives block rewards and priority fees).
Accessing the beneficiary's account triggers gas detention.
Not to be confused with the SELFDESTRUCT target address.

**Resource dimension** — One of four independent limits enforced per transaction: compute gas, data size, KV updates, and state growth.
See [resource limits](evm/resource-limits.md).

**Frame** / **Call frame** — A single execution context within a transaction, corresponding to a CALL, STATICCALL, DELEGATECALL, CALLCODE, CREATE, or CREATE2 invocation.
Starting from Rex4, each frame receives a bounded share (98/100) of its parent's remaining resource budget.

**Frame-local exceed** — When a call frame exceeds its per-frame resource budget, the frame **reverts** with `MegaLimitExceeded(uint8 kind, uint64 limit)`.
The parent frame can continue executing.
Distinct from a transaction-level exceed, which **halts** the entire transaction.

**Spec (`MegaSpecId`)** — Defines EVM behavior: what the EVM does at a given stage.
Progression: `EQUIVALENCE → MINI_REX → REX → REX1 → REX2 → REX3 → REX4`.
See [spec system](evm/spec-system.md).

**Hardfork (`MegaHardfork`)** — Defines a network upgrade event: when a spec is activated.
Multiple hardforks can map to the same spec (e.g., MiniRex1 → EQUIVALENCE, MiniRex2 → MINI_REX).
