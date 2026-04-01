# Transaction Types

`mega-evme` supports five transaction types via the `--tx-type` flag.
The transaction type determines which additional options are available and how the transaction is encoded.

## Common Transaction Options

These options apply to all transaction types in the `run` and `tx` commands.

| Flag | Default | Aliases | Description |
|------|---------|---------|-------------|
| `--tx-type <TYPE>` | `0` | `--type`, `--ty` | Transaction type number |
| `--gas <AMOUNT>` | `10000000` | `--gas-limit` | Gas limit |
| `--basefee <AMOUNT>` | `0` | `--gas-price`, `--price`, `--base-fee` | Gas price (Legacy) or max fee per gas (EIP-1559) |
| `--priority-fee <AMOUNT>` | N/A | `--priorityfee`, `--tip` | EIP-1559 max priority fee per gas |
| `--sender <ADDRESS>` | `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | `--from` | Transaction sender |
| `--receiver <ADDRESS>` | `0x0000…0000` | `--to` | Transaction receiver |
| `--nonce <NONCE>` | `0` | — | Transaction nonce |
| `--create` | `false` | — | Execute in create mode (deploy contract). Requires explicit `true`. |
| `--value <AMOUNT>` | `0` | — | ETH value in wei (supports `1ether`, `100gwei`, `1000wei` suffixes) |
| `--input <HEX>` | N/A | `--data` | Transaction calldata as hex |
| `--inputfile <PATH>` | N/A | `--datafile`, `--input-file`, `--data-file` | Calldata from file (`-` for stdin) |

## Type 0 — Legacy

The default transaction type.
Uses a single gas price (`--basefee`) for all gas costs.

```bash
mega-evme tx --tx-type 0 \
  --receiver 0x4200000000000000000000000000000000000006 \
  --gas 100000 \
  --basefee 1000000000 \
  --input 0x06fdde03
```

## Type 1 — EIP-2930 (Access List)

Adds an access list to declare which addresses and storage slots the transaction will touch.
Pre-warming the access list reduces gas costs for those accesses from cold to warm.

### Additional Options

| Flag | Default | Aliases | Description |
|------|---------|---------|-------------|
| `--access <ENTRY>` | N/A | `--accesslist`, `--access-list` | Access list entry (repeatable) |

Access list entry format:
- `ADDRESS` — pre-warm an address (no storage keys)
- `ADDRESS:KEY1,KEY2,...` — pre-warm an address and specific storage keys (comma-separated)

```bash
mega-evme tx --tx-type 1 \
  --receiver 0x4200000000000000000000000000000000000006 \
  --access "0x4200000000000000000000000000000000000006" \
  --access "0x28B7E77f82B25B95953825F1E3eA0E36c1c29861:0x0,0x1" \
  --input 0x06fdde03
```

## Type 2 — EIP-1559

Uses a base fee + priority fee model instead of a flat gas price.
Also supports access lists.

```bash
mega-evme tx --tx-type 2 \
  --receiver 0x4200000000000000000000000000000000000006 \
  --basefee 30000000000 \
  --priority-fee 1000000000 \
  --access "0x4200000000000000000000000000000000000006:0x0" \
  --input 0x06fdde03
```

## Type 4 — EIP-7702 (Authorization)

Allows an EOA to temporarily delegate its execution to a contract via authorization tuples.
Also supports access lists.

### Additional Options

| Flag | Default | Aliases | Description |
|------|---------|---------|-------------|
| `--auth <AUTH>` | N/A | `--authorization` | Authorization tuple (repeatable) |

Authorization format: `AUTHORITY:NONCE->DELEGATION`

- `AUTHORITY` — address of the EOA delegating control
- `NONCE` — authorization nonce (decimal or `0x`-prefixed hex)
- `DELEGATION` — address of the contract to delegate to

```bash
# Single authorization — delegate the sender's EOA to Multicall3
mega-evme tx --tx-type 4 \
  --auth "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266:0->0xcA11bde05977b3631167028862bE2a173976CA11" \
  --receiver 0x4200000000000000000000000000000000000006

# Multiple authorizations
mega-evme tx --tx-type 4 \
  --auth "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266:0->0xcA11bde05977b3631167028862bE2a173976CA11" \
  --auth "0x70997970C51812dc3A010C7d01b50e0d17dc79C8:1->0xcA11bde05977b3631167028862bE2a173976CA11" \
  --receiver 0x4200000000000000000000000000000000000006
```

## Type 126 — Deposit (Optimism)

A deposit transaction mints ETH from L1 and optionally executes a call.
These are system-level transactions used by the Optimism bridge.

### Additional Options

| Flag | Default | Aliases | Description |
|------|---------|---------|-------------|
| `--source-hash <HASH>` | N/A | `--sourcehash` | Source hash identifying the deposit origin (B256) |
| `--mint <AMOUNT>` | N/A | — | Amount of ETH to mint to the sender (in wei) |

```bash
mega-evme tx --tx-type 126 \
  --sender 0x4200000000000000000000000000000000000010 \
  --receiver 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  --mint 1000000000000000000 \
  --source-hash 0xabc123...
```
