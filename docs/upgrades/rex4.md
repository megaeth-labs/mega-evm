---
description: Rex4 adds per-call-frame resource budgets, a storage gas stipend for value transfers, relative gas detention, two new system contracts for access control and limit queries, and keyless deploy sandbox improvements.
---

# Rex4 Network Upgrade

This page is an informative summary of the Rex4 specification.
For the full normative definition, see the Rex4 spec in the mega-evm repository.

Rex4 is the current unstable specification and is subject to change before activation.

## Summary

Rex4 introduces **per-[call-frame](../glossary.md#call-frame) resource budgets** across all four [resource dimensions](../glossary.md#resource-dimension) ([compute gas](../glossary.md#compute-gas), data size, KV updates, and state growth).
Before Rex4, [resource limits](../evm/resource-limits.md) only applied at the transaction level, which meant a single inner call could consume nearly the entire budget and leave parent or sibling calls unable to execute predictably.
Per-call-frame budgets give each call frame a bounded share of remaining resources, making nested execution more predictable for contract authors.

Rex4 also shifts [gas detention](../evm/gas-detention.md) from absolute caps to **relative caps**, so transactions that access [volatile data](../glossary.md#volatile-data) late in execution are no longer penalized for compute work done before the access.
Two new [system contracts](../system-contracts/overview.md) — **MegaAccessControl** and **MegaLimitControl** — give contracts runtime control over volatile data access and the ability to query their effective remaining compute gas budget.

Rex4 also introduces a **storage gas stipend** for value-transferring calls, so that contracts receiving ETH via `transfer()` or `send()` can emit events without running out of gas.

Finally, the [keyless deploy](../system-contracts/keyless-deploy.md) sandbox now inherits the parent transaction's external environment for dynamic gas pricing and [oracle](../system-contracts/oracle.md) behavior, improving accuracy for contracts deployed via Nick's Method.

## What Changed

### Per-Call-Frame Resource Budgets

#### Previous behavior
- Data size, KV updates, compute gas, and state growth were constrained only by transaction-level limits.
- An inner call could consume nearly the entire remaining budget.

#### New behavior
- The top-level call frame starts with the full transaction budget for each resource dimension.
- Each inner call frame receives `remaining × 98 / 100` of its parent's remaining budget.
- When a call frame exceeds its local budget, it reverts with [`MegaLimitExceeded(uint8 kind, uint64 limit)`](../glossary.md#call-frame-local-exceed) (does not halt the transaction).
- The parent call frame can continue executing after a child call frame reverts due to a call-frame-local limit.
- Transaction-level exceeds still halt the entire transaction with `OutOfGas`.
- Compute gas consumed by reverted child frames still counts toward the transaction's total compute gas usage.

The `kind` discriminator identifies which resource was exceeded:

| kind | Resource    |
| ---- | ----------- |
| 0    | Data size   |
| 1    | KV updates  |
| 2    | Compute gas |
| 3    | State growth|

### MegaAccessControl System Contract

**Address**: `0x6342000000000000000000000000000000000004`

#### Previous behavior
- No runtime mechanism existed to proactively disable volatile data access within a call subtree.
- Gas detention was purely reactive — it only applied after volatile data was accessed.

#### New behavior
- You can disable volatile data access for your call frame and all descendant calls.
- While disabled, any volatile access (block environment reads, beneficiary-targeted account access including `SELFDESTRUCT` to the beneficiary, oracle storage reads) reverts immediately with `VolatileDataAccessDisabled(uint8 accessType)`.
- Blocked volatile access does not update volatile-access tracking and does not tighten gas detention.
- A descendant call frame cannot re-enable access disabled by an ancestor — attempting to do so reverts with `DisabledByParent()`.
- The restriction automatically ends when the disabling call frame returns.
- Calls with non-zero value transfer revert with `NonZeroTransfer()`.

```solidity
interface IMegaAccessControl {
    enum VolatileDataAccessType {
        BlockNumber,    // 0  — NUMBER opcode
        Timestamp,      // 1  — TIMESTAMP opcode
        Coinbase,       // 2  — COINBASE opcode
        Difficulty,     // 3  — DIFFICULTY opcode
        GasLimit,       // 4  — GASLIMIT opcode
        BaseFee,        // 5  — BASEFEE opcode
        PrevRandao,     // 6  — PREVRANDAO opcode
        BlockHash,      // 7  — BLOCKHASH opcode
        BlobBaseFee,    // 8  — BLOBBASEFEE opcode
        BlobHash,       // 9  — BLOBHASH opcode
        Beneficiary,    // 10 — account access targeting the block beneficiary
        Oracle          // 11 — SLOAD on oracle contract
    }

    error NotIntercepted();
    error NonZeroTransfer();
    error VolatileDataAccessDisabled(VolatileDataAccessType accessType);
    error DisabledByParent();

    /// @notice Disables volatile data access for the caller's call frame and all inner calls.
    function disableVolatileDataAccess() external view;

    /// @notice Re-enables volatile data access for the caller's call frame and inner calls.
    /// @dev Reverts with DisabledByParent() if an ancestor call frame disabled access.
    function enableVolatileDataAccess() external view;

    /// @notice Queries whether volatile data access is disabled at the current call depth.
    function isVolatileDataAccessDisabled() external view returns (bool disabled);
}
```

### MegaLimitControl System Contract

**Address**: `0x6342000000000000000000000000000000000005`

#### Previous behavior
- No dedicated system query returned the effective remaining compute gas.
- Contracts had no way to determine their actual compute budget under detention and call frame limits.

#### New behavior
- You can query your effective remaining compute gas at any point during execution.
- The returned value equals `min(frame_remaining, tx_detained_remaining)` at call time.
- This is a snapshot — the value decreases as execution proceeds.
- Calls with non-zero value transfer revert with `NonZeroTransfer()`.

```solidity
interface IMegaLimitControl {
    error NotIntercepted();
    error NonZeroTransfer();

    /// @notice Returns remaining compute gas of the current call.
    /// @return remaining The effective remaining compute gas.
    function remainingComputeGas() external view returns (uint64 remaining);
}
```

### Storage Gas Stipend for Value Transfers

#### Previous behavior

- The EVM's `CALL_STIPEND` (2,300 gas) was the only gas available to the callee when receiving ETH via `transfer()` or `send()`.
- On standard Ethereum, 2,300 gas is sufficient for a simple LOG event (e.g., `LOG1` costs 750 gas).
- On MegaETH, the 10× [storage gas](../evm/dual-gas-model.md) multiplier on LOG opcodes causes even `LOG1` to cost 4,500 gas (750 compute + 3,750 storage), exceeding the stipend.
- Any contract that emits an event in its `receive()` or `fallback()` function would revert when called via `transfer()` or `send()`.
- Affected contracts include WETH9 (whose `withdraw()` uses `transfer()`), Gnosis Safe, and smart contract wallets that emit events on ETH receipt.

#### New behavior

- When `CALL` or `CALLCODE` transfers value (value > 0), the callee receives an additional **storage gas stipend** of 23,000 gas on top of the standard `CALL_STIPEND` (2,300).
- The callee's total gas becomes: `forwarded_gas + CALL_STIPEND (2,300) + STORAGE_GAS_STIPEND (23,000)`.
- The callee's [compute gas](../glossary.md#compute-gas) limit remains at the original level (`forwarded_gas + CALL_STIPEND`), so the extra gas can only be consumed by [storage gas](../glossary.md#storage-gas) operations (the 10× LOG topic/data costs).
- On return, unused storage gas stipend is **burned** — it is never returned to the caller.
- `DELEGATECALL` and `STATICCALL` are unaffected (no value transfer, no stipend).
- The compute gas cap ensures the callee cannot perform state-modifying operations (SSTORE, CALL with value, CREATE) with the extra gas, preserving the reentrancy protection properties of the original `CALL_STIPEND`.

| Event type | Compute gas | Storage gas | Total gas | Fits in 25,300? |
| ---------- | ----------- | ----------- | --------- | --------------- |
| LOG1 (1 topic, 0 bytes) | 750 | 3,750 | 4,500 | ✅ |
| LOG2 (SafeReceived) | 1,125 | 7,500 | 8,625 | ✅ |
| LOG3 (Transfer) | 1,500 | 11,250 | 12,750 | ✅ |
| LOG4 + 32 bytes data | ~2,075 | 17,560 | ~19,635 | ✅ |
| LOG4 + 64 bytes data | ~2,387 | 20,120 | ~22,507 | ✅ |

### Relative Gas Detention Cap

#### Previous behavior
- Gas detention applied an absolute cap per volatile-access category.
- If a transaction had already consumed more compute gas than the cap before accessing volatile data, execution halted immediately.

#### New behavior
- The effective [detained limit](../glossary.md#detained-limit) is `current_usage + cap` at the time of volatile access.
- Execution continues up to `min(tx_compute_limit, effective_detained_limit)`.
- Across multiple volatile accesses, the most restrictive effective limit applies.
- Transactions that access volatile data late in execution can still use the full cap amount of compute gas after the access.

### Keyless Deploy Sandbox Environment Inheritance

#### Previous behavior
- The keyless deploy sandbox used fixed 1× pricing for all storage-related operations.
- Oracle hints emitted during sandbox execution were not forwarded to the parent context.

#### New behavior
- The sandbox inherits the parent transaction's external environment for dynamic gas pricing (SALT bucket multipliers) and oracle behavior.
- Oracle hints emitted during sandbox execution are forwarded to the parent context.
- The sandbox maintains its own isolated cache to prevent pollution of the parent's state.
- Pre-Rex4 specs retain the prior sandbox behavior.

## Developer Impact

**Contract authors writing nested call patterns** should be aware that each inner call frame now receives at most 98% of the parent's remaining resource budget.
If your contract makes deeply nested calls, the innermost call frames will have progressively smaller budgets.
Design your call depth and resource usage accordingly.

**Contracts that catch reverts from inner calls** can now decode `MegaLimitExceeded(uint8 kind, uint64 limit)` from the revert data to determine whether a child call failed due to a call-frame-local resource limit.
You can use this to implement fallback logic or graceful degradation.

**Contracts that want to avoid gas detention** can use `MegaAccessControl.disableVolatileDataAccess()` at the start of a call to guarantee that no descendant will trigger detention.
Any accidental volatile access in the subtree will revert immediately rather than silently tightening your gas budget.
This is useful for library contracts or aggregators that call untrusted code.

**Contracts that need to know their effective compute budget** can call `MegaLimitControl.remainingComputeGas()` to get a runtime snapshot.
This accounts for both call-frame-level budgets and detention, giving you a single reliable number for gas-aware logic.

**Contracts receiving ETH via `transfer()` or `send()`** can now safely emit events in their `receive()` or `fallback()` functions.
The storage gas stipend provides enough gas for LOG operations with the 10× storage gas multiplier.
No changes are needed to existing contracts — this fix is transparent to both senders and receivers.

**Deployers using keyless deployment** will see more accurate gas costs in Rex4 because the sandbox now uses the same dynamic pricing as the parent transaction.
If you previously needed extra gas headroom for keyless deploys, you may be able to reduce your `gasLimitOverride`.

## Safety and Compatibility

All pre-Rex4 behavior is unchanged.
Contracts deployed and running on Rex3 or earlier specs continue to execute with the same semantics.

Call-frame-local resource exceeds **revert** the call frame — they do not halt the transaction.
Gas is returned to the parent, and the parent can continue.
This is different from transaction-level exceeds, which **halt** the entire transaction with `OutOfGas`.

Compute gas consumed by reverted child frames still counts toward the transaction's total.
This is consistent with how compute gas has always worked (CPU cycles cannot be undone).

The detained compute gas limit is monotonically non-increasing across a transaction.
Once volatile data is accessed, no subsequent operation can raise the detained limit.

`MegaAccessControl` and `MegaLimitControl` intercept `CALL` and `STATICCALL` only.
`DELEGATECALL` and `CALLCODE` to these addresses are not intercepted and fall through to on-chain bytecode, which reverts with `NotIntercepted()`.

The storage gas stipend is burned on return — the caller never recovers the unused portion.
This prevents contracts from exploiting value-transferring calls to generate free gas.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Dual Gas Model](../evm/dual-gas-model.md) — how compute gas and storage gas interact
- [Resource Accounting](../evm/resource-accounting.md) — how MegaETH tracks resource usage across all dimensions
- [Gas Detention](../evm/gas-detention.md) — background on the gas detention mechanism
