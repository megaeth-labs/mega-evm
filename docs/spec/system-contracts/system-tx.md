---
description: Mega system transactions — sender/recipient identification rules, whitelisted contracts, and execution bypass semantics.
spec: Rex5
---

# Mega System Transactions

This page specifies how MegaETH identifies and processes Mega System Transactions.
These transactions provide protocol-level maintenance access to whitelisted system contracts.

## Motivation

MegaETH needs a protocol-native mechanism for protocol-managed maintenance operations such as updating Oracle state.
Those operations must execute with special fee and validation semantics while remaining part of the verifiable execution model.

Mega System Transactions provide that mechanism.

## Specification

### Resolution

`MEGA_SYSTEM_ADDRESS` is the authorized system-transaction sender for the current block.
A node MUST resolve it per block from [`SequencerRegistry.currentSystemAddress()`](sequencer-registry.md) after all pre-block changes are committed.
The fixed constant `0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d` is the genesis seed value for this address (see [Constants](#constants)); subsequent rotations are reflected by the registry value.

### Identification

A transaction MUST be classified as a Mega System Transaction if and only if all of the following are true:

1. the transaction type is legacy (`0x0`),
2. the signer is `MEGA_SYSTEM_ADDRESS`,
3. the transaction target is a `CALL` to an address in `MEGA_SYSTEM_TX_WHITELIST`.

CREATE transactions from `MEGA_SYSTEM_ADDRESS` MUST NOT be treated as Mega System Transactions.

### Stable Whitelist

`MEGA_SYSTEM_TX_WHITELIST` MUST contain only `ORACLE_CONTRACT_ADDRESS`.

### Processing Semantics

When a transaction is classified as a Mega System Transaction, a node MUST process it with the following special semantics:

- It MUST bypass ordinary signature validation.
- It MUST bypass gas-fee charging.
- It MUST bypass nonce validation.
- It MUST bypass balance checks for gas fees.
- It MUST preserve `MEGA_SYSTEM_ADDRESS` as the caller seen by the target contract.

Although nonce validation is bypassed, the sender nonce MUST still increment after execution.

Mega System Transactions MUST NOT cause state changes to the block beneficiary or fee vaults.

Before applying the special semantics above, a node MUST validate the transaction's chain id, nonce, and sender-code status against the same canonical rules that ordinary transactions follow:

- The transaction's chain id MUST be present and MUST equal the node's configured chain id, unless the chain-id check is disabled by node configuration.
- The transaction's nonce MUST equal the current account nonce of `MEGA_SYSTEM_ADDRESS`, unless the nonce check is disabled by node configuration.
- If `MEGA_SYSTEM_ADDRESS` carries code, the EIP-3607 sender-has-code rule applies, unless EIP-3607 enforcement is disabled by node configuration.

A transaction failing any of these checks MUST be rejected with the corresponding canonical invalid-transaction error (missing chain id, invalid chain id, nonce too low, nonce too high, or caller-has-code rejection) before any state mutation, signature bypass, or fee bypass takes effect.
The remaining bypasses (signature, gas-fee charging, balance) MUST still apply once these checks pass.

### Scope

This page specifies the execution semantics of Mega System Transactions.
Admission policy — including which external actor is allowed to inject such transactions into the chain — is outside the scope of this specification.

## Constants

| Constant                   | Value                                        | Description                                          |
| -------------------------- | -------------------------------------------- | ---------------------------------------------------- |
| `MEGA_SYSTEM_ADDRESS`      | `0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d` | Special maintenance sender address                   |
| `MEGA_SYSTEM_TX_WHITELIST` | `{ ORACLE_CONTRACT_ADDRESS }`                | Stable whitelist of callable system-contract targets |

## Rationale

**Why special transaction semantics instead of ordinary calls?**
Protocol maintenance operations such as Oracle updates must execute with deterministic behavior that is not coupled to ordinary fee payment and admission semantics.
Giving them distinct execution treatment makes that behavior explicit.

**Why preserve the caller as `MEGA_SYSTEM_ADDRESS`?**
Whitelisted target contracts must be able to distinguish protocol-maintenance calls from ordinary user calls.
Preserving the caller identity provides that signal without introducing a separate call context type.

**Why restore the chain-id, nonce, and EIP-3607 checks?**
Earlier specs bypassed all four canonical pre-checks via OP-style deposit promotion.
OP deposits can do this because L1 derivation and per-deposit source-hash uniqueness provide replay protection at a higher layer.
MegaETH system transactions have neither, so the canonical chain-id, nonce, and sender-code checks are restored to guard against replay and cross-chain misuse.
The configuration toggles for these checks are honored so that the system-transaction validation path stays symmetric with the ordinary user-transaction validation path for debugging, state-test, and replay tooling.

## Security Considerations

**The no-fee/no-nonce exemption MUST apply exclusively to addresses in `MEGA_SYSTEM_TX_WHITELIST`.**
If the whitelist check were absent or misconfigured, any legacy transaction from `MEGA_SYSTEM_ADDRESS` could bypass gas fees and nonce validation for arbitrary targets, enabling cost-free state manipulation across the entire chain.

**Adding an address to `MEGA_SYSTEM_TX_WHITELIST` is a consensus-critical change.**
Any modification to the stable whitelist changes which transactions receive privileged processing.
Such changes MUST be gated by a spec upgrade to ensure all nodes agree on the new whitelist contents at the same activation point.

**CREATE transactions from `MEGA_SYSTEM_ADDRESS` MUST NOT receive privileged processing.**
Allowing fee-exempt contract creation would let `MEGA_SYSTEM_ADDRESS` deploy arbitrary code at no cost, which is outside the intended maintenance scope.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced Mega System Transactions and the `MEGA_SYSTEM_ADDRESS` mechanism.
- [Rex](../upgrades/rex.md), [Rex1](../upgrades/rex1.md), [Rex2](../upgrades/rex2.md), and [Rex3](../upgrades/rex3.md) retain the same stable semantics.
- [Rex5](../upgrades/rex5.md) dynamized the system address — it is no longer a compile-time constant but is resolved per block from `SequencerRegistry.currentSystemAddress()` — and restored the canonical chain-id, nonce, and EIP-3607 sender-code checks that earlier specs bypassed via deposit promotion.
  Blocks before Rex5 continue to use the legacy `MEGA_SYSTEM_ADDRESS` constant and the deposit-promotion bypasses.
  The system transaction identification logic and whitelist are unchanged.
