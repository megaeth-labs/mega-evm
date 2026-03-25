# Rex Behavior Details

This document is informative.
Normative semantics are defined in [Rex Specification](../Rex.md).
If this document conflicts with the normative spec text, the normative spec wins.

## 1. Transaction intrinsic storage gas

The 39,000 storage gas is a flat cost applied unconditionally to every transaction, regardless of what the transaction does.
It is added to the initial gas calculation alongside the standard 21,000 compute intrinsic gas.
The storage gas component is not dynamically scaled by SALT bucket capacity — it is always 39,000.

## 2. Storage gas economics

The `multiplier` is derived from the SALT bucket capacity for the target account or storage slot: `multiplier = bucket_capacity / MIN_BUCKET_SIZE`.
At minimum bucket size, `multiplier = 1` and all dynamic storage gas costs are zero.

SSTORE storage gas applies only to zero-to-non-zero transitions (`0 == original_value == current_value != new_value`).
Writes that update an existing non-zero value or clear a value to zero do not incur dynamic storage gas.

Contract creation (via CREATE, CREATE2, or contract creation transaction) pays only the contract creation base (`32,000 × (multiplier - 1)`).
Contract creation storage gas subsumes account creation cost; account creation storage gas does not apply to contract creation.

Operations whose storage gas is unchanged from MiniRex:

| Operation | Storage gas |
| --- | --- |
| Code deposit | 10,000/byte |
| LOG topic | 3,750/topic |
| LOG data | 80/byte |
| Calldata (zero byte) | 40/byte |
| Calldata (non-zero byte) | 160/byte |
| Calldata floor (zero byte) | 100/byte |
| Calldata floor (non-zero byte) | 400/byte |

### Examples

**Zero storage gas at minimum bucket size.**
A contract writes a new storage slot in an uncrowded state region (bucket capacity = MIN_BUCKET_SIZE, so multiplier = 1).
Under MiniRex, this SSTORE would cost 2,000,000 storage gas.
Under Rex, it costs `20,000 × (1 - 1)` = 0 storage gas.
This makes storage writes in uncrowded regions effectively free from the storage gas perspective, while crowded regions still pay proportionally.

## 3. Consistent behavior among CALL-like opcodes

CALLCODE and DELEGATECALL are excluded from oracle access detection because they execute the callee's code in the caller's context.
The `target_address` in these opcodes is the caller's own address, not the callee's, so targeting the oracle contract address via DELEGATECALL or CALLCODE does not constitute a direct read of the oracle contract's state.

STATICCALL, however, does execute in the callee's context, so a STATICCALL to the oracle contract address is semantically equivalent to a CALL for the purpose of oracle access detection.

## 4. Transaction and block limits

State growth counts only **net new** entries after transaction execution:
- New storage slots: SSTORE transitions from zero to non-zero.
- New accounts: accounts created via CREATE, CREATE2, or CALL with value transfer to empty account (per EIP-161).

KV updates count **all** storage writes, including updates to existing non-zero slots.
This distinction is important: a transaction that updates 1000 existing slots counts 1000 KV updates but zero state growth.

Block-level state growth enforcement uses a "last included" policy: if a transaction causes the block's cumulative state growth to exceed the block limit, that transaction is still included, but no subsequent transactions may be added to the block.

The max total gas limit (storage + compute gas) for a single transaction or a whole block is not limited by the EVM spec; it is a chain-configurable parameter.

## References

- [Rex Specification](../Rex.md)
- [Rex Implementation References](Rex-Implementation-References.md)
