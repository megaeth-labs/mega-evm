---
description: Satin network upgrade (unstable) — MegaETH's execution rules restated on Optimism Karst (Ethereum Osaka) — EIP-8037 regular and state gas under a 200,000,000 execution cap with a state-gas reservoir, a history-gas ledger for the bytes a transaction appends, SALT-scaled state gas, per-transaction limits that stop a transaction with a revert, gas detention on withheld gas, EIP-7708 transfer logs, keyless deployment as a native creation, system calls on a split budget, the live system address read by the transaction itself, block limits as packing budgets, and the witness a stateless validator re-executes a block from.
---

# Satin Network Upgrade

This page is an informative summary of the Satin specification.
For the full normative definition, see the Satin spec in the mega-evm repository.

{% hint style="warning" %}
**Unstable** — Satin is under active development.
Every value and rule on this page may change before Satin is frozen, and nothing here should be relied on.
{% endhint %}

## Summary

Satin restates MegaETH's execution rules on a new base, Optimism Karst (Ethereum Osaka), and replaces the [dual gas model](../evm/dual-gas-model.md) with the two gas pools of [EIP-8037](https://eips.ethereum.org/EIPS/eip-8037).
A transaction's gas is split into regular gas, which pays for computation and is capped at 200,000,000 per transaction (the [execution cap](../glossary.md#execution-cap)), and a state-gas [reservoir](../glossary.md#reservoir) that holds the rest of its gas limit.
State growth is paid in [state gas](../glossary.md#state-gas) — EIP-8037's byte counts at MegaETH's own cost per state byte, scaled by the [SALT bucket](../glossary.md#salt-bucket) the state lands in — and the bytes a transaction appends to the chain's history (its body, its logs, its [write records](../glossary.md#write-record), its deployed code) are paid in a third ledger, [history gas](../glossary.md#history-gas).
Together they replace [storage gas](../glossary.md#storage-gas), the 39,000 intrinsic storage gas, the 10× multiplier on logs and calldata, and the 23,000-gas [storage gas stipend](../glossary.md#storage-gas-stipend).

The per-transaction resource limits keep data size and KV updates, measure state growth in state gas, and drop the separate compute-gas limit: compute is regular gas, bounded by the execution cap and by [gas detention](../evm/gas-detention.md).
Every transaction-level limit — data size, KV updates, state gas, and detention's compute limit — now stops a transaction with a revert carrying `MegaLimitExceeded(kind, limit)` instead of a halt, and the sender pays only for what ran.
Detention holds each frame's spendable regular gas inside the frame's own gas, so a transaction that read volatile data runs exactly as it would have without the read until a regular charge needs the gas it is not allowed to spend.

Satin also brings EIP-7708 transfer logs, deploys a `keylessDeploy` transaction as a native creation instead of in a sandbox, runs system calls on at most 30,000,000 of regular gas with the rest as reservoir, has a system-address transaction read the live system address itself, and makes every block-level execution limit a packing budget that never refuses a deposit.

Satin follows [Rex6](rex6.md) on the spec ladder, so every **Previous behavior** below is Rex6's.
Satin does not add to the Rex6 rules one by one: it restates the execution layer, and a Rex6 rule this page does not carry over does not apply under Satin.

The changes, in the order this page records them:

1. **Base layer.** Optimism Karst (Ethereum Osaka), four Amsterdam opcodes, and no EIP-7825 transaction gas cap.
2. **Precompiles.** Karst's set, with KZG point evaluation still at 100,000.
3. **Contract size limits.** 512 KiB of code and 1 MiB of initcode.
4. **Two-pool gas.** EIP-8037 regular gas and state-gas reservoir under a 200,000,000 execution cap, and three gas ledgers.
5. **Gas schedule and intrinsic gas.** Amsterdam's schedule and EIP-2780 intrinsic cost, with the EIP-8038 repricing left at Osaka's values.
6. **State gas and SALT pricing.** `entry × multiplier` on EIP-8037's byte counts, replacing `base × (multiplier − 1)`.
7. **History gas.** A price per byte on everything a transaction appends to history.
8. **History allowance.** A 160-byte allowance for value calls, replacing the 23,000-gas storage gas stipend.
9. **Gas forwarding.** The standard 63/64 rule, replacing 98/100.
10. **Resource limits.** Data size and KV updates over a byte table and write records, state growth as a state-gas limit, and no compute-gas limit.
11. **The revert-class stop.** Every transaction-level limit stops the transaction with a revert.
12. **Gas detention on withheld gas.**
13. **Volatile-data access control and `remainingComputeGas()`.**
14. **EIP-7708 transfer logs.**
15. **Native keyless deployment.**
16. **Oracle storage reads and hints.**
17. **The live system address.**
18. **System calls and pre-block calls.**
19. **The protocol's own transactions.**
20. **Block limits.**
21. **Transaction and block refusals.**
22. **The stateless witness.** What a validator re-executing a block must be given, and what the engine exports for it.

## What Changed

### 1. Base Layer: Optimism Karst (Ethereum Osaka)

#### Previous behavior

- Every spec through Rex6 builds on Optimism Isthmus (Ethereum Prague); every standard EVM, transaction and block rule the specification does not override is Isthmus's.
- The Osaka opcode `CLZ` and the Amsterdam opcodes `DUPN`, `SWAPN`, `EXCHANGE` and `SLOTNUM` are undefined.

#### New behavior

- Satin builds on Optimism Karst, whose Ethereum base is Osaka.
  Every standard EVM, transaction and block rule this page does not override MUST be Karst's, including the Optimism changes of Jovian and Karst and the Ethereum changes of Osaka (for example `CLZ`, [EIP-7939](https://eips.ethereum.org/EIPS/eip-7939)).
- The per-transaction gas limit cap of [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825) MUST NOT be applied.
  The execution cap of [Two-Pool Gas](#4-two-pool-gas-eip-8037-replaces-the-dual-gas-model) bounds a transaction's regular gas instead, and a transaction's gas limit is bounded by the block's gas limit; a block builder MAY refuse a larger declared gas limit as building policy, which a node validating a block does not apply (see [Block Limits](#20-block-limits)).
- A node MUST enable four opcodes that Osaka does not have: `DUPN` (`0xE6`), `SWAPN` (`0xE7`) and `EXCHANGE` (`0xE8`) from [EIP-8024](https://eips.ethereum.org/EIPS/eip-8024), and `SLOTNUM` (`0x4B`) from [EIP-7843](https://eips.ethereum.org/EIPS/eip-7843).
  `SLOTNUM` MUST push the block's slot number as the node supplies it, and zero for a block that carries none.
- The Karst block rules apply to MegaETH blocks:
  - A block in which Satin activates, or in which an Optimism fork from Jovian onward activates, MUST contain only deposit transactions; a non-deposit transaction in it makes the block invalid.
  - Each non-deposit transaction's data-availability footprint — its compressed size times the footprint gas scalar held by the L1 block contract — MUST accumulate against the block's gas limit, and the block MUST report the sum as its blob gas used.
    A transaction whose footprint does not fit in what the block has left MUST NOT be included.
    With no scalar in state the scalar is zero, and the rule costs nothing.
  - Every non-deposit transaction MUST be priced against the L1 block information that the block's own L1 attributes deposit wrote.

### 2. Precompiles

#### Previous behavior

- The precompile set is Isthmus's with two MegaETH overrides: KZG point evaluation (`0x0A`) at a fixed 100,000 gas, and ModExp (`0x05`) on the Osaka schedule ([EIP-7823](https://eips.ethereum.org/EIPS/eip-7823) and [EIP-7883](https://eips.ethereum.org/EIPS/eip-7883)).
- `P256VERIFY` (`0x100`) costs 3,450 gas.
- Input size limits: BN254 pairing (`0x08`) 112,687 bytes; BLS12-381 G1 MSM (`0x0C`) 513,760 bytes, G2 MSM (`0x0E`) 488,448 bytes, pairing (`0x0F`) 235,008 bytes.

#### New behavior

The precompile set MUST be Karst's, with KZG point evaluation at MegaETH's price:

| Precompile             | Address | Satin                                                           |
| ---------------------- | ------- | --------------------------------------------------------------- |
| KZG point evaluation   | `0x0A`  | Fixed 100,000 gas (unchanged)                                   |
| ModExp                 | `0x05`  | Osaka schedule (unchanged)                                      |
| `P256VERIFY`           | `0x100` | 6,900 gas ([EIP-7951](https://eips.ethereum.org/EIPS/eip-7951)) |
| BN254 pairing          | `0x08`  | Input of at most 57,600 bytes                                   |
| BLS12-381 G1 MSM       | `0x0C`  | Input of at most 288,960 bytes                                  |
| BLS12-381 G2 MSM       | `0x0E`  | Input of at most 278,784 bytes                                  |
| BLS12-381 pairing      | `0x0F`  | Input of at most 156,672 bytes                                  |
| Every other precompile | Any     | As Karst defines it                                             |

An input over a size limit fails the call before any gas check, as the Optimism specification of the limit has it.
KZG point evaluation keeps its Rex6 semantics: a call given less than 100,000 gas fails with `OutOfGas`, and a successful call is charged exactly 100,000.

### 3. Contract Size Limits

#### Previous behavior

- Deployed code of at most 524,288 bytes; initcode of at most 548,864 bytes (the code limit plus 24,576).

#### New behavior

- Deployed code MUST be at most `MAX_CONTRACT_SIZE` = 524,288 bytes.
- Initcode MUST be at most `MAX_INITCODE_SIZE` = 1,048,576 bytes, twice the code limit, the ratio [EIP-3860](https://eips.ethereum.org/EIPS/eip-3860) sets between the two.
- The creation opcodes check their operands, the static context and the initcode size in the inherited order: Satin charges nothing ahead of those checks.

### 4. Two-Pool Gas (EIP-8037) Replaces the Dual Gas Model

#### Previous behavior

- A transaction's gas is one pool; `total_gas_used = compute_gas_used + storage_gas_used`.
- Compute gas is separately limited to 200,000,000 per transaction, a limit that halts the transaction.
- Storage gas is charged out of the same pool for `SSTORE` of a fresh slot, account creation and contract creation (`base × (multiplier − 1)`), code deposit (10,000 per byte), logs (3,750 per topic and 80 per data byte) and calldata (40 per zero byte and 160 per non-zero byte, floors 100 and 400), plus 39,000 intrinsic storage gas per transaction.

#### New behavior

A node MUST apply [EIP-8037](https://eips.ethereum.org/EIPS/eip-8037), as Amsterdam applies it, with the parameters below.

- **The execution cap.**
  `EXECUTION_CAP` = 200,000,000.
  At the start of execution:

  ```
  regular_budget = min(gas_limit, EXECUTION_CAP) − intrinsic_regular_gas
  reservoir      = max(0, gas_limit − EXECUTION_CAP)
  ```

  - `gas_limit` — the transaction's gas limit.
  - `intrinsic_regular_gas` — the regular part of the transaction's intrinsic gas (see [Gas Schedule and Intrinsic Gas](#5-gas-schedule-and-intrinsic-gas)).

  The intrinsic state gas — under Satin, the history gas of the transaction's body (see [History Gas](#7-history-gas)) — MUST be taken from the reservoir first, and only its excess from the regular budget.

- **Three kinds of charge.**
  A regular charge (every inherited opcode and intrinsic cost) MUST draw only the regular budget.
  A state charge (see [State Gas](#6-state-gas-and-salt-pricing)) and a history charge (see [History Gas](#7-history-gas)) MUST draw the reservoir first and spill onto the regular budget only past it.
  `GAS` MUST return the regular budget the frame has left, not the reservoir.
- **Frames.**
  A call or creation MUST forward regular gas as the inherited rules have it (see [Gas Forwarding](#9-gas-forwarding)), and the child MUST inherit the caller's whole reservoir; what the child leaves of the reservoir returns to the caller.
  When a frame reverts or halts, the state and history gas it charged MUST be given back: the part the reservoir paid returns to the reservoir, and the part that spilled returns to the frame's regular budget.
  A revert then returns the frame's unspent regular gas to its caller; an exceptional halt consumes it, the returned spill included.
- **The transaction.**
  When the transaction ends, the reservoir left MUST return to the sender, and so MUST the unspent regular gas unless the transaction's own frame halted; this holds when the transaction reverts or is stopped by a limit (see [The Revert-Class Stop](#11-the-revert-class-stop)).
  The receipt's gas used is what the transaction spent from both pools — regular, state and history gas together — after the inherited refund and floor rules.
- **Three ledgers.**
  A node MUST split a transaction's spend into three ledgers, all paid from the pools above:
  - _regular gas_ — `total − state − history`;
  - _state gas_ — the state charges it kept;
  - _history gas_ — the history charges it kept, the body included.

  State or history gas that spilled onto the regular budget stays on its own ledger.
  The [EIP-7623](https://eips.ethereum.org/EIPS/eip-7623) floor, as [EIP-7976](https://eips.ethereum.org/EIPS/eip-7976) and [EIP-7981](https://eips.ethereum.org/EIPS/eip-7981) amend it, MUST be computed and validated as Amsterdam computes it; the figure a block counts towards its execution-gas limit is `max(regular, floor)`, with history gas taken out of the regular ledger before the floor applies.

- **Validation.**
  A node MUST reject a transaction, before it is included:
  - whose gas limit is below its intrinsic regular gas plus the history gas of its body (`CallGasCostMoreThanGasLimit`);
  - whose floor exceeds its gas limit (`GasFloorMoreThanGasLimit`);
  - whose gas limit exceeds `EXECUTION_CAP` while its intrinsic regular gas or its floor exceeds `EXECUTION_CAP` (`GasFloorMoreThanGasLimit`).

Satin has no separate compute-gas limit: compute is regular gas, bounded by the execution cap and, after a read of volatile data, by [gas detention](#12-gas-detention-on-withheld-gas).

### 5. Gas Schedule and Intrinsic Gas

#### Previous behavior

- The gas schedule is Prague's.
- Intrinsic gas is 21,000 (plus 32,000 for a creation) plus calldata, access-list and EIP-7702 costs, plus 39,000 intrinsic storage gas and the storage gas of the calldata.
- An EIP-7702 authorization costs 25,000 intrinsic gas, with 12,500 refunded for an authority that already exists.
- Code deposit costs 200 gas per byte, plus 10,000 storage gas per byte.

#### New behavior

The gas schedule MUST be Amsterdam's — including [EIP-2780](https://eips.ethereum.org/EIPS/eip-2780)'s decomposition of the intrinsic cost, EIP-7976's calldata floor and EIP-7981's access-list floor — with three changes:

- the seventeen regular-gas entries below, which Amsterdam repriced for [EIP-8038](https://eips.ethereum.org/EIPS/eip-8038) (four of them because EIP-8037 moved the cost of creating state onto the state dimension), MUST keep their Osaka values;
- the EIP-8037 state-gas entries MUST be rebuilt at MegaETH's cost per state byte (see [State Gas](#6-state-gas-and-salt-pricing));
- the history gas of deposited code MUST be the cost of one history byte (see [History Gas](#7-history-gas)); Amsterdam prices it at zero.

Every other entry MUST be Amsterdam's, including the regular code-deposit cost (zero), the EIP-7702 entries, and the entries Amsterdam introduced for EIP-8037, EIP-2780, EIP-7976 and EIP-7981.

Regular-gas entries that keep their Osaka value:

| Entry                                          | Satin  |
| ---------------------------------------------- | ------ |
| Warm storage read                              | 100    |
| Cold account access, additional                | 2,500  |
| Cold storage access, additional                | 2,000  |
| Cold `SLOAD`                                   | 2,100  |
| Value transfer of a call                       | 9,000  |
| New account of a call                          | 25,000 |
| New account of `SELFDESTRUCT`                  | 25,000 |
| `SSTORE` static cost                           | 100    |
| `SSTORE` of a fresh slot, before the load cost | 19,900 |
| `SSTORE` reset, before the cold load           | 2,800  |
| `SSTORE` refund of a restored fresh slot       | 19,900 |
| `SSTORE` refund of a restored reset            | 2,800  |
| `SSTORE` refund of a cleared slot              | 4,800  |
| `CREATE` and `CREATE2`                         | 32,000 |
| Creation transaction (unused under EIP-2780)   | 32,000 |
| Access-list address                            | 2,400  |
| Access-list storage key                        | 1,900  |

Entries that differ from Osaka:

| Entry                                         | Osaka  | Satin   |
| --------------------------------------------- | ------ | ------- |
| Code deposit, regular gas per byte            | 200    | 0       |
| Code deposit, state gas per byte              | 0      | 1,530   |
| Code deposit, history gas per byte            | 0      | 88      |
| Fresh-slot `SSTORE`, state gas                | 0      | 97,920  |
| New account, state gas                        | 0      | 183,600 |
| Created account, state gas                    | 0      | 183,600 |
| EIP-7702 delegation indicator, state gas      | 0      | 35,190  |
| EIP-7702 authorization, intrinsic regular gas | 25,000 | 7,816   |
| EIP-7702 refund for an existing authority     | 12,500 | 0       |
| Account write (EIP-2780)                      | 0      | 9,000   |
| Creation access (EIP-2780)                    | 0      | 12,000  |
| Floor cost per token                          | 10     | 16      |
| Floor base                                    | 21,000 | 12,000  |
| Floor tokens per zero calldata byte           | 1      | 4       |
| Floor tokens per access-list byte             | 0      | 4       |

Under EIP-2780 the floor's base is the transaction's own decomposed intrinsic base (below), not the 12,000 entry.

The EIP-2780 intrinsic regular gas of a transaction MUST be `TX_BASE_COST` = 12,000, plus, for a call to another account, 3,000 for the recipient's access and 6,000 more when it carries value; plus, for a creation, 12,000 and EIP-3860's initcode cost; plus the calldata, access-list and per-authorization costs.
A transfer to the sender itself pays the base alone.
The state-dependent parts — a recipient or created account that does not exist, an authority that does not exist, a delegation indicator, an authority's first write — MUST be charged at the start of execution, as EIP-2780 and EIP-8037 specify, with every state charge priced as [State Gas](#6-state-gas-and-salt-pricing) says.
A deposit-like transaction whose caller account does not exist MUST be charged the new-account state gas for it exactly once.

Deposited code MUST be charged, once the creation's code passed its checks and in this order: the regular per-byte cost (zero), the hashing cost of the code at 6 gas per 32-byte word, the state gas of the code, and its history gas.
A creation that cannot pay any of the four runs out of gas and leaves no code.

A node MUST charge account access to the addresses the inherited rules treat as warm from the start of a transaction — precompiles, access-list addresses and the block beneficiary — as warm, as [EIP-2929](https://eips.ethereum.org/EIPS/eip-2929) has it: Rex6's cold charge on the first `CALL`-family touch of such an address does not apply.

### 6. State Gas and SALT Pricing

#### Previous behavior

- Dynamic storage gas is `base × (multiplier − 1)` with bases 20,000 (fresh slot), 25,000 (new account) and 32,000 (contract creation), where `multiplier = bucket_capacity / MIN_BUCKET_SIZE`.
- A write into a minimum-size bucket pays no storage gas.

#### New behavior

Every EIP-8037 state charge MUST be `entry × m`:

- `entry` — the schedule's state-gas entry: EIP-8037's byte count of the state times `COST_PER_STATE_BYTE` = 1,530.
- `m` — the multiplier of the [SALT bucket](../glossary.md#salt-bucket) the state lands in, `floor(bucket_capacity / MIN_BUCKET_SIZE)` with `MIN_BUCKET_SIZE` = 256.

| State                                   | Bytes | `entry` at `m = 1` | Bucket                |
| --------------------------------------- | ----- | ------------------ | --------------------- |
| Fresh storage slot (`SSTORE` 0 → non-0) | 64    | 97,920             | The slot's            |
| New account                             | 120   | 183,600            | The account's         |
| Created contract account                | 120   | 183,600            | The created account's |
| Deployed code, per byte                 | 1     | 1,530              | The created account's |
| EIP-7702 delegation indicator           | 23    | 35,190             | The authority's       |

- A state charge MUST be made where the state is added: a fresh slot by the `SSTORE` that fills it; a new account by the `CALL` that moves value to it, by a `SELFDESTRUCT` that moves a balance to it, by the transaction whose value creates it, and by an applied EIP-7702 authorization that creates it; a created account by `CREATE`, `CREATE2` or a creation transaction.
- `m` scales state gas and nothing else: regular gas never scales.
- A bucket's multiplier MUST be read at most once per transaction: every later charge, and every give-back, in that bucket MUST use the same `m`, so a give-back cancels its charge exactly.
- A bucket capacity below `MIN_BUCKET_SIZE`, or a capacity the node cannot read, MUST fail the transaction; it MUST NOT be priced at any multiplier.
- A slot restored to its original zero value in the same transaction MUST give its state gas back at the price it was charged, as EIP-8037 specifies: first to the frame's regular gas, up to what the frame's state charges spilled onto it, and the rest to the reservoir.
- The protocol's own transactions MUST be priced at `m = 1` whatever their buckets hold (see [The Protocol's Own Transactions](#19-the-protocols-own-transactions)).

At `m = 1` every state charge is the schedule's own number, so, unlike Rex6, a fresh slot or account in a minimum-size bucket is not free: it pays its state gas.

### 7. History Gas

#### Previous behavior

- There is no history ledger.
  Logs pay 10× their standard cost as storage gas, calldata pays storage gas per byte, every transaction pays 39,000 intrinsic storage gas, and deployed code pays 10,000 storage gas per byte.

#### New behavior

History gas prices the bytes a transaction appends to the chain's history, at `COST_PER_HISTORY_BYTE` = 88 gas per byte.
It MUST NOT be scaled by SALT.
The bytes MUST be those of the byte table below, which is also the table the data-size limit counts (see [Resource Limits](#10-resource-limits)), so a record's history bytes are its own data size.

| Item                                      | Bytes                                                  |
| ----------------------------------------- | ------------------------------------------------------ |
| Transaction body                          | `TX_BODY_SIZE` = 310                                   |
| Calldata (a creation's initcode included) | 1 per byte, zero or non-zero                           |
| Access-list address                       | `ACCESS_LIST_ADDRESS_SIZE` = 20                        |
| Access-list storage key                   | `ACCESS_LIST_SLOT_SIZE` = 32                           |
| EIP-7702 authorization                    | `AUTHORIZATION_SIZE` = 101                             |
| Log                                       | `LOG_BASE_SIZE` = 32, plus 32 per topic, plus its data |
| Write record                              | `WRITE_RECORD_SIZE` = 40                               |
| Deployed code                             | 1 per byte                                             |

- `TX_BODY_SIZE` is `TX_BASE_SIZE` = 110 for the envelope plus `TX_FIXED_WRITE_RECORDS` = 5 write records: the sender's account and the four accounts a transaction's fees are credited to (the block beneficiary, the L1 fee vault, the base fee vault and the operator fee vault).
  It is an upper bound, charged whether or not every fee account is written.
- A write record is one account or storage write the transaction keeps (see [Resource Limits](#10-resource-limits)).

When each charge is made:

- The body — `TX_BODY_SIZE`, the calldata, the access list and the authorizations — MUST be charged before execution, as intrinsic state gas: the reservoir pays it first, and it does not count against the execution cap.
  A gas limit that cannot cover it MUST make the transaction invalid (see [Two-Pool Gas](#4-two-pool-gas-eip-8037-replaces-the-dual-gas-model)).
  The body's history gas MUST NOT be reported on the state ledger.
- The records of the applied EIP-7702 authorities, and the record the transaction's own frame makes (its value's recipient or its created account), MUST be charged before the first frame.
  The authorities' records are the transaction's own and MUST be kept when the first frame fails or is stopped; the first frame's record is that frame's, and comes back with it.
- A log, a storage write's record and the record of a `SELFDESTRUCT`'s beneficiary MUST be charged by the opcode, once it completed.
- A `CALL`, `CALLCODE`, `CREATE` or `CREATE2` MUST charge its own frame for the records the frame it starts makes (a value transfer's sender and recipient; a creation's creator nonce and created account), after the gas it forwards is computed, so the forward is not reduced by them.
  A caller that cannot pay MUST halt with out-of-gas, and the frame MUST NOT start.
- Deployed code MUST be charged at the deposit (see [Gas Schedule and Intrinsic Gas](#5-gas-schedule-and-intrinsic-gas)).
- What a frame does not keep — a failed frame's records and logs, a slot written back to its original value — MUST be given back at the price it was charged, to whoever paid it.

A record an opcode commits — a storage write, a log, a `SELFDESTRUCT`'s beneficiary — MUST be checked against the limits before its history is charged, so a record a limit refuses is not charged.
At a frame start the order is the reverse: the calling opcode pays for the records before the frame starts and they are counted, so a caller that cannot pay runs out of gas first, and the charge for a start a limit stops is given back when the stopped frame returns.

These MUST NOT pay history gas:

- an [EIP-7708 transfer log](#14-eip-7708-transfer-logs), which is data size only;
- an Oracle hint's payload, which goes to the oracle service rather than into a block;
- any byte of a deposit transaction, of a system-originated transaction, or of a system call (see [The Protocol's Own Transactions](#19-the-protocols-own-transactions)).
  For these the deposited-code history gas is zero too.

The EIP-7623 floor is still computed and validated.
At these prices it never raises a receipt's gas used, because the history gas of the same bytes is higher; it does bind the figure a block counts towards its execution-gas limit, `max(regular, floor)` (see [Two-Pool Gas](#4-two-pool-gas-eip-8037-replaces-the-dual-gas-model)).

### 8. History Allowance Replaces the Storage Gas Stipend

#### Previous behavior

- A value-transferring internal `CALL` or `CALLCODE` grants its callee a separate allowance of 23,000 gas, drawn only at storage-gas surcharge sites.

#### New behavior

- A value-transferring `CALL` or `CALLCODE` below the transaction's own frame MUST grant the frame it starts a [history allowance](../glossary.md#history-allowance) of `HISTORY_ALLOWANCE_BYTES` = 160 bytes at the cost per history byte (14,080 gas): one three-topic event carrying one word.
- Only a log's history charge in that frame MAY draw on it, before the frame's gas pays the rest.
  A write record MUST NOT draw on it.
- The allowance MUST NOT enter the frame's gas: it cannot pay for computation or state, it is never returned as gas, and what it pays for is on no gas ledger.
  The history bytes it pays for still count as bytes the transaction appended.
- The transfer log of the value that granted the allowance MUST NOT draw on it.

### 9. Gas Forwarding

#### Previous behavior

- A call or creation forwards at most 98/100 of the caller's remaining gas.

#### New behavior

- A call or creation MUST forward regular gas under the inherited [EIP-150](https://eips.ethereum.org/EIPS/eip-150) rule: at most all but one 64th of the caller's remaining regular gas.
- The child inherits the caller's reservoir whole (see [Two-Pool Gas](#4-two-pool-gas-eip-8037-replaces-the-dual-gas-model)).

### 10. Resource Limits

#### Previous behavior

- Four runtime transaction-level limits: compute gas `TX_COMPUTE_GAS_LIMIT` = 200,000,000, data size `TX_DATA_LIMIT` = 13,107,200 bytes, KV updates `TX_KV_UPDATE_LIMIT` = 500,000, state growth `TX_STATE_GROWTH_LIMIT` = 1,000 new accounts and slots.
- Every call frame gets 98/100 of its parent's remaining budget in each of the four dimensions.
- Data size and KV updates are counted by Rex6's own per-operation rules; the post-execution fee-reward credits are recorded after the transaction's result is final, and cannot change it.
- A transaction crossing a limit halts; a frame crossing its budget reverts alone.

#### New behavior

Three dimensions are limited per transaction; compute is not one of them (see [Two-Pool Gas](#4-two-pool-gas-eip-8037-replaces-the-dual-gas-model)).

| Dimension  | Counted in                               | Transaction limit                                          | Frame budget |
| ---------- | ---------------------------------------- | ---------------------------------------------------------- | ------------ |
| Data size  | Bytes of the byte table                  | `TX_DATA_LIMIT` = 13,107,200 unless the chain sets another | Yes          |
| KV updates | Write records                            | Unlimited unless the chain sets one                        | Yes          |
| State gas  | EIP-8037 state gas the transaction holds | Unlimited unless the chain sets one                        | No           |

- **Data size** counts the byte table of [History Gas](#7-history-gas), plus two items that pay no history: an EIP-7708 transfer log, counted as `TRANSFER_LOG_SIZE` = 160 bytes (a `LOG3` of one word), and an Oracle hint's payload, counted by its length before it is forwarded.
  It counts the body before any frame; the applied authorities' records before the first frame; a frame start's records and transfer log when the frame starts; a storage write's record, a log, and a `SELFDESTRUCT`'s beneficiary record and transfer log once the opcode completed; deployed code once every deposit charge is made and before the creation commits; a hint before it is forwarded, unless it would cross the transaction's limit, in which case it is neither counted nor forwarded (see [Oracle Storage Reads and Hints](#16-oracle-storage-reads-and-hints)).
  A deposit's data size is counted like any transaction's, and a deposit that fails keeps what it counted: its body and the hints it forwarded.
- **Write records.**
  One record per account or storage write the transaction keeps: a slot's first change in the transaction (taken back when the slot is written back to its original value); a value transfer's sender and recipient; a creation's creator nonce and created account; a `SELFDESTRUCT` that moves a balance to another account (its beneficiary); an applied EIP-7702 authority; the transaction's value recipient or created account.
  The sender's own account and the four fee accounts are part of the body and MUST NOT be counted as records; a frame running as the sender records nothing for the sender's account.
  Records MUST be deduplicated per frame: a frame records an account once.
  A successful frame's records and bytes MUST merge into its caller's; a failed frame's MUST be discarded, except that a creator's nonce record survives its creation's failure once the nonce was bumped, and lands on the creator.
  The KV count is the write-record count; every record weighs `WRITE_RECORD_SIZE` bytes of data size, counted and taken back with it, so `KV × 40 ≤ data size` at every point.
- **Frame budgets.**
  The transaction's own frame MUST get what the transaction has left once its body and pre-frame records are counted; every child MUST get `FRAME_SHARE_NUMERATOR / FRAME_SHARE_DENOMINATOR` = 98/100 of what its parent has left, rounded down, in each of data size and write records, under an optional per-frame cap.
  A frame that crosses its budget MUST revert with `MegaLimitExceeded(kind, budget)` and its caller continues.
- **State gas** is the net EIP-8037 state gas the transaction holds: what it was charged before the first frame, plus what every frame on the call stack holds, net of refills and of failed frames.
  It MUST be checked wherever state gas is charged, after the charge: a charge the frame cannot pay is an out-of-gas whatever the limit.
  The sites are the applied authorities and the account a deposit-like transaction creates for its caller, before the first frame (the authorities are taken back on a crossing), a fresh slot, a `SELFDESTRUCT`'s new beneficiary, a new account a frame's start adds — the first frame's recipient or created account, and the account a `CALL`, `CREATE` or `CREATE2` adds — (held once the frame is decided: a frame refused on its caller's account, or answered with a failure, gives the charge back and is not held for it), and deployed code (at the deposit, before the creation commits).
  Because it is a limit on gas, a slot or an account in a bucket `m` times the minimum reaches it `m` times sooner.
- **Order at one site.**
  Where the state gas and a count cross at the same site, the state gas is the dimension reported; between the counts, data size is checked before write records, and a transaction limit before a frame budget.
  A frame start is the exception: its records and transfer log are held before the frame is built, and the state gas charged upfront for the account it adds — by its opcode, or, for the first frame, by EIP-2780 — only once the frame is decided; a start the records stop adds no account, so its upfront state gas is given back rather than held, and the data-size or KV stop is the one reported.
- **A frame start** is counted before the frame is built.
  A start the caller's account refuses — a value the caller cannot fund, a creation whose creator nonce cannot be bumped — MUST count nothing, be charged nothing, and be stopped by no limit.
  A creation onto an occupied address is counted, and a crossing it causes stops it where it would otherwise have failed on the collision.
- The per-transaction limits and the per-frame caps are protocol values: the chain configuration carries them as parameters of the Satin hardfork, together with the detention caps and the block limits, and a node MUST hold every transaction to the values the chain configures, never to values it is handed with a block.
  The values above are the defaults.
  A chain configuration that activates Satin without these parameters MUST be refused when it is loaded, and so MUST one that sets a limit to zero, a transaction data-size limit below `TX_BODY_SIZE`, or a detention cap no transaction's compute reaches (see [Gas Detention on Withheld Gas](#12-gas-detention-on-withheld-gas)).
  The 98/100 share is not a parameter.
  At the default data-size limit a transaction keeps fewer than 327,680 write records (13,107,200 / 40), so a KV limit binds only below that.

### 11. The Revert-Class Stop

#### Previous behavior

- A transaction crossing a runtime transaction-level limit halts: it produces a failed receipt, its remaining gas is preserved and refunded to the sender, and it is included in the block.
- A frame crossing its budget reverts with `MegaLimitExceeded(uint8 kind, uint64 limit)` and its parent continues.

#### New behavior

Every transaction-level limit — data size, KV updates, state gas, and gas detention's compute limit — MUST stop the transaction with a revert, never a halt:

1. The frame that crosses the limit MUST revert with `MegaLimitExceeded(uint8 kind, uint64 limit)` as its output.
2. From then on no frame runs another instruction: a caller receiving that result MUST return the same revert instead of resuming, a frame about to start MUST be answered with it, and every result returned upward MUST be rewritten to it, whatever produced the result.
3. The transaction MUST settle like an EIP-8037 revert: every write and log its frames made MUST be reverted, the value its first frame carried and that frame's write record included; the sender pays the intrinsic gas, the body's history gas, the charges of what it keeps from before the first frame (below), and what ran; the unspent regular gas and the reservoir return to the sender.
4. The transaction is included with a failed receipt, and its usage counts towards the block's counters.

What was applied before the first frame is not the frames' doing, and a later stop MUST NOT revert it, as a revert of the first frame does not:

- the sender's nonce and the fees it pays; for a deposit, its mint and the caller account it created, with that account's state gas;
- the EIP-7702 authorizations admitted before the first frame: their authorities' nonce and delegation writes, their state gas, their write records, and the history gas those records cost.

An Oracle hint a frame forwarded before the stop is not the frames' state either: its payload has reached the oracle service, so it MUST stay counted as data size through the stop.
A hint that would itself cross the data-size limit is neither forwarded nor counted (see [Oracle Storage Reads and Hints](#16-oracle-storage-reads-and-hints)).

A limit is enforced before the writes it guards:

- A frame whose start would cross a limit MUST be answered with the stop before it is built, so no value moves; a stopped creation still bumps its creator's nonce.
  The one exception is the state gas charged upfront for the account the frame adds — by the calling opcode, or, for the first frame, by EIP-2780: it is held once the frame is decided, so a frame built by then returns the stop before its first instruction, a frame answered without running is rewritten to the stop, and what the start moved is reverted with the frames the stop reverts — for the first frame, which no frame's revert follows, with its answer.
- A body over the data-size limit MUST stop the transaction before it runs: no authorization is applied, no record made outside a frame is charged, the first frame's start is charged nothing, and the first frame is answered with the stop.
- EIP-7702 authorities whose state gas or records would cross a limit MUST be taken back before the first frame, with their writes and the state gas applying them charged; the first frame is then answered with the stop.
  Authorities admitted there are kept through a later stop, as above.

A frame budget MUST revert its frame alone, with the same revert data, and its caller continues.
A real out-of-gas, a precompile given less than its price, and an invalid opcode still halt and consume the frame's gas.

The revert data:

| `kind` | Dimension               | `limit`                                                                                     |
| ------ | ----------------------- | ------------------------------------------------------------------------------------------- |
| 0      | Data size               | The limit or frame budget crossed, in bytes                                                 |
| 1      | KV updates              | The limit or frame budget crossed, in write records                                         |
| 2      | Compute (gas detention) | The compute the transaction may reach: its compute at the binding read plus the cap, in gas |
| 3      | State growth            | The transaction's state-gas limit, in gas                                                   |

The discriminants are Rex6's, so a decoder of the Rex6 revert data decodes Satin's.
Kind 3 changes unit: Rex6 reported it in new accounts and slots, Satin in state gas.
A contract can revert with the same bytes, so the revert data alone does not identify a stop.

### 12. Gas Detention on Withheld Gas

#### Previous behavior

- A volatile read sets `effective_detained_limit = current_compute_gas_used + cap` on the compute-gas ledger; the most restrictive read binds.
- Crossing the detained limit halts the transaction with `VolatileDataAccessOutOfGas`; the detained gas is refunded at the end of the transaction.
- `BLOBHASH` is volatile; a transaction whose sender is the system address is exempt from oracle detention, and a system-originated transaction from detention.

#### New behavior

- **What is volatile.**
  The block environment (`NUMBER`, `TIMESTAMP`, `COINBASE`, `PREVRANDAO`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `SLOTNUM`, `BLOCKHASH`); the block beneficiary's account, through `BALANCE`, `SELFBALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, the four call opcodes and the EIP-7702 delegate they follow, `SELFDESTRUCT` at either end, a sender or recipient that is the beneficiary, a recipient that delegates to it, an applied EIP-7702 authority that is the beneficiary, and a keyless deployment's signer that is the beneficiary; and the Oracle's storage, through `SLOAD` in the Oracle's own frame.
  `BLOBHASH` MUST NOT be volatile: it reads the transaction's own blob hashes.
  A read MUST be marked where the value is loaded, and only once the opcode completed; a load that fails, and a read whose opcode then fails, mark nothing.
- **Compute.**
  A transaction's compute is its regular ledger — the running frame's regular gas spent, plus every suspended caller's, each less its child's whole gas limit — less the state and history gas that spilled onto regular gas, and less what halting frames burned.
  A child's gas limit includes a value call's 2,300 stipend, which its caller did not spend: what the callee spends of it is not compute, and what the callee leaves of it lowers the caller's spent regular gas when it returns.
  What a halting frame burns is not compute, with one exception: when an opcode's static gas cannot be paid, what the halting frame had left counts as compute.
- **The limit.**
  A read MUST set `limit = compute_at_read + cap`, with `cap` = `BLOCK_ENV_ACCESS_COMPUTE_GAS` = 20,000,000 for the block environment and the beneficiary and `ORACLE_ACCESS_COMPUTE_GAS` = 20,000,000 for the Oracle; the limit only goes down, so the most restrictive read binds.
  The caps are protocol values the chain configuration carries with the other limits (see [Resource Limits](#10-resource-limits)), 20,000,000 each by default.
  A chain's caps MUST be below 199,987,900, the most compute a transaction can spend, so that they can stop one: the execution cap less EIP-2780's base cost of 12,000, which every transaction pays, and the 100 of a warm account access, the least a frame pays for the code it runs.
- **Withheld gas.**
  Once a limit is set, every frame's regular gas MUST be split into a spendable part, held at what the limit leaves the transaction (`limit − compute`), and a withheld part, the rest.
  The split MUST be applied when the read's opcode completes, when a frame starts or resumes, and after an `SSTORE` (whose restore of a slot can refill regular gas).
  Only a regular charge MUST be limited to the spendable part.
  Every other reader of the frame's gas MUST see both parts: `GAS`, the 63/64 forward and the clamp of an explicit call gas, the `SSTORE` minimum-gas check, the checks that decide whether a cold load is attempted, the gas a child returns, and the refund at the end of the transaction.
  A forward and a spill of state or history gas MUST draw the withheld part first.
  So a transaction that read volatile data runs exactly as it would without the read until a regular charge needs the withheld part.
- **The crossing.**
  A regular charge that the spendable part cannot pay but the frame's whole regular gas can is the crossing: the charge MUST NOT be made, the frame's gas MUST be put back to what it was before the charge, and the transaction MUST be stopped with `MegaLimitExceeded(2, limit)` (see [The Revert-Class Stop](#11-the-revert-class-stop)).
  The transaction is billed what it spent before that charge, less than one charge short of the limit, and the sender gets back everything the frames had, the withheld part included.
  A regular charge beyond the frame's whole regular gas, and every other out-of-gas, MUST halt as it would without the read.
- **Precompiles.**
  A call to a precompile forwarded more regular gas than the frame's allowance MUST be decided from the precompile's price before it runs: priced within the allowance, it runs on its whole forward; priced past the allowance but within the forward, it MUST NOT run and is the crossing; priced past its whole forward, it runs on the forward and fails, as without the read.
  Every precompile of the Satin set has a price; an input over a size limit is priced at zero, since it is refused before any gas check.
  An input priced between the allowance and the forward that would fail a check the precompile makes after its gas check is the crossing too.
  A call refused on its caller's account runs no precompile and is answered as without the read.
- **Interceptor answers.**
  A system contract's intercepted answer that spent more regular gas than the frame's allowance MUST be the crossing.
- **Refused reads.**
  See [Volatile-Data Access Control](#13-volatile-data-access-control-and-remainingcomputegas).
- **Exemptions.**
  A system-originated transaction and a system call MUST NOT be detained, whatever they read (see [The Protocol's Own Transactions](#19-the-protocols-own-transactions)).

{% hint style="info" %}
A precompile a node adds or replaces is outside the Satin set and outside this specification.
The reference implementation has no price for one, so it runs such a precompile on the allowance and treats running out of it as the crossing.
{% endhint %}

### 13. Volatile-Data Access Control and `remainingComputeGas()`

#### Previous behavior

- A read refused by `disableVolatileDataAccess()` reverts the frame immediately with `VolatileDataAccessDisabled(VolatileDataAccessType)`; `BLOCKHASH` and `BLOBHASH` are refused before their operands are checked.
- `MegaLimitControl.remainingComputeGas()` answers from a separate compute-gas ledger with per-frame budgets of 98/100 of the caller's remaining compute, and can exceed the caller's gas.

#### New behavior

- `MegaAccessControl`'s interface, errors, and the scope of `disableVolatileDataAccess()`, `enableVolatileDataAccess()` and `isVolatileDataAccessDisabled()` are unchanged.
- A read is refused where the opcode loads the value, after it read its operands: an opcode that fails on its operands (a stack underflow, for example) fails as it would with access on.
- A refused read MUST revert the frame with `VolatileDataAccessDisabled(accessType)` on the gas it had after the opcode's static gas: the refused read pays its static gas and nothing else.
- The error's argument MUST be ABI-encoded as a `uint8`:

  | `accessType` | Read                            |
  | ------------ | ------------------------------- |
  | 0            | `NUMBER`                        |
  | 1            | `TIMESTAMP`                     |
  | 2            | `COINBASE`                      |
  | 4            | `GASLIMIT`                      |
  | 5            | `BASEFEE`                       |
  | 6            | `PREVRANDAO` (`0x44`)           |
  | 7            | `BLOCKHASH`                     |
  | 8            | `BLOBBASEFEE`                   |
  | 10           | The block beneficiary's account |
  | 11           | The Oracle's storage            |
  | 12           | `SLOTNUM`                       |

  Values 3 (`Difficulty`) and 9 (`BlobHash`) are not produced.
  Value 12 is outside the contract's `VolatileDataAccessType` enum, whose ABI decoder reverts on it: a handler MUST decode the argument as a `uint8`.
  The system contracts' bytecode is unchanged.

- `remainingComputeGas()` MUST answer the lesser of two figures, read when the call reaches the interceptor: the calling frame's own regular gas with the gas the call forwarded counted back, and, once a read of volatile data set a limit, what the limit leaves the transaction.
  It is regular gas only: the reservoir is not in it, so a transaction above the execution cap hears at most the cap's share.
  A transaction that calls the contract directly hears its own frame's regular gas, or less when it is detained from its start (its sender is the block beneficiary).

### 14. EIP-7708 Transfer Logs

#### Previous behavior

- A value transfer emits no log.

#### New behavior

A node MUST apply [EIP-7708](https://eips.ethereum.org/EIPS/eip-7708):

- Every movement of value to another account — the transaction's value, a value `CALL`, a creation's endowment, a `SELFDESTRUCT` that moves a balance to another account — MUST emit `Transfer(address indexed from, address indexed to, uint256 value)` from `0xfffffffffffffffffffffffffffffffffffffffe`, with topic `0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef`.
- The log MUST appear in the receipt in execution order among the contracts' own logs, and a failed frame's logs are discarded with the rest of its logs.
- A `CALLCODE`, a call to the caller itself, and a transaction's value sent to its own sender move nothing to another account and MUST NOT log.
- A deposit's value is logged; its mint is not.
- A `SELFDESTRUCT` to itself of an account created in the same transaction burns the balance without a log.
- A transfer log is data size (`TRANSFER_LOG_SIZE` = 160 bytes), counted where the value moves: with the start of the frame that moves it, before any value moves, or with the `SELFDESTRUCT` that moves it.
  It is not a write record, pays no history gas, and does not draw on a history allowance.
- A frame start that the caller's account refuses moves nothing and logs nothing.

### 15. Native Keyless Deployment

#### Previous behavior

- A top-level `keylessDeploy(bytes,uint256)` call runs the signed creation in a sandbox: a separate, fee-free transaction with its own resource trackers, capped to the parent's remaining budgets, whose state is merged into the parent afterwards.
- Inside the sandbox `ORIGIN` is the signer and `GASPRICE` is 0.
- A transaction-level limit crossed after the sandbox ran halts the outer call with `OutOfGas` and merges nothing; a sandbox preflight failure reverts with `ParentBudgetExceeded`; a failed read reverts with `InternalError()`.
- `gasUsed` is the sandbox transaction's gas used, its intrinsic gas included.

#### New behavior

A `keylessDeploy(bytes,uint256)` call that is the transaction's own frame (depth 0, to `KEYLESS_DEPLOY_ADDRESS` = `0x6342000000000000000000000000000000000003`) MUST deploy the carried creation as an ordinary creation frame, a child of the call; no code runs in the call's frame itself.
A `keylessDeploy` call a contract makes MUST run the contract's bytecode: without value it reverts with `NotIntercepted()`, and with value it reverts with empty data, the method not being payable.

The call MUST make these charges and checks, in this order.
A rule's refusal reverts the call with the error named: the state and history gas the call was charged are given back, and the regular gas it spent stays spent.
An out-of-gas step halts the call, consuming its regular gas.

| Step | Check or charge                                                                                                                                                                                                                                                                   | Refusal                                                          |
| ---- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| 1    | Charge `KEYLESS_DEPLOY_OVERHEAD_GAS` = 100,000 regular gas                                                                                                                                                                                                                        | Out-of-gas                                                       |
| 2    | The call carries no value                                                                                                                                                                                                                                                         | `NoEtherTransfer()`, before the frame is built: no value moves   |
| 3    | The arguments and the carried transaction decode: a signed legacy creation, no chain id, no trailing bytes                                                                                                                                                                        | `MalformedEncoding()`, `NotContractCreation()`, `NotPreEIP155()` |
| 4    | The carried transaction's nonce is 0                                                                                                                                                                                                                                              | `NonZeroTxNonce(uint64)`                                         |
| 5    | The initcode is within `MAX_INITCODE_SIZE`                                                                                                                                                                                                                                        | `InitCodeTooLarge(uint64,uint64)`                                |
| 6    | `gasLimitOverride` covers the signed gas limit                                                                                                                                                                                                                                    | `GasLimitTooLow(uint64,uint64)`                                  |
| 7    | The signer can be recovered                                                                                                                                                                                                                                                       | `InvalidSignature()`                                             |
| 8    | The signer's nonce is at most 1                                                                                                                                                                                                                                                   | `SignerNonceTooHigh(uint64)`                                     |
| 9    | Unless EIP-3607 is disabled, the signer has no code other than an EIP-7702 delegation                                                                                                                                                                                             | `SignerHasCode()`                                                |
| 10   | If the signer's account is empty (EIP-161), charge its new-account state gas (`183,600 × m`)                                                                                                                                                                                      | Out-of-gas                                                       |
| 11   | `min(gasLimitOverride, remaining gas)` still covers the signed gas limit                                                                                                                                                                                                          | `GasLimitTooLow(uint64,uint64)`                                  |
| 12   | The deploy address `keccak256(rlp([signer, 0]))[12:]` holds no code                                                                                                                                                                                                               | `ContractAlreadyExists()`                                        |
| 13   | The signer's balance covers the carried value                                                                                                                                                                                                                                     | `InsufficientBalance()`                                          |
| 14   | Charge what a `CREATE` charges its frame: 32,000 plus EIP-3860's initcode cost, the created account's state gas if the deploy address is empty, and the history gas of the created account's record and, unless the signer is the transaction's sender, the signer's nonce record | Out-of-gas                                                       |
| 15   | `min(gasLimitOverride, remaining gas)` still covers the signed gas limit                                                                                                                                                                                                          | `GasLimitTooLow(uint64,uint64)`                                  |

- Steps 1 to 13 keep the legacy engine's order, so a call several rules refuse reports the Rex6 error.
- The deploy-address read of step 12 MUST NOT warm the deploy address and MUST NOT load its code.
- A signer that is the block beneficiary MUST be detained as a read of the beneficiary's account.
- The creation MUST then start as a creation by the signer at the deploy address, one level below the call, forwarding `min(gasLimitOverride, remaining gas)`; the forward draws the call's withheld gas first (see [Gas Detention](#12-gas-detention-on-withheld-gas)).
  From there it is an ordinary creation frame: priced, limited, detained and logged as one.
  `ORIGIN` and `GASPRICE` in the initcode are the transaction's own.
- **The signer's nonce.**
  The creation bumps the signer's nonce as any creation bumps its creator's.
  A deployment from nonce 0 MUST keep the bump.
  From nonce 1, when the creation's bump is the last nonce change the deployment made, the node MUST take the bump back, whether the deployment succeeded or failed.
  The bump's write record and that record's history gas go back with it, unless the creation succeeded and moved value out of the signer, whose account then keeps a write of its own.
  A signer whose own code spent a nonce during the creation that survives it keeps every bump.
- **The answer.**
  The call MUST succeed with `(gasUsed, deployedAddress, errorData)`: the deploy address and empty `errorData` when the deploy address holds code afterwards; otherwise the zero address and `ExecutionReverted(uint64 gasUsed, bytes output)`, `ExecutionHalted(uint64 gasUsed)` or `EmptyCodeDeployed(uint64 gasUsed)` — the last also for a constructor that destroyed its own account.
  `gasUsed` MUST be what the creation spent from both pools, before refunds, with no intrinsic gas.
- **Limits.**
  A frame budget the creation crosses reverts the creation, and the call answers `ExecutionReverted` with the stop's revert data.
  A transaction-level limit stops the transaction: the call reverts with the stop, and the deployment — the signer's nonce included — is taken back.
  When the signer's nonce record, which survives a failed creation, takes the call over its own frame budget, the call reverts with that stop and the deployment, the nonce included, is taken back without stopping the transaction.
- Of the contract's errors, `ParentBudgetExceeded(uint8,uint64,uint64)`, `InvalidTransaction()`, `InsufficientComputeGas(uint64,uint64)` and `InternalError()` MUST NOT be produced: a failed database read or SALT lookup fails the transaction with its cause.
  `AddressMismatch()` and `NoContractCreated()` remain as answers to a creation reported at another address or at none, which no deployment reaches.
- The `KeylessDeploy` contract, its interface and its errors are unchanged.

### 16. Oracle Storage Reads and Hints

#### Previous behavior

- An `SLOAD` of the Oracle's storage is always charged as a cold access and triggers oracle detention.
- A `sendHint` is forwarded to the oracle service when the call is forwarded gas, its payload decodes, it stays within the data-size limit, and the calling frame's volatile-data access is on; its payload counts as data size.

#### New behavior

- An `SLOAD` in the Oracle's own frame MUST load the slot from state, then ask the oracle service, and answer the service's value when it gives one and the loaded value otherwise.
- Every such read MUST be charged as a cold access (2,100 gas), however often the slot was read, and the slot MUST be loaded whichever source answers, so a later write finds it warm and a stateless witness carries it.
- A frame whose regular gas, its withheld part included, cannot pay the cold access MUST read nothing and ask nothing; a refused read asks nothing.
- A `sendHint` MUST reach the service synchronously, when it is called, and only when the call is forwarded gas, carries no value, and comes from a frame whose volatile-data access is on.
  A hint that fails any of those conditions is dropped and counts nothing; the call runs the Oracle's bytecode either way.
  The value condition is new.
- An admitted hint MUST count the call's whole input length as data size on the transaction, before the input is decoded, and pays no history gas.
  Every admitted input stays counted, whatever the calling frame or the transaction does afterwards — a revert, a halt, a later stop, a failed deposit — and that includes an input that fails to decode, which reaches no service.
  Two reasons keep the count, each on its own: a payload forwarded to the service cannot be taken back, and admitting an input and decoding it is work the node has done whether or not the input decodes.
- A hint whose input length would take the transaction's data size over its data-size limit MUST NOT be forwarded and MUST NOT be counted.
  The limit stops the transaction, and the stop reports the data size the hint would have reached.
  The data size the stopped transaction keeps, which the block's data-size budget counts, holds none of the hint's bytes: the payload never left the node, so it is taken back as a log that crosses the limit is.
  The check is against the transaction's limit alone, because a hint's bytes are on no frame's budget.

### 17. The Live System Address

#### Previous behavior

- The system address is resolved per block from `SequencerRegistry.currentSystemAddress()` after the pre-block changes commit.
- A Mega System Transaction is a legacy transaction from that address to a contract on `MEGA_SYSTEM_TX_WHITELIST`.
- A transaction from the system address without that shape is refused.

#### New behavior

- A transaction MUST be tested for the system shape on its own fields first: type `0x0`, a call (not a creation) to an address on `MEGA_SYSTEM_TX_WHITELIST` (the Oracle).
- Only a transaction with that shape MUST read the registry: when the registry holds the code this spec deploys, the node reads `_currentSystemAddress` from the transaction's own state, cold, without the registry's code, and compares it with the caller.
  The registry account (without code) and that slot are part of the transaction's state and witness, not the pre-block witness.
- A registry that is absent, holds other code, or names the zero address MUST make no transaction a Mega System Transaction.
- A transaction of any other shape reads nothing and is never a Mega System Transaction, whoever sent it.
  A transaction from the system address without the shape is an ordinary transaction, validated and charged as a user's.
  The Oracle authorizes writes on the sender alone, so such a transaction to the Oracle succeeds as a user transaction: it pays fees, is held to every per-transaction limit, is detained, is priced by bucket, and pays history gas.
- A Mega System Transaction is validated (chain id, nonce, EIP-3607) and promoted to a deposit exactly as in Rex6.
- Because a role change can only be scheduled for a later block and is applied by that block's pre-block call, every transaction of a block reads the address the block's pre-block step left.

### 18. System Calls and Pre-Block Calls

#### Previous behavior

- The EIP-2935, EIP-4788 and `SequencerRegistry.applyPendingChanges()` pre-block calls run on `max(block_gas_limit, 30,000,000)` gas, fail-closed.
- `applyPendingChanges()` runs before the registry deploy step, which from Rex6 upgrades a version 1.0.0 registry in place.

#### New behavior

- **The split.**
  A system call MUST run on at most `SYSTEM_CALL_REGULAR_GAS_LIMIT` = 30,000,000 of regular gas; the rest of its gas limit is its state-gas reservoir, as EIP-8037 specifies for system calls.
  `GAS` inside a system call reads at most 30,000,000.
  A transaction is not a system call: every transaction's gas is split by the execution cap, a Mega System Transaction's included.
- **The pre-block budget.**
  Each pre-block call MUST run from `0xfffffffffffffffffffffffffffffffffffffffe` on `max(block_gas_limit, 30,000,000)`; what the budget adds above 30,000,000 is reservoir.
- **The pre-block sequence.**
  Before its transactions, a block MUST, in order:
  1. make the EIP-2935 call;
  2. make the EIP-4788 call (neither call runs in the genesis block, and a block without a parent beacon block root is invalid, as EIP-2935 and EIP-4788 define);
  3. deploy the six MegaETH system contracts in address order — Oracle, High-Precision Timestamp, KeylessDeploy, MegaAccessControl, MegaLimitControl, SequencerRegistry — and then the [EIP-7997](https://eips.ethereum.org/EIPS/eip-7997) `CREATE2` factory at `0x4e59b44847b379578588920ca78fbf26c0b4956c` with nonce 1;
  4. read whether the `SequencerRegistry` has a role change due in the block, and if it has, call `applyPendingChanges()`;
  5. read the L1 block contract's account and its five fee slots, as [the stateless witness](#22-the-stateless-witness) lays out.
- **Deploys.**
  Each deploy is idempotent: an address already holding the expected code is only read (its nonce kept).
  An absent account, or one with empty code and nonce 0, MUST get the code, nonce 1 and the registry's seed slots; it keeps any balance it has, and any storage it had is cleared.
  There is one version of each contract (the Oracle and the SequencerRegistry at version 2.0.0): an address holding other code, an address with empty code and a non-zero nonce, and a factory with the expected code and nonce 0 MUST make the block invalid.
- **Failure.**
  A pre-block call that does not succeed MUST make the block invalid, before its state is used.
  A database error the node's database declares fatal is an internal error of the node, not a verdict on the block; any other failure of a call — a non-fatal database error, another EVM error, an outcome that is not a success — makes the block invalid.
  A database error in a deploy, in the due-change read or in the read of the L1 block info is always an internal error.
  The L1 block info is read for every block, whatever its transactions, so a node whose database cannot serve the L1 block contract's account, or one of the five slots of a contract the chain holds, cannot execute the block: it fails before the block's first transaction, as an internal error, even for a block of deposits alone, which prices nothing against the contract, or a block whose transactions never read the overhead slot (5).

### 19. The Protocol's Own Transactions

#### Previous behavior

- A system-originated transaction — a pre-block call from `0xfffffffffffffffffffffffffffffffffffffffe`, or a Mega System Transaction before or after its promotion — is charged dynamic storage gas at the minimum bucket, is not halted by the four resource limits or by detention, and has its usage recorded.

#### New behavior

A system-originated transaction and every system call:

- MUST be priced at `m = 1` and MUST NOT read a bucket capacity;
- MUST pay no history gas, the body and deposited code included;
- MUST NOT be stopped by any per-transaction limit or frame budget;
- MUST NOT be detained;
- still have their usage counted.

A system-originated transaction's usage is reported and counts towards the block's counters; the pre-block calls do not count towards the block's counters or limits.

A deposit pays no history gas either, but a user's deposit is not system-originated: it is held to every per-transaction limit, detained, and priced by bucket.

### 20. Block Limits

#### Previous behavior

- Block-level runtime limits on data size (13,107,200), KV updates (500,000) and state growth (1,000); the first transaction that reaches one is included and later candidates are skipped.
- There is no block-level compute-gas limit.

#### New behavior

A block MUST count, per transaction, its regular (as `max(regular, floor)`), state and history gas, its history bytes, its data size and its write records.
Four of those counts can be limited:

| Block limit   | Counted                               | Checked                          | Default          |
| ------------- | ------------------------------------- | -------------------------------- | ---------------- |
| Execution gas | `max(regular, floor)` per transaction | Before the next transaction runs | Unlimited        |
| State gas     | State gas per transaction             | After the next transaction runs  | Unlimited        |
| Data size     | Data-size bytes per transaction       | Before the next transaction runs | 13,107,200 bytes |
| KV updates    | Write records per transaction         | Before the next transaction runs | Unlimited        |

- These are packing budgets: the transaction that reaches a limit MUST be included, and later transactions MUST be skipped — for state gas, only a later transaction that adds state gas.
- A deposit — a transaction whose envelope is a deposit — MUST NOT be refused by any of the four, and MUST count towards all four.
  A Mega System Transaction is a legacy transaction in the block, so at block level it is held to these budgets and to the data-availability limits like any non-deposit transaction.
- History gas has no block limit; the block's history bytes are reported, not limited.
- The block gas limit is unchanged: a transaction's declared gas limit MUST fit in what the block has left.
- The four limits are protocol values the chain configuration carries as parameters of the Satin hardfork, with the per-transaction limits (see [Resource Limits](#10-resource-limits)); the defaults are the table's.
  A node validating a block MUST hold it to the chain's values, as the rules above state them, and to no other value.
- A block builder MAY pack tighter, as building policy: it MAY refuse a transaction for its declared gas limit, its encoded size or its data-availability size, hold the block to an encoded-size or data-availability budget, and hold any of the four limits below the chain's value, never above it.
  A building policy MUST NOT refuse a deposit, which the block derived from L1 must include: none of those per-transaction limits and budgets applies to one.
  A deposit counts towards the block's encoded size and not towards its data-availability size; the block gas limit holds it as it holds every transaction.
  A node validating a block MUST NOT apply a building policy, and a policy never changes what a packed block computes: it only decides which transactions the builder packs.
- Satin sets no limit on a transaction's or a block's encoded size; a block-size rule of the base layer, where a chain adopts one, is the node's to apply and is not one of these limits.

### 21. Transaction and Block Refusals

#### Previous behavior

- Rex6's refusals: the inherited validation errors, the resource-limit halts of [Resource Limits](../evm/resource-limits.md), and the fail-closed pre-block calls.

#### New behavior

A transaction MUST be rejected before inclusion when:

- its gas limit is below its intrinsic regular gas plus the history gas of its body (`CallGasCostMoreThanGasLimit`);
- its floor exceeds its gas limit, or, above the execution cap, its intrinsic regular gas or floor exceeds the cap (`GasFloorMoreThanGasLimit`);
- it is a Mega System Transaction that fails the chain-id, nonce or EIP-3607 check;
- it fails any inherited validation.

A transaction MUST be skipped for the current block, and MAY be included in a later one, when:

- its declared gas limit does not fit in the block's remaining gas;
- its data-availability footprint does not fit in what the block's gas limit has left for footprints;
- the block has reached the chain's execution-gas, data-size or KV limit;
- the block has reached the chain's state-gas limit and the transaction adds state gas;
- it is not a deposit and the block activates Satin or an Optimism fork from Jovian onward.

A block that contains a transaction it should have skipped is invalid.

A block builder MAY also skip a transaction other than a deposit under its own building policy (see [Block Limits](#20-block-limits)): a per-transaction declared-gas, encoded-size or data-availability limit, a block encoded-size or data-availability budget, or one of the four block limits held below the chain's value.
A transaction over a builder's per-transaction limit never fits that builder's blocks, and the builder MAY drop it.
A block is not invalid for a transaction a building policy would have skipped.

A transaction MUST be included with a failed receipt when it reverts, halts, or is stopped by a limit (see [The Revert-Class Stop](#11-the-revert-class-stop)).
A failed bucket-capacity read or database read MUST fail the transaction with its cause rather than settle it at any price.

A block MUST be invalid when:

- a pre-block call does not succeed (see [System Calls and Pre-Block Calls](#18-system-calls-and-pre-block-calls));
- a system-contract address holds foreign code or has empty code and a used nonce, or the factory's address holds the factory's code at nonce 0;
- it contains a transaction the rules above skip.

### 22. The Stateless Witness

#### Previous behavior

- Block execution exported the SALT bucket ids of one cache, reset once per block, and the buckets a keyless deployment's sandbox asked about were never merged into it; the block hashes a block read were whatever its state cache held, which may include hashes an earlier block read.
- The node collected the pre-block states through a state hook it installed on block execution.
- The L1 block info a block's transactions were priced against was read on the block's state and landed in no state a node collects.
- The oracle service's answers were known only to the node's own service, which sees every candidate the builder executes, included or not.

#### New behavior

A stateless validator re-executes a block from a witness of what the block read.
The witness is defined by what the block's execution returns and exports, and a node building it and a validator consuming it MUST agree on the following.

- **The witness.**
  A block's witness holds:
  - every account and every storage slot the block loads, in a pre-block step or in a transaction the block includes, as the chain held it before the block — an absent account recorded as absent and an empty slot as zero — and the code the chain held for every such account before the block;
  - the hash of every block `BLOCKHASH` read, which the block's execution exports;
  - the capacity of every SALT bucket the block's execution exports;
  - the oracle service's answer to every oracle read the block's included transactions recorded, in block order.
    A transaction loads every account and slot its execution reads from the state: in its validation, in its fee settlement, and in any of its frames, a frame that reverted or halted included.
    A failed frame's writes are taken back, not its reads, and a validator re-executing the transaction makes the same reads in that frame.
    The one exception is the L1 block info the transaction is priced against, which the pre-block phase loads for the whole block (see the L1 block info, below).
    A read that gas or a limit skipped loads nothing (see the reads a validator makes again, below).
    Execution reports what it loaded: a pre-block step in the pre-block state it hands over, and a transaction in its returned state, which names every account and slot the transaction loaded, whether it wrote it or not and whether the frame that loaded it succeeded or not.
    The set is a function of those states and the exports alone, not of how the node caches state: a node that serves a block's execution from a state cache another block filled builds the same witness, because the states name what was loaded whether or not a database was asked for it.
    A state names the keys; the values are the chain's.
    The code in particular is not the code a returned state carries: a transaction that replaces an account's code — an EIP-7702 authority delegated anew — is admitted against the code the chain held, and its returned state carries the code it wrote.
- **What the block reads, and where it lands.**
  A transaction's loads are in its returned state: its sender, its recipient or created address with its code, every account, with its code, and every slot any of its frames loads, the fee recipients, the live system address a transaction of the system shape reads, and a keyless deployment's signer and deploy address.
  The pre-block phase's reads land in the pre-block states, in order: the two EIP calls' states, each system-contract deploy's read-only or created entry, the registry's pending slots the due-change decision read, the `applyPendingChanges()` call's state, and last the L1 block info.
  `BLOCKHASH` reads the chain's history outside the journal and its reads are exported; a SALT bucket's capacity and the oracle service's answer are read outside the database and are exported or recorded.
  The header, the chain configuration — the schedule with the protocol limits and the sequencer registry parameters attached to it — the parent hash, the parent beacon block root, the extra data, the activation flag and the block's slot number are inputs a validator is given beside the witness.
  `SLOTNUM` pushes the slot number the node supplies, and zero for a block that carries none, so a validator MUST be given the slot number the building node supplied, or that it supplied none.
- **The L1 block info.**
  A non-deposit transaction is priced against the L1 block contract (`0x4200000000000000000000000000000000000015`): its L1 base fee slot (1), its Ecotone fee scalars slot (3), its Ecotone blob base fee slot (7), its operator fee scalars slot (8), which also holds the DA footprint gas scalar, and, only when the Ecotone scalars are empty (the blob base fee scalar and the eight bytes of the two scalars zero), its L1 fee overhead slot (5).
  These reads are made on the block's state, not through a transaction's journal, so they land in no transaction's returned state.
  None is made before the block's first non-deposit transaction, and each finds the state the deposits before it left: whether the overhead is read is decided by the scalars those deposits left, not by the scalars the chain held before the block, and a deposit that empties the scalars names no overhead in its returned state.
  The pre-block phase MUST therefore read the contract's account and all five slots, whatever the scalars it reads hold, and record them in a pre-block state as read-only entries, after every other pre-block step; on a chain that does not hold the contract it MUST record the account as not existing and read no slot, the slots of an account the block itself creates not being read from the chain.
  A witness so holds the overhead of a block that never reads it again, and a block cannot be executed on a state that cannot serve one of the five slots, whether or not a transaction would have been priced with it.
  The read changes nothing: the entries are not written, and a transaction finds the same values, or the ones the block's L1 attributes deposit wrote over them, which are in that deposit's own returned state.
- **The reads a validator makes again.**
  A read that gas or a limit skipped is skipped again on replay, so the witness need not carry it: a cold `SLOAD` or account load with less gas than the cold surcharge, a `BLOCKHASH` of the current block or outside the last 256 blocks, an oracle read the frame cannot pay, a read a frame whose volatile-data access is off is refused, a keyless deployment's reads after a rule refused it, and the bucket of a charge a system-originated transaction or a system call makes.
- **The SALT buckets.**
  Block execution exports the set of SALT buckets the SALT environment answered with a valid capacity during the block, emptied when the block starts and accumulated over everything executed for the block.
  A bucket is in the set once it was answered with a valid capacity, whether or not the transaction that asked is in the block: a candidate the builder executed and dropped leaves its buckets in the set, which a validator does not need.
  A lookup that failed is not in the set, whether the environment could not answer it or answered a capacity below `MIN_BUCKET_SIZE`: it fails its transaction, which is in no block, and a validator never makes it.
  A witness MUST prove the capacity of every bucket in the set.
  The set is complete for the transactions the block's own execution ran: a bucket is asked about at a state charge site and nowhere else, and a validator re-executing the block's transactions reaches the same charge sites.
  A node that runs the block's transactions on several executions, or that empties the set between transactions, MUST take the union of what each reports; a transaction executed elsewhere and committed into the block adds nothing to the set of the execution it was committed into.
  The pre-block calls and the system transactions price at the minimum bucket and add none.
- **The block hashes.**
  Block execution exports every hash `BLOCKHASH` was served, emptied when the block starts.
  It is not what a node's state cache holds, which may carry hashes an earlier block read.
- **The oracle.**
  An `SLOAD` in the Oracle's own frame loads the chain's slot, which is in the transaction's returned state and so in the witness, then takes the service's answer over it.
  The answer is in no database.
  The engine records every read a transaction makes through the service, with the answer, in order, on the transaction's outcome; an answer can be none, where the service had no value and the slot's loaded value stood.
  A node MUST take the oracle reads of the transactions it includes from their outcomes, in block order, and nothing of a candidate it executed and dropped.
  A validator MUST do one of the following:
  - be given those records, and answer each read its own execution makes from them, in order; or
  - run no service, and find each answer in the Oracle's slot at its read — the value the journal holds for the slot when the read is made, as the block's earlier included transactions and the reading transaction's own frames left it.
    This reproduces the block only when every answer an included transaction recorded equals the slot's value at that read; an answer of none always does.
    Meeting it is the node's to arrange, by including a transaction that writes every value its service answers into the Oracle's storage before the transaction that reads it.
    It cannot always be arranged: one transaction can be answered two values for one slot — it reads the slot, sends a hint that has the service fetch another value, and reads the slot again — and only the system address writes the Oracle's storage, so a transaction the system address does not send finds one value in the slot at both reads, and no value the slot can hold reproduces both.
    A node that serves validators running no service MUST either give them the included transactions' own records, as the first option has it, or leave out of the block every transaction whose recorded answers the slot cannot hold at its reads.
    A validator without a service that finds another value in the slot computes another block; a transaction that wrote the value but was dropped by the builder wrote nothing the chain holds.
    Hints reach the service alone: they are not in the witness, and change nothing a validator computes except through the answers the service then gives, which the records carry.
- **The check.**
  A block replayed on exactly its witness MUST produce the same receipts, state changes, gas ledgers, logs, block counters and exports as the block's execution, and MUST read nothing the witness does not hold.

## Developer Impact

Gas numbers move for every transaction.
At the minimum bucket: an empty call uses 42,280 gas (15,000 regular and 27,280 history), a transfer to an existing account 51,800, a fresh `SSTORE` 165,826 (97,920 of it state gas), a `LOG1` with 32 bytes of data about 51,750, and a value `CALL` that creates its recipient about 267,240 (183,600 of it state gas); the last two move by a few gas with the code that sets them up.
Calldata, logs and deployed code cost 88 gas per byte of history; a fresh slot or account costs state gas even in a minimum-size bucket, and `m` times that in a bucket `m` times larger.

Gas limits above 200,000,000 behave differently: at most 200,000,000, less the intrinsic regular gas, is available to computation and `GAS`, and the rest can pay only state and history gas.
A contract that checks `gasleft()` against a large constant sees at most the regular budget.

A transaction stopped by a resource limit or by gas detention now reverts rather than halting, with `MegaLimitExceeded(kind, limit)` as its output, and its sender pays only for what ran.
Kind 3 now reports the state-gas limit in gas.
A transaction that reads volatile data is still stopped at its compute at the read plus 20,000,000, but until a regular charge needs gas past that limit, `GAS`, call forwarding and state or history charges see its whole gas, and the stop is a revert.
`VolatileDataAccessDisabled` can carry the value 12 (`SLOTNUM`), which a Solidity handler must decode as a `uint8`.

Every movement of value to another account now emits a `Transfer` log from `0xff…fe`, which indexers see in receipts beside the contracts' own logs; a `CALLCODE`, a call to oneself, a deposit's mint and a self-destruct to oneself emit none.

A `keylessDeploy` constructor now sees the transaction's `ORIGIN` and `GASPRICE` and runs one call level deeper than the same initcode in a creation transaction; `gasUsed` counts no intrinsic gas.
The call pays 183,600 × `m` of state gas for an empty signer's account, where Rex6 charged 25,000 × (`m` − 1) of storage gas.
A signer at nonce 1 stays at 1 whether its deployment succeeds or fails, so failed attempts by anyone cannot make its address undeployable; only a delegated signer whose own code spends a nonce during the creation moves past it.

A `receive()` hook reached through Solidity's `transfer()` can emit one three-topic event with one word of data from the history allowance; a second event must be paid from the 2,300-gas stipend and cannot be.

### Node integrators

A node, a stateless validator and a replay tool must agree on everything below, or they execute different chains.

- **The execution configuration.**
  The spec fixes part of the configuration a transaction runs on, whatever a node supplies: the Satin gas schedule, EIP-8037, EIP-2780 and EIP-7708 on, the 200,000,000 execution cap, and the 512 KiB and 1 MiB code-size limits.
  A node that reads the configuration before it executes anything — gas estimation reads the execution cap — must read these values, not its own.
- **What a transaction pool can check without state.**
  The gas limit must cover the intrinsic regular gas (EIP-2780's base and recipient and value charges, calldata, access list, authorizations, init code) plus the history gas of the transaction's body, and the calldata floor; above the execution cap, the intrinsic regular gas and the floor must fit under the cap.
  The rest of the stateless validation is the base layer's.
  Whether a recipient, a created account or an authority is new is the state's to say: EIP-2780 charges it when the transaction runs, and a gas limit that cannot pay it runs out of gas rather than being refused.
  The history of the write records a transaction's start makes is charged when it runs too: one record for the recipient of its value or the account it creates, and one for each authority that applies.
  So the least gas limit validation admits is not what a transaction needs: at that gas limit a transfer of value to another account runs out of gas on its recipient's record, and one to a new account needs that account's state gas as well.
  A deposit and a Mega System Transaction pay no history gas; a pool that recognizes the latter needs the live system address from the state.
  That address changes only in a block's pre-block step and then holds for the whole block, so a pool reads it again for every block, from the state after that block's pre-block changes.
  An address read from another state treats the old system address's transactions as the protocol's and the new one's as a user's, or the reverse.
  A deposit is never refused: one that fails validation is included as a failed deposit, which bumps its sender's nonce and uses its whole gas limit.
- **Reporting a limit stop.**
  A stop is a revert whose output is `MegaLimitExceeded(kind, limit)`, and a contract can revert with the same bytes, as can a caller that re-raises what a frame budget returned.
  Execution reports whether a transaction-level limit stopped the transaction; a node must read that report, and must not infer a stop from the output alone.
- **The chain configuration.**
  A genesis file's `config` object carries Satin as flat keys beside the other forks': `satinTime`, the activation timestamp, and one key per parameter, the `satin` prefix before the parameter's own name.

  | Key                                  | Parameter                                                                          |
  | ------------------------------------ | ---------------------------------------------------------------------------------- |
  | `satinInitialSystemAddress`          | The `SequencerRegistry`'s initial system address                                   |
  | `satinInitialSequencer`              | Its initial sequencer                                                              |
  | `satinInitialAdmin`                  | Its admin                                                                          |
  | `satinInitialFromBlock`              | The first block its historical lookups are valid for                               |
  | `satinMinRotationDelay`              | Its minimum rotation delay, in blocks                                              |
  | `satinTxDataSizeLimit`               | A transaction's data-size limit                                                    |
  | `satinFrameDataSizeLimit`            | A frame's data-size cap                                                            |
  | `satinTxKvUpdateLimit`               | A transaction's KV-update limit                                                    |
  | `satinFrameKvUpdateLimit`            | A frame's KV-update cap                                                            |
  | `satinTxStateGasLimit`               | A transaction's state-gas limit                                                    |
  | `satinBlockEnvAccessComputeGasLimit` | Detention's cap after a read of the block environment or the beneficiary's account |
  | `satinOracleAccessComputeGasLimit`   | Detention's cap after a read of the Oracle's storage                               |
  | `satinBlockExecutionGasLimit`        | The block's execution-gas limit                                                    |
  | `satinBlockStateGasLimit`            | The block's state-gas limit                                                        |
  | `satinBlockTxsDataLimit`             | The block's data-size limit                                                        |
  | `satinBlockKvUpdateLimit`            | The block's KV-update limit                                                        |

  Once `satinTime` is present every key is required, and none has a default; a key that starts with `satin` and is not one of these is refused, and so is a value its parameter refuses.
  Integers are JSON numbers up to 2^64 − 1, which leaves a limit unlimited; detention's caps must be below 199,987,900 (see [Gas Detention on Withheld Gas](#12-gas-detention-on-withheld-gas)), and no limit may be zero.
  Addresses are hex strings.
  The key format is provisional until a network publishes a genesis file carrying it.

  The Satin keys do not time the forks Satin runs on: Optimism Karst and the Ethereum forks it includes, Osaka among them, are timed by the configuration's own keys for them.
  Every Satin block executes on Karst's rules, so a node must check, when it loads the configuration, that each of those forks is active at `satinTime`, and refuse the configuration otherwise: a Satin block before one of them would be executed on rules the node's own schedule does not apply to it.

## Safety and Compatibility

Satin changes nothing about blocks under earlier specs: a node replaying history resolves each block's spec from its timestamp and applies that spec's rules.
Satin is a restatement rather than a delta, so a Rex6 rule this page does not carry over — the compute-gas limit, the storage-gas formulas, the sandbox, the 98/100 forwarding, the cold first touch of preload-warm addresses — does not apply to Satin blocks.

The six system contracts keep their Rex6 bytecode and ABI; the Satin deploy step refuses, rather than upgrades, any other code at their addresses.
A system-address transaction without the system shape is now executed as a user transaction instead of being refused; only the holder of the system key can send one.

The per-transaction and block limits and the detention caps are protocol values: the chain configuration carries them as parameters of the Satin hardfork, so every node executing a chain holds its blocks to the same ones, and a configuration that activates Satin without them does not load.
By default the KV-update and state-gas limits and the block's execution-gas limit are unlimited, the data-size limits are 13,107,200 bytes, and the detention caps are 20,000,000 each.
The per-transaction gas-limit, encoded-size and data-availability limits are a block builder's policy, not the chain's: a node validating a block does not apply them.

Satin is unstable and not scheduled on any network: its prices, limits and rules may change in either direction until it is frozen.

## References

- [EIP-8037](https://eips.ethereum.org/EIPS/eip-8037) — state gas and the reservoir.
- [EIP-2780](https://eips.ethereum.org/EIPS/eip-2780) — the decomposed intrinsic cost.
- [EIP-7708](https://eips.ethereum.org/EIPS/eip-7708) — transfer logs.
- [EIP-7997](https://eips.ethereum.org/EIPS/eip-7997) — the deterministic `CREATE2` factory.
- [EIP-8024](https://eips.ethereum.org/EIPS/eip-8024) and [EIP-7843](https://eips.ethereum.org/EIPS/eip-7843) — `DUPN`, `SWAPN`, `EXCHANGE` and `SLOTNUM`.
- [Rex6 Network Upgrade](rex6.md) — the behavior every Previous behavior above describes.
- [Dual Gas Model](../evm/dual-gas-model.md), [Resource Limits](../evm/resource-limits.md), [Resource Accounting](../evm/resource-accounting.md), [Gas Detention](../evm/gas-detention.md) — the Rex6 mechanisms Satin replaces.
- [KeylessDeploy](../system-contracts/keyless-deploy.md), [MegaAccessControl](../system-contracts/mega-access-control.md), [MegaLimitControl](../system-contracts/mega-limit-control.md), [Oracle](../system-contracts/oracle.md), [SequencerRegistry](../system-contracts/sequencer-registry.md), [Mega System Transactions](../system-contracts/system-tx.md) — the system contracts.
- [Hardforks and Specs](../hardfork-spec.md) — how specs are versioned, frozen, and activated.
- [mega-evm](https://github.com/megaeth-labs/mega-evm) — reference implementation.
