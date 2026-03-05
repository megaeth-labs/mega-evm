# Rex4 Specification

Rex4 is the fourth patch to the Rex hardfork.
It introduces per-frame state growth limits while inheriting all Rex3 behavior.

## Changes from Rex3

### 1. Per-Frame State Growth Limits

Rex4 introduces **per-frame state growth budgets** to prevent a single inner call from consuming the entire transaction's state growth budget.

#### Problem

Prior to Rex4, the TX-level state growth limit (1000 new accounts/slots per transaction) is enforced globally.
An inner call frame can consume the entire budget, leaving nothing for the parent frame.
This makes it difficult for contracts to reason about remaining state growth capacity after calling external contracts.

#### Solution

Each inner call frame receives a fraction of the parent's remaining state growth budget:

- **Top-level frame**: Gets the full TX state growth limit (no reduction).
- **Inner frames**: Get `remaining * 98 / 100` of the parent's remaining budget.

The 98/100 ratio ensures that the parent always retains at least 2% of its remaining budget after spawning a child frame.

Only state growth has per-frame enforcement.
The other three resource dimensions (compute gas, data size, KV updates) continue to use TX-level-only enforcement.

#### Constants

| Constant                  | Value | Description                         |
| ------------------------- | ----- | ----------------------------------- |
| `FRAME_LIMIT_NUMERATOR`   | 98    | Numerator of the forwarding ratio   |
| `FRAME_LIMIT_DENOMINATOR` | 100   | Denominator of the forwarding ratio |

#### Frame Budget Calculation

When a new inner call frame is created:

1. Compute `remaining` from the parent frame:
   - `remaining = parent_limit - (used - refund)`, clamped to `[0, parent_limit]`
   - For the top-level frame, `parent_limit` is the TX state growth limit
2. Compute child budget: `child_limit = remaining * 98 / 100`

#### Example

```text
TX state growth limit: 1000
Top-level frame limit: 1000

Top-level calls Child A:
  Child A budget = 1000 * 98/100 = 980
  Child A creates 100 storage slots (growth = 100)

  Child A calls Grandchild B:
    Remaining for Child A = 980 - 100 = 880
    Grandchild B budget = 880 * 98/100 = 862
    Grandchild B creates 862 slots → at frame limit → reverted (absorbed)

Top-level calls Child C (after Child A succeeds):
  Growth so far = 100 (Grandchild B's growth was discarded)
  Remaining = 1000 - 100 = 900
  Child C budget = 900 * 98/100 = 882
```

#### Limit Exceeding Semantics

When an inner frame exceeds its frame-local state growth limit:

1. The frame's result is changed to **Revert** (not Halt).
2. Gas is returned to the parent frame (not consumed).
3. The parent can continue executing after the reverted child call.
4. The child's state growth is discarded (standard revert behavior).
5. The revert data is ABI-encoded as `MegaLimitExceeded(uint8 kind, uint64 limit)` (see below).

This is different from TX-level limit enforcement, which halts the entire transaction with `OutOfGas`.

#### Revert Data Encoding

When a frame-local limit exceed triggers a revert, the revert output carries ABI-encoded data so that parent contracts can identify the cause:

```solidity
error MegaLimitExceeded(uint8 kind, uint64 limit);
```

| Field   | Type     | Description                           |
| ------- | -------- | ------------------------------------- |
| `kind`  | `uint8`  | Which resource limit was exceeded     |
| `limit` | `uint64` | The configured limit for this frame   |

The `kind` discriminant values are:

| Value | Resource     |
| ----- | ------------ |
| 0     | Data size    |
| 1     | KV updates   |
| 2     | Compute gas  |
| 3     | State growth |

Currently only state growth has per-frame enforcement, so in practice `kind = 3` is the only value emitted.
The encoding is general to support future per-frame limits for other resource dimensions.

Parent contracts can detect a frame-local limit exceed via low-level call return data or try/catch, and decode it to determine which limit was hit and what the frame's budget was.

### 2. MegaAccessControl System Contract

