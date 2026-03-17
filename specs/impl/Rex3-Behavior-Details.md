# Rex3 Behavior Details

This document is informative.
Normative semantics are defined in [Rex3 Specification](../Rex3.md).
If this document conflicts with the normative spec text, the normative spec wins.

## 1. Oracle access compute gas limit increase

The oracle access compute gas cap is increased from 1M to 20M, matching the block environment access cap.
This means that after Rex3, accessing oracle data and accessing block environment fields impose the same compute gas restriction.

The cap is still an absolute cap in Rex3 (applied to total transaction compute gas usage at the point of access).
If a transaction has already consumed more than 20M compute gas before accessing the oracle, execution will halt immediately.
Rex4 changes this to a relative cap — see [Rex4 Specification](../Rex4.md).

## 2. Oracle gas detention triggers on SLOAD

### SLOAD-based detection is caller-agnostic

The detention trigger fires on any SLOAD that reads from the oracle contract's storage, regardless of which contract in the call chain initiated the read.
For example, if Contract A calls Contract B, which then calls the oracle contract, the oracle's SLOAD triggers detention for the entire transaction.

### DELEGATECALL exception

DELEGATECALL executes the callee's code in the caller's context.
When Contract A DELEGATECALLs the oracle contract, SLOAD instructions read Contract A's storage, not the oracle contract's storage.
Therefore, DELEGATECALL to the oracle does not trigger detention — the address context check (whether the SLOAD target is the oracle address) fails.

### MEGA_SYSTEM_ADDRESS exemption

The system address exemption allows the sequencer to update oracle storage without triggering gas detention.

In pre-Rex3 (CALL-based path), the exemption checked `call_inputs.caller` — the frame-level caller.
In Rex3 (SLOAD-based path), the exemption checks `TxEnv.caller` — the transaction sender.
This means the entire transaction from `MEGA_SYSTEM_ADDRESS` is exempted, regardless of call depth or intermediate contract calls.

## 3. Keyless deploy compute gas tracking

In Rex2, the 100K overhead gas for keyless deploy is deducted from the frame's gas budget but not recorded in the compute gas tracker.
This means the overhead does not count toward the 200M per-transaction compute gas limit.

Rex3 fixes this by calling `record_compute_gas(100_000)` when the overhead is deducted.
If this causes the compute gas limit to be exceeded, the keyless deploy call halts.

## References

- [Rex3 Specification](../Rex3.md)
- [Rex3 Implementation References](Rex3-Implementation-References.md)
