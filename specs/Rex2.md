# Rex2 Specification

## Abstract

Rex2 is the second patch to the Rex hardfork.
It re-enables the `SELFDESTRUCT` opcode with post-Cancun (EIP-6780) semantics and introduces the KeylessDeploy system contract for deterministic contract deployment with gas limit overrides.
All Rex1 semantics are preserved unless explicitly changed below.

## Changes

### 1. SELFDESTRUCT re-enabled (EIP-6780)

#### Motivation

MiniRex completely disabled `SELFDESTRUCT`, making it an invalid opcode.
Rex2 restores it with EIP-6780 semantics to support contracts that rely on same-transaction self-destruct patterns.

#### Semantics

Previous behavior:
- `SELFDESTRUCT` halts execution with `InvalidFEOpcode`.

New behavior:
- `SELFDESTRUCT` MUST be a valid opcode.
- If the contract was created in the same transaction, `SELFDESTRUCT` MUST remove the contract's code and storage and transfer the remaining balance to the beneficiary.
- If the contract was not created in the same transaction, `SELFDESTRUCT` MUST only transfer the remaining balance to the beneficiary and MUST NOT delete code or storage.

### 2. KeylessDeploy system contract

#### Motivation

MegaETH's gas model prices operations differently than Ethereum.
Contracts that deploy successfully via keyless transactions (Nick's Method) on Ethereum may run out of gas on MegaETH.
Since modifying the signed transaction to increase the gas limit would change the recovered signer and the deployment address, a system contract is needed to apply a gas limit override at execution time while preserving the original signature.

#### Semantics

Previous behavior:
- No system-level support for keyless deployment with gas limit overrides.

New behavior:
- A system contract MUST be deployed at `0x6342000000000000000000000000000000000003`.
- The contract MUST provide `keylessDeploy(bytes keylessDeploymentTransaction, uint256 gasLimitOverride)` returning `(uint64 gasUsed, address deployedAddress, bytes errorData)`.
- The contract MUST only be intercepted at call depth 0 (direct transaction call).
- Calls from other contracts (depth > 0) MUST NOT be intercepted and MUST fall through to on-chain bytecode, reverting with `NotIntercepted()`.
- Value-bearing calls to the contract MUST revert.
- Unknown selectors MUST NOT be intercepted and MUST fall through to on-chain bytecode, reverting with `NotIntercepted()`.
- The keyless deploy sandbox MUST charge a fixed overhead of 100,000 gas for RLP decoding, signature recovery, and state filtering.

## Invariants

- `I-1`: Stable Rex1 semantics MUST remain unchanged.
- `I-2`: `SELFDESTRUCT` of a contract not created in the same transaction MUST NOT delete code or storage.
- `I-3`: KeylessDeploy interception MUST only occur at depth 0.

## Inheritance

Rex2 inherits Rex1 except for the deltas defined in `Changes`.
Semantic lineage: `Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague`.

## References

- [Rex1 Specification](Rex1.md)
- [Rex3 Specification](Rex3.md)
- [Rex Specification](Rex.md)
- [MiniRex Specification](MiniRex.md)
- [Rex2 Behavior Details (Informative)](impl/Rex2-Behavior-Details.md)
- [Rex2 Implementation References (Informative)](impl/Rex2-Implementation-References.md)
- [EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)
- [Keyless Deployment](../docs/KEYLESS_DEPLOYMENT.md)
