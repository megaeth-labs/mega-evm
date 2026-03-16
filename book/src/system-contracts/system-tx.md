# Mega System Transactions

## Overview

Mega System Transactions are special transactions used by the MegaETH sequencer to perform state maintenance operations.
They enable the sequencer to interact with whitelisted system contracts without incurring transaction fees.

> **Security**: Mega system transactions can only be included by the sequencer.
> Any mega system transaction submitted via L1 bridge or L2 mempool is ignored.

## Definition

A transaction is a Mega System Transaction if:

- It is a legacy transaction (EIP-2718)
- Its signer is the `MEGA_SYSTEM_ADDRESS` (`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`)
- Its target is within the `MEGA_SYSTEM_TX_WHITELIST`

## Whitelisted Contracts

| Contract         | Address                                        |
| ---------------- | ---------------------------------------------- |
| Oracle Contract  | `0x6342000000000000000000000000000000000001`    |

## Processing

Mega system transactions have unique properties:

1. **Zero transaction fees** — No L2 gas fees, L1 data fees, operator fees, or base fee charges
2. **Deposit-like processing** — Bypasses signature validation, skips nonce verification and balance checks (nonce still increments)
3. **No beneficiary impact** — No state changes to block beneficiary or fee vaults

## Use Case

The primary use case is updating oracle storage.
The sequencer publishes off-chain data (price feeds, etc.) to the [Oracle Contract](oracle.md) via system transactions, making it available to all smart contracts at minimal cost.
