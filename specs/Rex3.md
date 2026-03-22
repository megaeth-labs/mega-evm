# Rex3 Specification

## Abstract

Rex3 is the third patch to the Rex hardfork.
It increases the oracle access compute gas cap, changes oracle gas detention from CALL-based to SLOAD-based triggering, and fixes keyless deploy compute gas tracking.
All Rex2 semantics are preserved unless explicitly changed below.

## Changes

### 1. Oracle access compute gas limit increase

#### Motivation

The 1M compute gas cap after oracle access is too restrictive for legitimate use cases, causing frequent `VolatileDataAccessOutOfGas` halts in contracts that need to perform meaningful computation after reading oracle data.

#### Semantics

Previous behavior:
- Oracle access (triggered by CALL or STATICCALL to the oracle address) caps compute gas at 1,000,000 (1M).

New behavior:
- Oracle storage reads (SLOAD from the oracle contract) MUST cap compute gas at 20,000,000 (20M).
- The block environment access cap remains at 20M (unchanged).
- When both block environment and oracle storage are read, both caps are equal (20M), so neither is more restrictive than the other.

### 2. Oracle gas detention triggers on SLOAD

#### Motivation

CALL-based oracle detection triggers detention when simply calling the oracle contract, even without reading any storage.
SLOAD-based detection more accurately captures actual oracle data access.

#### Semantics

Previous behavior:
- CALL or STATICCALL to the oracle contract address triggers gas detention.

New behavior:
- SLOAD from the oracle contract's storage MUST trigger gas detention.
- CALL to the oracle contract address alone MUST NOT trigger gas detention; however, if the CALL results in an SLOAD from the oracle contract's storage, that SLOAD triggers detention.
- The SLOAD-based trigger MUST be caller-agnostic: any SLOAD reading the oracle contract's storage triggers detention regardless of call depth.
- DELEGATECALL to the oracle contract MUST NOT trigger detention, because SLOAD in a DELEGATECALL context reads the caller's storage, not the oracle contract's storage.
- Transactions from `MEGA_SYSTEM_ADDRESS` MUST be exempted from oracle gas detention.
- The exemption MUST check the transaction sender (`TxEnv.caller`), not the frame-level caller.

### 3. Keyless deploy compute gas tracking

#### Motivation

The 100K overhead gas for keyless deploy sandbox execution is deducted from frame gas but not recorded as compute gas, meaning keyless deploy transactions do not count toward the per-transaction compute gas limit.

#### Semantics

Previous behavior:
- Keyless deploy overhead gas (100K) is not counted toward the compute gas limit.

New behavior:
- The 100K keyless deploy overhead MUST be recorded as compute gas.
- If recording the overhead causes the compute gas limit to be exceeded, execution MUST halt.

## Invariants

- `I-1`: Stable Rex2 semantics MUST remain unchanged.
- `I-2`: DELEGATECALL to the oracle contract MUST NOT trigger gas detention.
- `I-3`: `MEGA_SYSTEM_ADDRESS` transactions MUST be exempted from oracle gas detention regardless of call depth.

## Inheritance

Rex3 inherits Rex2 except for the deltas defined in `Changes`.
Semantic lineage: `Rex3 -> Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague`.

## References

- [Rex2 Specification](Rex2.md)
- [Rex4 Specification](Rex4.md)
- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
- [Rex3 Behavior Details (Informative)](impl/Rex3-Behavior-Details.md)
- [Rex3 Implementation References (Informative)](impl/Rex3-Implementation-References.md)
- [Oracle Service](../docs/ORACLE_SERVICE.md)
- [Keyless Deployment](../docs/KEYLESS_DEPLOYMENT.md)
