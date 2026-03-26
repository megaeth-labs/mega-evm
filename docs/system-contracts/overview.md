# System Contracts

MegaETH pre-deploys **system contracts** at well-known addresses as part of the protocol.
They provide infrastructure services (oracle storage, keyless deployment, access control) that are available to all smart contracts on the network.
System contracts are deployed idempotently during block execution, gated by [hardfork](../hardfork-spec.md) activation.

## Registry

| Contract             | Address                                        | Since  | Purpose                               |
| -------------------- | ---------------------------------------------- | ------ | ------------------------------------- |
| [Oracle](oracle.md)  | `0x6342000000000000000000000000000000000001`    | [MiniRex](../hardfork-spec.md#mini_rex) | Off-chain data key-value storage      |
| [High-Precision Timestamp](../oracle-services/timestamp.md) | `0x6342000000000000000000000000000000000002` | [MiniRex](../hardfork-spec.md#mini_rex) | Sub-second block timestamp ([oracle service](../oracle-services/overview.md)) |
| [KeylessDeploy](keyless-deploy.md) | `0x6342000000000000000000000000000000000003`    | [Rex2](../hardfork-spec.md#rex2)   | Deterministic cross-chain deployment  |

<details>

<summary>Rex4 (unstable): New System Contracts</summary>

| Contract          | Address                                        | Purpose                               |
| ----------------- | ---------------------------------------------- | ------------------------------------- |
| MegaAccessControl | `0x6342000000000000000000000000000000000004`    | Disable [volatile data](../glossary.md#volatile-data) access detection |
| MegaLimitControl  | `0x6342000000000000000000000000000000000005`    | Query remaining [compute gas](../glossary.md#compute-gas) budget    |

See [Rex4 Network Upgrade](../upgrades/rex4.md) for details.

</details>

## Key Design Aspects

### MEGA_SYSTEM_ADDRESS

The `MEGA_SYSTEM_ADDRESS` (`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`) is a special account intended for sequencer-managed maintenance operations.
Whitelisted transactions from this address are processed as deposit-like transactions, bypassing signature validation and execution-fee charging.
This is how the sequencer updates oracle storage.

See [Mega System Transactions](system-tx.md) for details.

### Backward Compatibility

Any system contract modification requires a new spec.
This ensures that the behavior of existing specs is preserved across upgrades.

### Intercepted Methods

Some system contract methods are handled at the EVM level rather than by on-chain bytecode.
These methods are only available via direct CALL — DELEGATECALL and CALLCODE to system contract addresses are not intercepted and fall through to on-chain bytecode.

- [KeylessDeploy](keyless-deploy.md): `keylessDeploy` is intercepted at depth 0 only. Unknown selectors or non-top-level calls fall through to on-chain bytecode, which reverts with `NotIntercepted()`.
- [Oracle](oracle.md): `sendHint` is intercepted to forward hints to the sequencer. All other methods execute via deployed bytecode.
