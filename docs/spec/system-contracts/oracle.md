---
description: MegaETH Oracle system contract — address, storage layout, hint forwarding, and gas detention trigger.
spec: Rex5
---

# Oracle

This page specifies the Oracle system contract.
It defines the address, interface, restricted write behavior, storage access semantics, and hint forwarding.

## Motivation

MegaETH needs a canonical protocol-level storage backend for externally sourced data such as timestamps and other oracle-fed values.
That storage must be readable by contracts, writable by protocol-controlled maintenance transactions, and stable across specs.

## Specification

### Address

The Oracle system contract MUST exist at `ORACLE_CONTRACT_ADDRESS`.

### Bytecode

A node MUST deploy the bytecode version corresponding to the active spec.
Versions 1.0.0 and 1.1.0 take `MEGA_SYSTEM_ADDRESS` as a constructor `immutable`.
Version 2.0.0 reads the authorized address from `SequencerRegistry.currentSequencer()` via a `constant` reference.

#### Version 1.0.0

Since: [MiniRex](../upgrades/minirex.md)

Code hash: `0xe9b044afb735a0f569faeb248088b4f267578f60722f87d06ec3867b250a2c34`

Deployed bytecode:

```
0x608060405234801561000f575f5ffd5b506004361061006f575f3560e01c80637eba7ba61161004d5780637eba7ba614610118578063a21e2d6914610138578063fbc0d03514610158575f5ffd5b806301caec13146100735780630dc9b5da1461008857806354fd4d50146100d9575b5f5ffd5b610086610081366004610324565b61016b565b005b6100af7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d81565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b604080518082018252600581527f312e302e30000000000000000000000000000000000000000000000000000000602082015290516100d09190610390565b61012a6101263660046103e3565b5490565b6040519081526020016100d0565b61014b6101463660046103fa565b6101e6565b6040516100d09190610439565b61008661016636600461047b565b61025c565b8281146101b2576040517f5b7232fa000000000000000000000000000000000000000000000000000000008152600481018490526024810182905260440160405180910390fd5b8382845f5b818110156101d457602081028381013590850135556001016101b7565b505050506101e061026b565b50505050565b60608167ffffffffffffffff8111156102015761020161049b565b60405190808252806020026020018201604052801561022a578160200160208202803683370190505b5090506020810183835f5b818110156102525760208102838101355490850152600101610235565b5050505092915050565b80825561026761026b565b5050565b3373ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d16146102da576040517f5e742c5a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b565b5f5f83601f8401126102ec575f5ffd5b50813567ffffffffffffffff811115610303575f5ffd5b6020830191508360208260051b850101111561031d575f5ffd5b9250929050565b5f5f5f5f60408587031215610337575f5ffd5b843567ffffffffffffffff81111561034d575f5ffd5b610359878288016102dc565b909550935050602085013567ffffffffffffffff811115610378575f5ffd5b610384878288016102dc565b95989497509550505050565b602081525f82518060208401528060208501604085015e5f6040828501015260407fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011684010191505092915050565b5f602082840312156103f3575f5ffd5b5035919050565b5f5f6020838503121561040b575f5ffd5b823567ffffffffffffffff811115610421575f5ffd5b61042d858286016102dc565b90969095509350505050565b602080825282518282018190525f918401906040840190835b81811015610470578351835260209384019390920191600101610452565b509095945050505050565b5f5f6040838503121561048c575f5ffd5b50508035926020909101359150565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffdfea26469706673582212205bb66f27c8ccdec3b5bbd6071d5f516754488531634a3ad38e6c7ffacf47a02464736f6c634300081e0033
```

#### Version 1.1.0

Since: [Rex2](../upgrades/rex2.md)

Code hash: `0x06df675a69e53ea2a3c948521e330b3801740fede324a1cef2044418f8e09242`

Deployed bytecode:

