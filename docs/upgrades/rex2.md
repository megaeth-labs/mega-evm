---
description: Rex2 re-enables SELFDESTRUCT with EIP-6780 semantics and introduces the KeylessDeploy system contract for deterministic cross-chain deployment.
---

# Rex2 Network Upgrade

This page is an informative summary of the Rex2 specification.
For the full normative definition, see the Rex2 spec in the mega-evm repository.

## Summary

Rex2 makes two changes.
First, it restores the `SELFDESTRUCT` opcode with post-Cancun ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)) semantics — MiniRex had completely disabled it.
Second, it introduces the **[KeylessDeploy](../system-contracts/keyless-deploy.md)** system contract, which enables deterministic cross-chain contract deployment via Nick's Method with a gas limit override to account for MegaETH's different [gas pricing](../evm/dual-gas-model.md).

## What Changed

### SELFDESTRUCT Re-Enabled (EIP-6780)

#### Previous behavior
- `SELFDESTRUCT` halts execution with `InvalidFEOpcode`.

#### New behavior
- `SELFDESTRUCT` is a valid opcode.
- If the contract was created in the same transaction, `SELFDESTRUCT` removes the contract's code and storage and transfers the remaining balance to the target address.
- If the contract was not created in the same transaction, `SELFDESTRUCT` only transfers the remaining balance to the target address — code and storage are preserved.

This is the standard EIP-6780 behavior already used across Ethereum post-Cancun.

### KeylessDeploy System Contract

#### Previous behavior
- No system-level support for keyless deployment with gas limit overrides.
- Contracts that deploy via keyless transactions on Ethereum may run out of gas on MegaETH due to different gas pricing.

#### New behavior
- A system contract at `0x6342000000000000000000000000000000000003` provides keyless deployment.
- The contract intercepts calls at depth 0 only (direct transaction calls).

```solidity
interface IKeylessDeploy {
    function keylessDeploy(
        bytes calldata keylessDeploymentTransaction,
        uint256 gasLimitOverride
    ) external returns (uint64 gasUsed, address deployedAddress, bytes memory errorData);
}
```

- `keylessDeploymentTransaction` is the RLP-encoded pre-EIP-155 transaction.
- `gasLimitOverride` replaces the gas limit in the original transaction.
- The sandbox charges a fixed overhead of 100,000 gas (deducted from the caller's total gas, tracked as [compute gas](../glossary.md#compute-gas) from Rex3 onward) for RLP decoding, signature recovery, and state filtering.
- Calls from other contracts (depth > 0) are not intercepted and fall through to on-chain bytecode, reverting with `NotIntercepted()`.
- Value-bearing calls are rejected.
- Unknown selectors are not intercepted and fall through to on-chain bytecode.

## Developer Impact

**SELFDESTRUCT is available again.**
If your contracts previously needed workarounds for the disabled opcode, you can now use `SELFDESTRUCT` with EIP-6780 semantics.
Note that only contracts created in the same transaction can be fully destroyed — pre-existing contracts will only have their balance transferred.

**You can deploy contracts via Nick's Method on MegaETH.**
Use the KeylessDeploy system contract to execute pre-signed keyless deployment transactions with an adjusted gas limit.
The `gasLimitOverride` parameter lets you account for MegaETH's different gas costs without changing the signed transaction (which would change the deployment address).

**KeylessDeploy only works at depth 0.**
You cannot call KeylessDeploy from within another contract — it must be the direct transaction target.
This prevents wrap-and-revert attacks.

## Safety and Compatibility

All pre-Rex2 behavior is unchanged.

The SELFDESTRUCT restoration uses the same [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) semantics already implemented in the underlying revm.
The "same transaction" check means only contracts created via CREATE or CREATE2 within the currently executing transaction are eligible for full destruction.

KeylessDeploy sandbox state (deployed contract, nonce changes) is merged into main execution on success and discarded on failure.

## References

- [mega-evm repository](https://github.com/megaeth-labs/mega-evm)
- [Keyless Deployment](../system-contracts/keyless-deploy.md) — detailed usage guide and examples
- [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780) — SELFDESTRUCT only in same transaction
