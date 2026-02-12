# Rex3 Specification

Rex3 is the third patch to the Rex hardfork.
It introduces several behavioral changes while inheriting all Rex2 behavior.

## Changes from Rex2

### 1. Oracle Access Compute Gas Limit Increase

Rex3 increases the compute gas cap applied after oracle contract access:

- **Previous limit (MINI_REX through REX2):** 1,000,000 (1M) compute gas
- **New limit (REX3):** 20,000,000 (20M) compute gas

The block environment access compute gas limit remains unchanged at 20M.
When both block environment and oracle are accessed, both caps are now equal (20M), so neither is more restrictive than the other.

This change allows transactions that read oracle data to perform more computation after the oracle access, reducing the frequency of `VolatileDataAccessOutOfGas` halts for legitimate use cases.

### 2. Oracle Gas Detention Triggers on SLOAD (not CALL)

In specs prior to Rex3 (MINI_REX through REX2), oracle gas detention is triggered when any CALL targets the oracle contract address.
This means simply calling the oracle contract — even without reading any storage — activates gas detention.

Rex3 changes this so that oracle gas detention is triggered by SLOAD from the oracle contract's storage, not by CALL to the oracle contract.
This more accurately captures actual oracle data access: only transactions that read oracle storage values trigger gas detention.

- **Previous behavior (MINI_REX through REX2):** CALL to oracle contract → triggers gas detention
- **New behavior (REX3):** SLOAD from oracle contract storage → triggers gas detention

### 3. Keyless Deploy Compute Gas Tracking

The 100K overhead gas for sandbox execution are deducted from frame gas but never recorded as the compute gas.
This means keyless deploy transactions don't count toward the 200M per-transaction compute gas limit.

Rex3 fixes this by recording the keyless deploy overhead gas (100K) as compute gas.
If this causes the compute gas limit to be exceeded, the execution will halt.

- **Previous behavior (REX2):** Keyless deploy overhead gas not counted toward compute gas limit
- **New behavior (REX3):** 100K overhead gas recorded as compute gas

## Inheritance

Rex3 inherits all Rex2 behavior (including SELFDESTRUCT with EIP-6780 semantics, KeylessDeploy system contract, compute gas limit reset between transactions) and all features from Rex1, Rex, and MiniRex.

The semantics of Rex3 are inherited from:

- **Rex3** -> **Rex2** -> **Rex1** -> **Rex** -> **MiniRex** -> **Optimism Isthmus** -> **Ethereum Prague**

## Implementation References

- Oracle access compute gas limit constant: `crates/mega-evm/src/constants.rs` (`rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS`).
- Oracle SLOAD gas detention: `crates/mega-evm/src/evm/host.rs` (`sload` method), `crates/mega-evm/src/evm/instructions.rs` (`rex3::instruction_table`, `volatile_data_ext::sload`).
- Keyless deploy compute gas: `crates/mega-evm/src/evm/execution.rs` (`frame_init`, keyless deploy section).
- Gas detention mechanism: `crates/mega-evm/src/evm/instructions.rs` (`wrap_op_detain_gas!`), `crates/mega-evm/src/access/tracker.rs` (`VolatileDataAccessTracker`).

## References

- [Rex2 Specification](Rex2.md)
- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