Rex4 introduces the **MegaAccessControl** system contract at address `0x6342000000000000000000000000000000000004`.
This contract provides access restriction functions that are intercepted and enforced by the EVM.

#### Volatile Data

The following are considered **volatile data**:
- Block environment fields: TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, BLOCKHASH, BLOBBASEFEE, BLOBHASH
- Beneficiary account access: BALANCE, EXTCODESIZE, EXTCODECOPY, EXTCODEHASH, CALL, STATICCALL, DELEGATECALL, CALLCODE, SELFDESTRUCT when targeting the block beneficiary
- Oracle contract access: SLOAD on the oracle contract

#### `disableVolatileDataAccess()`

When a contract calls `MegaAccessControl.disableVolatileDataAccess()`, the caller's frame and all inner calls that access volatile data will revert with `VolatileDataAccessDisabled(uint8 accessType)` error.

**Semantics:**
- The caller's own frame **is** restricted.
  Accessing volatile data after calling `disableVolatileDataAccess()` in the same frame will revert.
- The restriction applies to all inner call types: CALL, STATICCALL, DELEGATECALL, CALLCODE.
- When a restricted frame accesses volatile data, it immediately reverts with ABI-encoded `VolatileDataAccessDisabled(uint8 accessType)` error.
  The `accessType` parameter identifies which volatile data triggered the revert (see `VolatileDataAccessType` enum below).
- The reverted call returns gas to the parent frame (standard revert behavior).
- Calls carrying non-zero transferred ETH revert and do not modify the disable state.
- If called multiple times at different depths, the shallowest (more restrictive) activation depth is kept.
- The restriction is **scoped to the caller's subtree**: when the frame that called `disableVolatileDataAccess()` returns, the restriction is deactivated.
  Sibling calls made by the parent after the disabling frame returns are not affected.
- Blocked volatile data accesses do **not** set the `volatile_data_accessed` bitmap or lower the compute gas limit (gas detention).
  The access is rejected before it can affect the transaction-level tracking state.

#### `enableVolatileDataAccess()`

Re-enables volatile data access for the caller's frame and all inner calls.

**Semantics:**
- Succeeds if access is not currently disabled, or if the caller (or a frame at the same depth) was the one that disabled it.
- Reverts with `DisabledByParent()` (selector `0xcc11219e`) if a parent frame (shallower depth) disabled access.
  A child frame cannot override a parent's restriction.
- After a successful call, both the caller and its inner calls can access volatile data again.

#### `isVolatileDataAccessDisabled()`

Queries whether volatile data access is disabled at the caller's current call depth.

**Semantics:**
- Returns `true` if volatile data access is disabled at the caller's depth, `false` otherwise.
- This is a read-only query; it does not modify the disable state.

#### `VolatileDataAccessType` Enum

The `VolatileDataAccessType` enum identifies which type of volatile data triggered the revert.
Discriminant values match the bit positions in the internal `VolatileDataAccess` bitmap.

```solidity
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
    Beneficiary,    // 10 — BALANCE/EXTCODESIZE/EXTCODECOPY/EXTCODEHASH/CALL/STATICCALL/DELEGATECALL/CALLCODE/SELFDESTRUCT on beneficiary
    Oracle          // 11 — SLOAD on oracle contract
}
```

#### Contract Interface

```solidity
interface IMegaAccessControl {
    error NotIntercepted();
    error VolatileDataAccessDisabled(VolatileDataAccessType accessType);
    error DisabledByParent();
    function disableVolatileDataAccess() external view;
    function enableVolatileDataAccess() external view;
    function isVolatileDataAccessDisabled() external view returns (bool disabled);
}
```

