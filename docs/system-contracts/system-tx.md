---
description: Mega system transactions — sender/recipient identification rules, whitelisted contracts, and execution bypass semantics.
spec: Rex3
---

# Mega System Transactions

This page specifies how MegaETH identifies and processes Mega System Transactions.
These transactions provide protocol-level maintenance access to whitelisted system contracts.

## Motivation

MegaETH needs a protocol-native mechanism for sequencer-managed maintenance operations such as updating Oracle state.
Those operations must execute with special fee and validation semantics while remaining part of the verifiable execution model.

Mega System Transactions provide that mechanism.

## Specification

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

### Scope

This page specifies the execution semantics of Mega System Transactions.
Admission policy — including which external actor is allowed to inject such transactions into the chain — is outside the scope of this specification.

## Constants

| Constant | Value | Description |
| -------- | ----- | ----------- |
| `MEGA_SYSTEM_ADDRESS` | `0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d` | Special maintenance sender address |
| `MEGA_SYSTEM_TX_WHITELIST` | `{ ORACLE_CONTRACT_ADDRESS }` | Stable whitelist of callable system-contract targets |

## Rationale

**Why special transaction semantics instead of ordinary calls?**
Protocol maintenance operations such as Oracle updates must execute with deterministic behavior that is not coupled to ordinary fee payment and admission semantics.
Giving them distinct execution treatment makes that behavior explicit.

**Why preserve the caller as `MEGA_SYSTEM_ADDRESS`?**
Whitelisted target contracts must be able to distinguish protocol-maintenance calls from ordinary user calls.
Preserving the caller identity provides that signal without introducing a separate call context type.

## Spec History

- [MiniRex](../upgrades/minirex.md) introduced Mega System Transactions and the `MEGA_SYSTEM_ADDRESS` mechanism.
- [Rex](../upgrades/rex.md), [Rex1](../upgrades/rex1.md), [Rex2](../upgrades/rex2.md), and [Rex3](../upgrades/rex3.md) retain the same stable semantics.