```
0x608060405234801561000f575f5ffd5b50600436106100b9575f3560e01c806366cdf82f116100725780638d4909dc116100585780638d4909dc146101c8578063a21e2d69146101db578063fbc0d035146101fb575f5ffd5b806366cdf82f146101955780637eba7ba6146101a8575f5ffd5b8063138f5ec5116100a2578063138f5ec514610123578063348a0cdc1461013657806354fd4d5014610156575f5ffd5b806301caec13146100bd5780630dc9b5da146100d2575b5f5ffd5b6100d06100cb3660046105db565b61020e565b005b6100f97f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d81565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020015b60405180910390f35b6100d0610131366004610647565b61028a565b61014961014436600461068f565b6102d4565b60405161011a919061071a565b604080518082018252600581527f312e312e300000000000000000000000000000000000000000000000000000006020820152905161011a919061079b565b6100d06101a33660046107b4565b505050565b6101ba6101b636600461082b565b5490565b60405190815260200161011a565b6100d06101d63660046107b4565b61044b565b6101ee6101e936600461068f565b61045e565b60405161011a9190610842565b6100d0610209366004610884565b6104d4565b828114610256576040517f5b7232fa00000000000000000000000000000000000000000000000000000000815260048101849052602481018290526044015b60405180910390fd5b8382845f5b81811015610278576020810283810135908501355560010161025b565b505050506102846104e3565b50505050565b805f5b818110156102ca576102c2858585848181106102ab576102ab6108a4565b90506020028101906102bd91906108d1565b610554565b60010161028d565b50506101a36104e3565b60608167ffffffffffffffff8111156102ef576102ef610932565b60405190808252806020026020018201604052801561032257816020015b606081526020019060019003908161030d5790505b5090505f5b82811015610444575f8030868685818110610344576103446108a4565b905060200281019061035691906108d1565b60405161036492919061095f565b5f60405180830381855af49150503d805f811461039c576040519150601f19603f3d011682016040523d82523d5f602084013e6103a1565b606091505b50915091508161041c578051156103ba57805181602001fd5b6040517f08c379a000000000000000000000000000000000000000000000000000000000815260206004820152601660248201527f4d756c746963616c6c3a2063616c6c206661696c656400000000000000000000604482015260640161024d565b8084848151811061042f5761042f6108a4565b60209081029190910101525050600101610327565b5092915050565b610456838383610554565b6101a36104e3565b60608167ffffffffffffffff81111561047957610479610932565b6040519080825280602002602001820160405280156104a2578160200160208202803683370190505b5090506020810183835f5b818110156104ca57602081028381013554908501526001016104ad565b5050505092915050565b8082556104df6104e3565b5050565b3373ffffffffffffffffffffffffffffffffffffffff7f000000000000000000000000a887dcb9d5f39ef79272801d05abdf707cfbbd1d1614610552576040517f5e742c5a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b565b827fda678492695e6a825d786c2375f7fdf3c1dc012451c61c1804227f499b0fc53e838360405161058692919061096e565b60405180910390a2505050565b5f5f83601f8401126105a3575f5ffd5b50813567ffffffffffffffff8111156105ba575f5ffd5b6020830191508360208260051b85010111156105d4575f5ffd5b9250929050565b5f5f5f5f604085870312156105ee575f5ffd5b843567ffffffffffffffff811115610604575f5ffd5b61061087828801610593565b909550935050602085013567ffffffffffffffff81111561062f575f5ffd5b61063b87828801610593565b95989497509550505050565b5f5f5f60408486031215610659575f5ffd5b83359250602084013567ffffffffffffffff811115610676575f5ffd5b61068286828701610593565b9497909650939450505050565b5f5f602083850312156106a0575f5ffd5b823567ffffffffffffffff8111156106b6575f5ffd5b6106c285828601610593565b90969095509350505050565b5f81518084528060208401602086015e5f6020828601015260207fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe0601f83011685010191505092915050565b5f602082016020835280845180835260408501915060408160051b8601019250602086015f5b8281101561078f577fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffc087860301845261077a8583516106ce565b94506020938401939190910190600101610740565b50929695505050505050565b602081525f6107ad60208301846106ce565b9392505050565b5f5f5f604084860312156107c6575f5ffd5b83359250602084013567ffffffffffffffff8111156107e3575f5ffd5b8401601f810186136107f3575f5ffd5b803567ffffffffffffffff811115610809575f5ffd5b86602082840101111561081a575f5ffd5b939660209190910195509293505050565b5f6020828403121561083b575f5ffd5b5035919050565b602080825282518282018190525f918401906040840190835b8181101561087957835183526020938401939092019160010161085b565b509095945050505050565b5f5f60408385031215610895575f5ffd5b50508035926020909101359150565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b5f5f83357fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe1843603018112610904575f5ffd5b83018035915067ffffffffffffffff82111561091e575f5ffd5b6020019150368190038213156105d4575f5ffd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffd5b818382375f9101908152919050565b60208152816020820152818360408301375f818301604090810191909152601f9092017fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe016010191905056fea164736f6c634300081e000a
```

