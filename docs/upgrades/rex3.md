---
description: Rex3 increases the oracle gas detention cap to 20M, changes oracle detention to SLOAD-based triggering, and fixes keyless deploy compute gas tracking.
---

# Rex3 Network Upgrade

This page is an informative summary of the Rex3 specification.
For the full normative definition, see the Rex3 spec in the mega-evm repository.

## Summary

Rex3 addresses two issues with [oracle](../system-contracts/oracle.md) [gas detention](../evm/gas-detention.md) and fixes a gap in [keyless deploy](../system-contracts/keyless-deploy.md) resource accounting.

The oracle access [compute gas](../glossary.md#compute-gas) cap is increased from 1M to **20M**, matching the block environment cap.
The 1M cap was too restrictive for legitimate use cases, causing frequent `VolatileDataAccessOutOfGas` halts in contracts that need meaningful computation after reading oracle data.

Oracle detention is also changed from CALL-based to **SLOAD-based** triggering — simply calling the oracle contract without reading its storage no longer activates gas detention.
Finally, the keyless deploy sandbox overhead (100K gas) is now properly tracked as compute gas.

## What Changed

### Oracle Access Compute Gas Limit Increase

#### Previous behavior
- Oracle access (triggered by CALL or STATICCALL to the oracle address) caps compute gas at 1,000,000 (1M).

#### New behavior
- Oracle storage reads (SLOAD from the oracle contract) cap compute gas at 20,000,000 (20M).
- The block environment access cap remains at 20M (unchanged).
- When both block environment and oracle storage are accessed, neither is more restrictive than the other.

### Oracle Gas Detention Triggers on SLOAD

#### Previous behavior
- CALL or STATICCALL to the oracle contract address triggers gas detention.
- Any CALL or STATICCALL to the oracle triggers detention even without reading storage.

#### New behavior
- SLOAD from the oracle contract's storage triggers gas detention.
- CALL to the oracle contract address alone does not trigger gas detention — only an SLOAD from the oracle's storage triggers it.
- The SLOAD-based trigger is caller-agnostic: any SLOAD reading the oracle contract's storage triggers detention regardless of call depth.
- DELEGATECALL to the oracle does not trigger detention — SLOAD in a DELEGATECALL context reads the caller's storage, not the oracle's.
- Transactions from `MEGA_SYSTEM_ADDRESS` are exempted from oracle gas detention (unchanged, but the exemption now checks the transaction sender rather than the call-frame-level caller).

### Keyless Deploy Compute Gas Tracking

#### Previous behavior
- The 100K overhead gas for keyless deploy sandbox execution is deducted from call frame gas but not recorded as compute gas.
- Keyless deploy transactions do not count the overhead toward the per-transaction compute gas limit.

#### New behavior
- The 100K keyless deploy overhead is recorded as compute gas.
- If recording the overhead causes the compute gas limit to be exceeded, execution halts.

## Developer Impact

**Contracts that read oracle data can now perform more computation.**
The oracle access cap increased from 1M to 20M, giving you 20× more post-access compute budget.
If you previously hit `VolatileDataAccessOutOfGas` after oracle reads, this upgrade should resolve the issue.

**Simply calling the oracle no longer triggers detention.**
Only actual SLOAD reads from oracle storage activate the cap.
If your contract calls the oracle contract for hint operations or other non-storage-reading functionality, you will no longer be penalized.

**The gas detention cap is still absolute in Rex3.**
If your transaction has consumed more than 20M compute gas before an oracle SLOAD, execution will halt immediately.
Rex4 changes this to a relative cap — see the [Rex4 upgrade page](rex4.md).

## Safety and Compatibility

All pre-Rex3 behavior is unchanged.

The shift from CALL-based to SLOAD-based oracle detection more accurately captures actual oracle data access.
DELEGATECALL to the oracle is excluded because SLOAD in a DELEGATECALL context reads the caller's storage, not the oracle's — this is consistent with Rex2 behavior.

The [`MEGA_SYSTEM_ADDRESS`](../system-contracts/system-tx.md) exemption now checks `TxEnv.caller` (the transaction sender) rather than the call-frame-level caller, meaning the entire transaction from the system address is exempted regardless of call depth.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Gas Detention](../evm/gas-detention.md) — background on the gas detention mechanism
- [Oracle Service](../system-contracts/oracle.md) — oracle contract documentation
