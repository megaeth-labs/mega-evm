# Mega System Transactions

## Overview

**Mega System Transactions** are special transactions used by the MegaETH sequencer to perform state maintenance operations with minimal side effects. They enable the sequencer to interact with whitelisted system contracts without incurring transaction fees or affecting normal fee distribution mechanisms.

> _Security_: Mega system transactions are restricted to be only able to call a whitelisted contract. It can only be included by the sequencer itself. Any mega system transaction submitted on L1 bridge or via L2 mempool should be ignored.

## Definition

A transaction satisfying the following conditions is a **Mega System Transaction**:

- It is a legacy transaction as defined in EIP-2718, and
- It has a valid signature whose signer is a defined `MEGA_SYSTEM_ADDRESS`.
- Its calling target (i.e., the `to` address) must be within `MEGA_SYSTEM_TX_WHITELIST`.

The `MEGA_SYSTEM_ADDRESS` is a special account controlled by the sequencer:
`0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d`, which is held exclusively by the sequencer and not accessible to regular users (ignored if sent from L1 bridge or L2 mempool)

`MEGA_SYSTEM_TX_WHITELIST` contains the following contracts:

- **Oracle Contract** (`0x6342000000000000000000000000000000000001`): Provides off-chain data to on-chain contracts. For more information, see [ORACLE_SERVICE.md](./ORACLE_SERVICE.md).

## Processing

Mega system transactions possess unique properties that differentiate them from regular transactions:

1. **Zero Transaction Fees**: No gas fees are deducted from the system address

   - No L2 gas fees
   - No L1 data fees
   - No operator fees
   - No base fee charges

2. **Deposit-Like Processing**: Processed internally as deposit transactions

   - Bypasses signature validation
   - Skips nonce verification, but the sender's nonce still bumps
   - Skips balance checks for gas fees
   - Still increments sender nonce normally

3. **No Beneficiary Impact**: State changes to block beneficiary or fee vaults are avoided
   - Maintains clean fee accounting
   - Prevents unintended balance modifications