#### Version 2.0.0

Since: [Rex5](../upgrades/rex5.md)

The `onlySystemAddress` modifier reads the authorized address from [`SequencerRegistry.currentSequencer()`](sequencer-registry.md) instead of a constructor `immutable`.
This enables sequencer rotation without redeploying the Oracle contract.
All other functionality is preserved from v1.1.0.

Code hash: `0xcdc4cffc96a152777dd4952a5446bc5402fcfeec16a6b4b458ddee2e0b7af525`

### Public Read Interface

The Oracle contract MUST expose the following externally callable read methods:

```solidity
interface IOracle {
    function getSlot(uint256 slot) external view returns (bytes32 value);
    function getSlots(uint256[] calldata slots) external view returns (bytes32[] memory values);
}
```

`getSlot` MUST return the storage value at the specified slot.
`getSlots` MUST return the storage values at the specified slots in the same order as the input array.

### Restricted Write Interface

The Oracle contract MUST expose the following write and log-emission methods:

```solidity
interface IOracle {
    function setSlot(uint256 slot, bytes32 value) external;
    function setSlots(uint256[] calldata slots, bytes32[] calldata values) external;
    function emitLog(bytes32 topic, bytes calldata data) external;
    function emitLogs(bytes32 topic, bytes[] calldata dataVector) external;
}
```

The methods above MUST be callable only by `SYSTEM_ADDRESS`.
Calls from any other sender MUST revert with `NotSystemAddress()`.

For `setSlots`, if the `slots` and `values` array lengths differ, the call MUST revert with `InvalidLength(uint256 slotsLength, uint256 valuesLength)`.

### Auxiliary Interface

The Oracle contract MUST expose the following auxiliary methods:

```solidity
interface IOracle {
    function multiCall(bytes[] calldata data) external returns (bytes[] memory results);
    function sendHint(bytes32 topic, bytes calldata data) external view;
}
```

`multiCall` MUST execute each payload by `DELEGATECALL` into the Oracle contract and MUST return the results in order.
If any delegated call fails, `multiCall` MUST revert and MUST bubble up the revert data if present.

`sendHint` MUST be externally callable and MUST be a no-op at the Solidity bytecode level.

### Storage Access Semantics

**Reads.**
`getSlot` and `getSlots` read Oracle storage via `SLOAD`.
The node MAY serve Oracle reads from an external data source that provides realtime, per-transaction values.
When an `SLOAD` targets `ORACLE_CONTRACT_ADDRESS`, the node MUST first consult the external data source.
If it provides a value for the requested slot, that value MUST be returned.
Otherwise, the node MUST return the on-chain storage value.

