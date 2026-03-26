# Mega System Transactions

## Overview

Mega System Transactions are special transactions used by the MegaETH sequencer to perform state maintenance operations.
They enable the sequencer to interact with whitelisted [system contracts](overview.md) without incurring transaction fees.

{% hint style="info" %}
mega-evm defines how these transactions are identified and executed.
Whether only the sequencer may admit them to the chain is enforced by components outside this EVM implementation.
{% endhint %}

## Definition

A transaction is a Mega System Transaction if:

- It is a legacy transaction
- Its signer is the `MEGA_SYSTEM_ADDRESS` (`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`)
- Its target is within the `MEGA_SYSTEM_TX_WHITELIST`

## Whitelisted Contracts

| Contract         | Address                                        |
| ---------------- | ---------------------------------------------- |
| Oracle Contract  | `0x6342000000000000000000000000000000000001`    |

## Processing

Mega system transactions have unique properties:

1. **Zero transaction fees** — No L2 gas fees, L1 data fees, operator fees, or base fee charges
2. **Deposit-like processing** — Bypasses signature validation, skips nonce verification, and skips balance checks for gas fees
3. **Nonce handling** — Nonce verification is skipped (the transaction is accepted regardless of the sender's current nonce), but the sender's nonce still increments after execution
4. **No beneficiary impact** — No state changes to block beneficiary or fee vaults

In production, these transactions are expected to be injected by the sequencer.
Admission policy is outside the scope of mega-evm itself.

## Use Case

The primary use case is updating oracle storage.
The sequencer publishes off-chain data (price feeds, etc.) to the [Oracle Contract](oracle.md) via system transactions, making it available to all smart contracts at minimal cost.
