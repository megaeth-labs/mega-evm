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
  Remaining = 1000 - 962 = 38
  Child C budget = 38 * 98/100 = 37
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

## References

- [Rex3 Specification](Rex3.md)
- [Rex2 Specification](Rex2.md)
- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
