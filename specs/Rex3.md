# Rex3 Specification

Rex3 is the third patch to the Rex hardfork.
It increases the oracle access compute gas limit from 1M to 10M, giving oracle-accessing transactions more room for post-oracle computation while inheriting all Rex2 behavior.

## Changes from Rex2

### 1. Oracle Access Compute Gas Limit Increase

Rex3 increases the compute gas cap applied after oracle contract access:

- **Previous limit (MINI_REX through REX2):** 1,000,000 (1M) compute gas
- **New limit (REX3):** 10,000,000 (10M) compute gas

The block environment access compute gas limit remains unchanged at 20M.
When both block environment and oracle are accessed, the most restrictive cap still wins (10M from oracle, since 10M < 20M).

This change allows transactions that read oracle data to perform more computation after the oracle access, reducing the frequency of `VolatileDataAccessOutOfGas` halts for legitimate use cases.

## Inheritance

Rex3 inherits all Rex2 behavior (including SELFDESTRUCT with EIP-6780 semantics, KeylessDeploy system contract, compute gas limit reset between transactions) and all features from Rex1, Rex, and MiniRex.

The semantics of Rex3 are inherited from:

- **Rex3** -> **Rex2** -> **Rex1** -> **Rex** -> **MiniRex** -> **Optimism Isthmus** -> **Ethereum Prague**

## Implementation References

- Oracle access compute gas limit constant: `crates/mega-evm/src/constants.rs` (`rex3::ORACLE_ACCESS_REMAINING_COMPUTE_GAS`).
- Transaction runtime limits: `crates/mega-evm/src/evm/limit.rs` (`EvmTxRuntimeLimits::rex3()`).
- Gas detention mechanism: `crates/mega-evm/src/evm/instructions.rs` (`wrap_op_detain_gas!`), `crates/mega-evm/src/access/tracker.rs` (`VolatileDataAccessTracker`).

## References

- [Rex2 Specification](Rex2.md)
- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
