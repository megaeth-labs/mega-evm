# Rex2 Behavior Details

This document is informative.
Normative semantics are defined in [Rex2 Specification](../Rex2.md).
If this document conflicts with the normative spec text, the normative spec wins.

## 1. SELFDESTRUCT re-enabled (EIP-6780)

SELFDESTRUCT was completely disabled in MiniRex through Rex1 (halts with `InvalidFEOpcode`).
Rex2 restores it using the standard post-Cancun EIP-6780 semantics already implemented in the underlying revm.

The "same transaction" check means the contract must have been created via CREATE or CREATE2 within the currently executing transaction.
Contracts deployed in prior transactions or in prior blocks are never eligible for full destruction.

## 2. KeylessDeploy system contract

The system contract address is `0x6342000000000000000000000000000000000003`.

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress, bytes memory errorData);
}
```

The `keylessDeploymentTransaction` parameter is the RLP-encoded pre-EIP-155 transaction.
The `gasLimitOverride` replaces the gas limit in the original transaction, allowing deployment on MegaETH where gas costs differ from Ethereum.

### Activation

The KeylessDeploy system contract is deployed during `pre_execution_changes()` when the Rex2 hardfork activates.
On pre-Rex2 specs, the contract does not exist and calls to the address have no special behavior.

### Depth-0 restriction

The interceptor only activates at call depth 0 (the top-level transaction call).
If a contract calls KeylessDeploy from within another call (depth > 0), the interception is skipped and the call falls through to the on-chain bytecode, which reverts with `NotIntercepted()`.

This restriction prevents wrap-and-revert attacks where an outer contract could call KeylessDeploy, observe the deployment result, and then revert the outer frame — effectively probing deployment without paying gas.

### Value transfer

Calls to `keylessDeploy()` with non-zero value are rejected.
The interceptor checks `transfer_value` and reverts before executing the sandbox.

### Unknown selectors

Only the `keylessDeploy` selector is intercepted.
Calls with any other selector are not intercepted and fall through to the on-chain bytecode, which reverts with `NotIntercepted()`.

### Sandbox execution

The keyless deploy call executes the decoded transaction in an isolated sandbox.
The sandbox charges a fixed overhead of 100,000 gas for RLP decoding, signature recovery, and state filtering.
On success, the sandbox state (deployed contract, nonce changes) is merged into the main execution state.
On failure, the sandbox state is discarded and the error data is returned to the caller.

For detailed usage instructions and security considerations, see the [Keyless Deployment documentation](../../docs/KEYLESS_DEPLOYMENT.md).

## References

- [Rex2 Specification](../Rex2.md)
- [Rex2 Implementation References](Rex2-Implementation-References.md)
