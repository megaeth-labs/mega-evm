# mega-evm

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/bluealloy/op-revm).

## EVM Version

- **Base EVM**: [revm v27.1.0 (v83)](https://github.com/bluealloy/revm)
- **Optimism EVM**: [op-revm v8.1.0 (v83)](https://github.com/bluealloy/op-revm)
- **Alloy EVM**: [alloy-evm v0.15.0](https://github.com/alloy-rs/alloy-evm)

## Terminology: Spec vs Hardfork

This codebase distinguishes between two related concepts:

- **Spec (`MegaSpecId`)**: Defines EVM behavior - what the EVM does. Values: `EQUIVALENCE`, `MINI_REX`, `REX`, `REX1`, `REX2`, `REX3`, `REX4`
- **Hardfork (`MegaHardfork`)**: Defines network upgrade events - when specs are activated. Values: `MiniRex`, `MiniRex1`, `MiniRex2`, `Rex`, `Rex1`, `Rex2`, `Rex3`, `Rex4`

Multiple hardforks can map to the same spec.
For example, both `MiniRex` and `MiniRex2` hardforks use the `MINI_REX` spec.

## Key Features

### EQUIVALENCE Spec

- **Optimism Compatibility**: Maintains full compatibility with Optimism Isthmus EVM
- **Parallel Execution Support**: Block environment access tracking for conflict detection

### MINI_REX Spec

- **Multidimensional Gas Model**: Independent tracking for compute gas (1B), data size (3.125 MB), and KV updates (125K)
- **Compute Gas Tracking**: Separate limit for computational work with gas detention for volatile data access
- **Dynamic Gas Costs**: SALT bucket-based scaling preventing state bloat
- **Split LOG Costs**: Compute gas (standard) + storage gas (10x multiplier) for independent resource pricing
- **SELFDESTRUCT Prohibition**: Complete disabling for contract integrity
- **Large Contract Support**: 512 KB contracts (21x increase from 24 KB)
- **Gas Detention**: Volatile data access (block env, beneficiary, oracle) triggers gas limiting with refunds
- **Enhanced Security**: Comprehensive limit enforcement preserving remaining gas on limit violations

For complete MiniRex specification, see the [MiniRex upgrade page](https://megaeth-labs.github.io/mega-evm/upgrades/minirex.html).

### REX Spec

- **Refined Storage Gas Economics**: Optimized storage gas formulas with gradual scaling (20K-32K base costs vs. MiniRex's 2M)
- **Transaction Intrinsic Storage Gas**: 39,000 storage gas baseline for all transactions (total 60K with compute gas)
- **Zero Cost Fresh Storage**: Storage operations in minimum-sized SALT buckets charge 0 storage gas
- **Separate Contract Creation Cost**: Distinct storage gas for contract creation (32K base) vs. account creation (25K base)
- **Critical Security Fixes**: DELEGATECALL, STATICCALL, and CALLCODE now properly enforce 98/100 gas forwarding and oracle access detection
- **MiniRex Foundation**: Inherits all MiniRex features including multidimensional gas model, compute gas detention, and enhanced security

For complete Rex specification, see the [Rex upgrade page](https://megaeth-labs.github.io/mega-evm/upgrades/rex.html).

### REX1 Spec

- **Limit Reset Fix**: Resets compute gas limits at the start of each transaction
- **No Other Behavioral Changes**: Inherits Rex semantics fully

For complete Rex1 specification, see the [Rex1 upgrade page](https://megaeth-labs.github.io/mega-evm/upgrades/rex1.html).

### REX2 Spec

- **SELFDESTRUCT Restored**: Re-enabled with EIP-6780 semantics
- **KeylessDeploy System Contract**: Enables keyless deployment (Nick's Method) with custom gas limits
- **Rex1 Baseline**: Inherits Rex1 behavior for all other features

For complete Rex2 specification, see the [Rex2 upgrade page](https://megaeth-labs.github.io/mega-evm/upgrades/rex2.html).

### REX3 Spec

- **Increased Oracle Access Gas Limit**: Oracle access compute gas limit raised from 1M to 20M, allowing more post-oracle computation
- **SLOAD-based Oracle Detention**: Oracle gas detention triggers on SLOAD from oracle storage instead of CALL to oracle contract
- **Keyless Deploy Compute Gas Tracking**: Records the 100K keyless deploy overhead as compute gas
- **Rex2 Baseline**: Inherits all Rex2 behavior

For complete Rex3 specification, see the [Rex3 upgrade page](https://megaeth-labs.github.io/mega-evm/upgrades/rex3.html).

### REX4 Spec

- **Per-Call-Frame Resource Budgets**: All four resource dimensions (compute gas, data size, KV updates, state growth) are bounded per call frame with 98/100 forwarding
- **Relative Gas Detention**: Effective detained limit is `current_usage + cap` instead of an absolute cap
- **Storage Gas Stipend**: Value-transferring CALL/CALLCODE receives an additional 23,000 gas for storage gas operations
- **MegaAccessControl System Contract**: Allows contracts to proactively disable volatile data access for a call subtree
- **MegaLimitControl System Contract**: Allows querying effective remaining compute gas under detention and call frame limits
- **Rex3 Baseline**: Inherits all Rex3 behavior

For complete Rex4 specification, see the [Rex4 upgrade page](https://megaeth-labs.github.io/mega-evm/upgrades/rex4.html).

## Quick Start

```rust
use mega_evm::{Context, Evm, SpecId, Transaction};
use revm::{
    context::TxEnv,
    database::{CacheDB, EmptyDB},
    inspector::NoOpInspector,
    primitives::TxKind,
};

// Create EVM instance with MINI_REX spec
let mut db = CacheDB::<EmptyDB>::default();
let spec = SpecId::MINI_REX;
let mut context = Context::new(db, spec);
let mut evm = Evm::new(context, NoOpInspector);

// Execute transaction
let tx = Transaction {
    base: TxEnv {
        caller: address!("..."),
        kind: TxKind::Call(target_address),
        data: Bytes::default(),
        value: U256::ZERO,
        gas_limit: 1000000,
        ..Default::default()
    },
    ..Default::default()
};

let result = alloy_evm::Evm::transact_raw(&mut evm, tx)?;
```

## Documentation

- [Full specification](https://megaeth-labs.github.io/mega-evm/)
- [Architecture](../../ARCH.md)