The contract body reverts with `NotIntercepted()` if called outside the MegaETH EVM (e.g., on a network that doesn't support this feature).

#### DELEGATECALL / CALLCODE

DELEGATECALL and CALLCODE to the MegaAccessControl system contract are **not intercepted**.
The EVM interception only triggers on CALL and STATICCALL (where `target_address` equals the system contract address).
For DELEGATECALL and CALLCODE, `target_address` is the caller's own address (not the code address), so the interception does not match.
The call falls through to the underlying contract code, which reverts with `NotIntercepted()`.

### 3. MegaLimitControl System Contract

Rex4 introduces the **MegaLimitControl** system contract at address `0x6342000000000000000000000000000000000005`.
This contract currently provides a read-only query for transaction-level remaining compute gas.
The runtime value is returned by EVM interception.

#### `remainingComputeGas()`

Returns the remaining transaction-level compute gas as `uint64`.
The value is computed as `max(0, effective_tx_compute_limit - tx_compute_used)`.
`effective_tx_compute_limit` includes gas detention (`min(tx_limit, detained_limit)`).
`tx_compute_used` includes all compute usage accumulated so far in the transaction.

This query is TX-scoped.
It is not a per-frame remaining budget query.
In Rex4 where per-frame compute budgets exist, the returned value may be larger than the remaining budget of the current call frame.
The returned value is a snapshot at call time and can change after further execution.
Calls carrying non-zero transferred ETH revert.

#### Contract Interface

```solidity
interface IMegaLimitControl {
    error NotIntercepted();
    function remainingComputeGas() external view returns (uint64 remaining);
}
```

The contract body reverts with `NotIntercepted()` if called outside the MegaETH EVM.

#### Interception Scope

Interception matches CALL and STATICCALL to `0x6342...0005`.
DELEGATECALL and CALLCODE are not intercepted for the same reason as `MegaAccessControl`.

### 4. Oracle `sendHint` Value Guard

Rex4 adds a value guard for intercepted `Oracle.sendHint(bytes32,bytes)` calls.
If the call carries non-zero transferred ETH, the call is rejected before `on_hint` side-effects.
Pre-Rex4 specs keep the old behavior.

## Inheritance

Rex4 inherits all Rex3 behavior (including increased oracle access compute gas limit, SLOAD-based oracle gas detention, and keyless deploy compute gas tracking) and all features from Rex2, Rex1, Rex, and MiniRex.

The semantics of Rex4 are inherited from:

- **Rex4** -> **Rex3** -> **Rex2** -> **Rex1** -> **Rex** -> **MiniRex** -> **Optimism Isthmus** -> **Ethereum Prague**

## Implementation References

- Frame state growth limit constants: `crates/mega-evm/src/constants.rs` (`rex4::FRAME_LIMIT_NUMERATOR`, `rex4::FRAME_LIMIT_DENOMINATOR`).
- Frame limit tracker: `crates/mega-evm/src/limit/frame_limit.rs` (`FrameLimitTracker`, `max_forward_limit()`).
- State growth tracker (Rex4 per-frame logic): `crates/mega-evm/src/limit/state_growth.rs` (`StateGrowthLimit2`).
- Absorb logic: `crates/mega-evm/src/limit/limit.rs` (`AdditionalLimit::before_frame_return_result`).
- Revert data encoding: `crates/mega-evm/src/limit/mod.rs` (`MegaLimitExceeded`, `LimitCheck::revert_data()`).
- TX runtime limits: `crates/mega-evm/src/evm/limit.rs` (`rex4()`).
- MegaAccessControl system contract: `crates/mega-evm/src/system/control.rs`, `crates/system-contracts/contracts/MegaAccessControl.sol`.
- MegaLimitControl system contract: `crates/mega-evm/src/system/limit_control.rs`, `crates/system-contracts/contracts/MegaLimitControl.sol`.
- Volatile access disable tracking: merged into `VolatileDataAccessTracker` in `crates/mega-evm/src/access/tracker.rs`.
- Frame init interception: `crates/mega-evm/src/evm/execution.rs` (`frame_init` method).
- Frame return deactivation: `crates/mega-evm/src/evm/execution.rs` (`frame_return_result` method).
- Instruction enforcement: `crates/mega-evm/src/evm/instructions.rs` (`wrap_op_detain_gas_unconditional!`, `wrap_op_detain_gas_conditional!`, and `wrap_call_volatile_check!` macros).

## References

- [Rex3 Specification](Rex3.md)
- [Rex2 Specification](Rex2.md)
- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
