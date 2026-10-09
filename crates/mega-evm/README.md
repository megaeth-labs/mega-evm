# mega-evm

A specialized Ethereum Virtual Machine (EVM) implementation tailored for MegaETH, built on top of [revm](https://github.com/bluealloy/revm) and [op-revm](https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-revm).

## EVM Version

- **Base EVM**: [revm v40.0.3](https://github.com/bluealloy/revm)
- **Optimism EVM**: [op-revm v20.0.0](https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-revm), from the Optimism monorepo
- **Alloy EVM**: [alloy-evm v0.36.0](https://github.com/alloy-rs/alloy-evm)

The exact pins, including the Optimism monorepo revision, are in the workspace `Cargo.toml`.

## Terminology: Spec vs Hardfork

This codebase distinguishes between two related concepts:

- **Spec (`MegaSpecId`)**: Defines EVM behavior - what the EVM does. Values: `EQUIVALENCE`, `MINI_REX`, `MINI_REX_1`, `MINI_REX_2`, `REX`, `REX1`, `REX2`, `REX3`, `REX4`, `REX5`, `REX6`
- **Hardfork (`MegaHardfork`)**: Defines network upgrade events - when specs are activated. Values: `MiniRex`, `MiniRex1`, `MiniRex2`, `Rex`, `Rex1`, `Rex2`, `Rex3`, `Rex4`, `Rex5`, `Rex6`

The mapping between hardforks and specs is one-to-one: every hardfork schedules a spec rung of its own.
Rollbacks are expressed by alias specs whose behavior projects to an earlier spec: `MiniRex1` schedules `MINI_REX_1` (behavior: `EQUIVALENCE`) and `MiniRex2` schedules `MINI_REX_2` (behavior: `MINI_REX`).

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

For complete MiniRex specification, see the [MiniRex upgrade page](https://docs.megaeth.com/spec/upgrades/minirex).

### REX Spec

- **Refined Storage Gas Economics**: Optimized storage gas formulas with gradual scaling (20K-32K base costs vs. MiniRex's 2M)
- **Transaction Intrinsic Storage Gas**: 39,000 storage gas baseline for all transactions (total 60K with compute gas)
- **Zero Cost Fresh Storage**: Storage operations in minimum-sized SALT buckets charge 0 storage gas
- **Separate Contract Creation Cost**: Distinct storage gas for contract creation (32K base) vs. account creation (25K base)
- **Critical Security Fixes**: DELEGATECALL, STATICCALL, and CALLCODE now properly enforce 98/100 gas forwarding and oracle access detection
- **MiniRex Foundation**: Inherits all MiniRex features including multidimensional gas model, compute gas detention, and enhanced security

For complete Rex specification, see the [Rex upgrade page](https://docs.megaeth.com/spec/upgrades/rex).

### REX1 Spec

- **Limit Reset Fix**: Resets compute gas limits at the start of each transaction
- **No Other Behavioral Changes**: Inherits Rex semantics fully

For complete Rex1 specification, see the [Rex1 upgrade page](https://docs.megaeth.com/spec/upgrades/rex1).

### REX2 Spec

- **SELFDESTRUCT Restored**: Re-enabled with EIP-6780 semantics
- **KeylessDeploy System Contract**: Enables keyless deployment (Nick's Method) with custom gas limits
- **Rex1 Baseline**: Inherits Rex1 behavior for all other features

For complete Rex2 specification, see the [Rex2 upgrade page](https://docs.megaeth.com/spec/upgrades/rex2).

### REX3 Spec

- **Increased Oracle Access Gas Limit**: Oracle access compute gas limit raised from 1M to 20M, allowing more post-oracle computation
- **SLOAD-based Oracle Detention**: Oracle gas detention triggers on SLOAD from oracle storage instead of CALL to oracle contract
- **Keyless Deploy Compute Gas Tracking**: Records the 100K keyless deploy overhead as compute gas
- **Rex2 Baseline**: Inherits all Rex2 behavior

For complete Rex3 specification, see the [Rex3 upgrade page](https://docs.megaeth.com/spec/upgrades/rex3).

### REX4 Spec

- **Per-Call-Frame Resource Budgets**: All four resource dimensions (compute gas, data size, KV updates, state growth) are bounded per call frame with 98/100 forwarding
- **Relative Gas Detention**: Effective detained limit is `current_usage + cap` instead of an absolute cap
- **Storage Gas Stipend**: Value-transferring CALL/CALLCODE receives an additional 23,000 gas for storage gas operations
- **MegaAccessControl System Contract**: Allows contracts to proactively disable volatile data access for a call subtree
- **MegaLimitControl System Contract**: Allows querying effective remaining compute gas under detention and call frame limits
- **Rex3 Baseline**: Inherits all Rex3 behavior

For complete Rex4 specification, see the [Rex4 upgrade page](https://docs.megaeth.com/spec/upgrades/rex4).

### REX5 Spec

- **SequencerRegistry System Contract**: Tracks the system address and the sequencer as two independently rotatable roles, and Oracle v2.0.0 reads its authority from it
- **Dynamic System Address**: The system-transaction authority comes from the registry instead of a fixed constant
- **KeylessDeploy Hardening**: Rejects signed inner transactions with trailing bytes, and the sandbox's resource usage now counts toward the parent transaction
- **Resource-Accounting Corrections**: Caller-account update deduplication, CALLCODE new-account storage gas, failed-precompile compute gas, EIP-7702 authority state growth, and SELFDESTRUCT beneficiary creation
- **Boundary Hardening**: Precompile calls are bounded by the remaining compute gas, deposit-caller account creation is metered, system contract interceptors respect `CALL_STACK_LIMIT`, and oracle hints require gas and are metered
- **Rex4 Baseline**: Inherits all Rex4 behavior

For complete Rex5 specification, see the [Rex5 upgrade page](https://docs.megaeth.com/spec/upgrades/rex5).

### REX6 Spec

- **Unified Gas Metering Order**: Every storage-affecting opcode charges storage gas before its body and records compute gas once after it, with CREATE2 brought under the rule
- **Consolidated EIP-7702 Accounting**: Every per-authorization effect comes from one validation-time scan and is charged only for authorizations that apply
- **Resource-Accounting Corrections**: CREATE-frame nonce bump and net-new state growth, post-execution fee-reward writes, a per-log data-size base, forwarded gas returned on compute-gas halts, and value self-transfer deduplication
- **System Transaction Metering Exemption**: Protocol-originated transactions are exempt from SALT-scaled storage gas, the resource limits, and gas detention
- **Volatile-Access Coverage**: Beneficiary detention and `disableVolatileDataAccess` extend to source-side SELFDESTRUCT and EIP-7702-delegated CALLs, and oracle hints are not forwarded while volatile access is disabled
- **KeylessDeploy and CREATE Hardening**: Sandbox gas rescue on compute-gas halts, a self-destructing constructor reported as an empty-code deployment, and early halts for oversized CREATE2 initcode and static-frame CREATE
- **SequencerRegistry v2.0.0**: Sequencer rotation requires an EIP-712 possession proof and a minimum activation delay
- **Rex5 Baseline**: Inherits all Rex5 behavior

For complete Rex6 specification, see the [Rex6 upgrade page](https://docs.megaeth.com/spec/upgrades/rex6).

## Quick Start

```rust
use mega_evm::{
    alloy_evm,
    alloy_primitives::{address, Bytes, U256},
    op_revm::OpTransaction,
    revm::{
        context::{ContextTr, TxEnv},
        database::{CacheDB, EmptyDB},
        primitives::TxKind,
    },
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};

// An empty in-memory database and the latest frozen spec.
let db = CacheDB::<EmptyDB>::default();
let mut context = MegaContext::new(db, MegaSpecId::REX6);
// Isthmus requires the L1 operator fee parameters to be set.
context.chain_mut().operator_fee_scalar = Some(U256::ZERO);
context.chain_mut().operator_fee_constant = Some(U256::ZERO);
let mut evm = MegaEvm::new(context);

let mut tx = MegaTransaction(OpTransaction::new(TxEnv {
    caller: address!("0x0000000000000000000000000000000000100000"),
    kind: TxKind::Call(address!("0x0000000000000000000000000000000000100001")),
    gas_limit: 1_000_000,
    ..Default::default()
}));
// The enveloped transaction feeds the L1 data fee; empty is enough here.
tx.enveloped_tx = Some(Bytes::new());

let result = alloy_evm::Evm::transact_raw(&mut evm, tx)?;
assert!(result.result.is_success());
```

The same code runs as an example: `cargo run -p mega-evm --example quick_start`.

## Documentation

- [Full specification](https://docs.megaeth.com/spec/)
- [Architecture](../../ARCH.md)
