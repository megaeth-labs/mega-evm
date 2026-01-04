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
