# MiniRex Behavior Details

This document is informative.
Normative semantics are defined in [MiniRex Specification](../MiniRex.md).
If this document conflicts with the normative spec text, the normative spec wins.

## 1. Dual gas model

The `multiplier` for dynamic storage gas is derived from SALT bucket capacity: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`.
Each account and storage slot maps to a SALT bucket; gas cost scales proportionally with bucket capacity.
This makes storage operations more expensive in crowded state regions, discouraging state bloat.

For detailed gas calculation formulas and SALT bucket mechanics, see [DUAL_GAS_MODEL.md](../../docs/DUAL_GAS_MODEL.md).

## 2. Multi-dimensional resource limits

The three resource dimensions (compute gas, data size, KV updates) are tracked independently during transaction execution.
Each dimension has its own tracker that monitors usage and halts execution when the limit is exceeded.

For detailed resource accounting formulas, the two-phase checking strategy, and block construction workflow, see [BLOCK_AND_TX_LIMITS.md](../../docs/BLOCK_AND_TX_LIMITS.md) and [RESOURCE_ACCOUNTING.md](../../docs/RESOURCE_ACCOUNTING.md).

## 3. Volatile data access control

### Block environment access

Each block environment opcode has a named access type (e.g., `TIMESTAMP`, `BLOCK_NUMBER`).
Once an opcode is executed, the corresponding access type is marked and the compute gas cap is applied.
Subsequent accesses to already-marked types do not further restrict the cap.

### Beneficiary account access

The beneficiary address is the block's coinbase field.
Access includes not only explicit opcode reads (`BALANCE`, `SELFBALANCE`, etc.) but also implicit access when the transaction sender or recipient is the beneficiary.

### Oracle contract access

In MiniRex, oracle detention is CALL-based: it triggers when any CALL targets the oracle contract address.
STATICCALL is excluded from oracle detection in MiniRex — this is a known limitation that Rex fixes by adding STATICCALL to the detection.
CALLCODE and DELEGATECALL are excluded by design because they execute in the caller's state context.

The mega system address (`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`) is exempted from oracle detention to enable system operations (e.g., sequencer updating oracle storage).

### Oracle forced cold access

Oracle SLOAD is forced cold to ensure deterministic replay.
During live execution, oracle data may come from `oracle_env` (external oracle environment) or on-chain state.
Since replayers cannot determine which source was used, and `oracle_env` reads are inherently cold, forcing all oracle reads to cold access guarantees identical gas costs in both scenarios.

### Examples

**Gas detention reduces parallel conflicts without banning access.**
A DeFi contract reads `TIMESTAMP` to check whether a deadline has passed.
After the TIMESTAMP opcode executes, the transaction's remaining compute gas is capped at 20M.
The contract can still perform meaningful logic (up to 20M compute gas), but cannot monopolize execution resources after reading time-sensitive data.
Transactions that never touch volatile data face no cap at all, maximizing parallelism for pure-computation workloads.

## 4. Modified gas forwarding

The 98/100 rule replaces the standard EVM's 63/64 rule.
With MegaETH's 10B gas limit, the standard 63/64 rule leaves `10^10 × (63/64)^1024 ≈ 991 gas` after 1,024 nested calls — enough to make one more call and exceed the stack depth limit.
The 98/100 rule leaves `10^10 × (98/100)^1024 ≈ 10 gas`, preventing call depth attacks.

In MiniRex, only CALL and CREATE/CREATE2 enforce the 98/100 rule.
CALLCODE, DELEGATECALL, and STATICCALL bypass it — this is a known limitation fixed in Rex.

## 5. System contracts

The oracle contract at `0x6342000000000000000000000000000000000001` provides external key-value storage with hint support.
The high-precision timestamp oracle at `0x6342000000000000000000000000000000000002` provides sub-second block timestamps.

Both contracts are deployed idempotently: deployment only occurs if the contract is not already present or has a different code hash.
For detailed oracle service documentation, see [ORACLE_SERVICE.md](../../docs/ORACLE_SERVICE.md).

## Migration Impact

### For contracts

- Contracts can now be up to 512 KB (previously 24 KB under EIP-170).
- `SELFDESTRUCT` halts with `InvalidFEOpcode` — contracts using it must migrate to alternative patterns.

### For applications

- New storage gas costs are added on top of compute gas. Total gas cost = compute gas + storage gas.
- Transactions must respect three independent limits (compute gas, data size, KV updates). Exceeding any limit halts execution with remaining gas refunded.
- Accessing volatile data (block env, beneficiary, oracle) triggers compute gas detention. Applications should front-load volatile reads and minimize computation after access.
- Subcalls receive at most 98/100 of remaining gas — contracts depending on precise 63/64 gas forwarding may need adjustment.
- Local gas estimation tools may be inaccurate due to dynamic SALT-based storage gas multipliers and compute gas detention. Use MegaETH's native gas estimation APIs.

## References

- [MiniRex Specification](../MiniRex.md)
- [MiniRex Implementation References](MiniRex-Implementation-References.md)