**Writes.**
`setSlot` and `setSlots` write Oracle storage via `SSTORE`.
These methods are restricted to `SYSTEM_ADDRESS` (see [Restricted Write Interface](#restricted-write-interface)).

**On-chain persistence.**
When the external data source provides a value for a read, the sequencer MUST persist that value on-chain by inserting a [Mega System Transaction](system-tx.md) that calls `setSlot` or `setSlots`.
This system transaction MUST be ordered before the user transaction that triggered the read, so that full nodes replaying the block observe the same storage state.

### Hint Forwarding

`sendHint` is the only function in Oracle system contract that participates in [call interception](interception.md).
All other Oracle functions (`getSlot`, `getSlots`, `setSlot`, `setSlots`, `emitLog`, `emitLogs`, `multiCall`) execute via ordinary contract bytecode only.

When a `CALL` or `STATICCALL` targets `ORACLE_CONTRACT_ADDRESS` and the input matches the `sendHint(bytes32,bytes)` selector, the node MUST forward the decoded `topic` and `data` to the external oracle backend as a side effect.
The call MUST then fall through — the Oracle contract's deployed `sendHint` function body executes as ordinary bytecode.

Because the Solidity implementation of `sendHint` is a no-op `view` function, the net observable behavior is the combination of:

- hint forwarding to the oracle backend (side effect), and
- normal bytecode execution of the no-op function body (which returns successfully with no output).

Calls to `ORACLE_CONTRACT_ADDRESS` that do not match the `sendHint` selector MUST fall through without any side effect.

If a transaction calls `sendHint` and subsequently reads an Oracle slot, the hint MUST be delivered to the oracle backend before the read is served.

### Gas and Detention Semantics

The following gas and detention rules MUST apply:

- `SLOAD` against Oracle storage MUST use the cold access gas cost.
- Oracle storage reads MUST participate in [gas detention](../evm/gas-detention.md).
- `CALL` or `STATICCALL` to the Oracle contract address alone MUST NOT trigger oracle detention unless Oracle storage is actually read.
- `DELEGATECALL` to the Oracle contract MUST NOT trigger oracle detention solely by targeting the Oracle address.

### Versioning

Pre-[Rex2](../upgrades/rex2.md), the deployed Oracle bytecode does not include `sendHint`.
From [Rex2](../upgrades/rex2.md) onward, the stable Oracle bytecode includes `sendHint`.

## Constants

| Constant                  | Value                                        | Description                           |
| ------------------------- | -------------------------------------------- | ------------------------------------- |
| `ORACLE_CONTRACT_ADDRESS` | `0x6342000000000000000000000000000000000001` | Stable Oracle system-contract address |

## Rationale

**Why centralize oracle-backed data in one contract?**
Oracle-backed protocol data needs a single canonical storage location so all contracts and all nodes observe the same values under the same addressing scheme.

**Why restrict writes to `SYSTEM_ADDRESS`?**
Externally sourced oracle values are part of protocol-maintained state.
Allowing arbitrary writes would destroy the meaning of oracle-backed data and make the values untrustworthy as protocol inputs.

**Why use a per-transaction external data source instead of pre-populating all oracle data?**
Traditional oracle designs require all data to be written on-chain before any transaction can read it, even if most transactions never access oracle data.
The external data source enables a realtime lazy oracle: values are only fetched and persisted when a transaction actually reads them.
This avoids unnecessary system transactions for data that no one consumes, reduces block overhead, and allows oracle data to be as fresh as the moment of access rather than the moment of block construction.
The sequencer's frontrunning system transaction ensures that the lazily served value is still persisted on-chain for full nodes and verifiers that replay the block.

**Why intercept `sendHint` during call interception?**
Hint forwarding depends on external backend behavior that cannot be expressed by on-chain bytecode alone.
The no-op Solidity body provides a stable interface, while the [call interception](interception.md) mechanism supplies the protocol-level side effect.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced the Oracle contract.
- [Rex2](../upgrades/rex2.md) added the `sendHint` entry point to the deployed Oracle bytecode.
- [Rex3](../upgrades/rex3.md) changed oracle detention to SLOAD-based triggering and raised the oracle detention cap to 20M.
- [Rex5](../upgrades/rex5.md) replaced the constructor `immutable` authority with a dynamic read from `SequencerRegistry.currentSequencer()` (Oracle v2.0.0).
