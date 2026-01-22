# Rex2 Specification

Rex2 is the second patch to the Rex hardfork. It restores the `SELFDESTRUCT` opcode with
post-Cancun (EIP-6780) semantics while inheriting all Rex1 behavior.

## Changes from Rex1

### 1. SELFDESTRUCT Re-enabled (EIP-6780)

Rex2 re-enables `SELFDESTRUCT` with EIP-6780 semantics:

- `SELFDESTRUCT` is no longer an invalid opcode.
- If a contract was **created in the same transaction**, `SELFDESTRUCT` removes the contract and
  its storage, as in the pre-Cancun behavior.
- If a contract was **not** created in the same transaction, `SELFDESTRUCT` only transfers the
  remaining balance to the beneficiary and does **not** delete code or storage.

### 2. KeylessDeploy System Contract

Rex2 introduces the **KeylessDeploy** system contract to enable keyless deployment (Nick's Method)
on MegaETH with custom gas limits.

**System Contract Address**: `0x6342000000000000000000000000000000000003`

**Why it's needed**: MegaETH's gas model prices operations differently than Ethereum. Contracts
that deploy successfully via keyless transactions on Ethereum may run out of gas on MegaETH.
Since modifying the signed transaction to increase the gas limit would change the recovered signer
(and thus the deployment address), a system contract is needed to apply a gas limit override at
execution time while preserving the original signature.

**Restriction**: The system contract must be called directly in a transaction (`depth == 0`). Calls
from other contracts will revert with `NotIntercepted()`. This prevents wrap-and-revert attacks
that could avoid gas charges.

**Interface**:

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress, bytes memory errorData);
}
```

For detailed usage instructions, examples, and security considerations, see the
[Keyless Deployment documentation](../docs/KEYLESS_DEPLOYMENT.md).

## Inheritance

Rex2 inherits all Rex1 behavior (including compute gas limit reset between transactions) and all
features from Rex and MiniRex.

The semantics of Rex2 are inherited from:

- **Rex2** -> **Rex1** -> **Rex** -> **MiniRex** -> **Optimism Isthmus** -> **Ethereum Prague**

## References

- [Rex1 Specification](Rex1.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
- [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)
- [Keyless Deployment](../docs/KEYLESS_DEPLOYMENT.md)
