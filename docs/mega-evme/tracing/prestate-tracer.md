---
description: Snapshot accessed account state before execution, with optional before/after diff mode.
---

# Pre-State Tracer

The pre-state tracer captures the account state that was accessed during execution.
In diff mode, it shows what changed — before and after values for each touched account and storage slot.

## Usage

```bash
mega-evme run 0x... --trace --tracer pre-state
```

The tracer name accepts both `pre-state` and `prestate`.

## Options

| Flag                               | Default | Aliases                             | Description                                                  |
| ---------------------------------- | ------- | ----------------------------------- | ------------------------------------------------------------ |
| `--trace.prestate.diff-mode`       | `false` | `--trace.pre-state.diff-mode`       | Show state diff (before/after) instead of just the pre-state |
| `--trace.prestate.disable-code`    | `false` | `--trace.pre-state.disable-code`    | Omit contract bytecode from the output                       |
| `--trace.prestate.disable-storage` | `false` | `--trace.pre-state.disable-storage` | Omit storage slots from the output                           |

## Output Format

### Schema

Each account entry in the output contains:

| Field     | Type       | Description                                                                            |
| --------- | ---------- | -------------------------------------------------------------------------------------- |
| `balance` | hex number | Account balance in wei                                                                 |
| `nonce`   | number     | Account nonce (only in diff mode)                                                      |
| `code`    | hex bytes  | Contract bytecode (omitted with `--trace.prestate.disable-code`)                       |
| `storage` | object     | Storage slots as hex key-value pairs (omitted with `--trace.prestate.disable-storage`) |

In default mode, the output is a flat map of addresses to account state.
In diff mode, the output has two top-level keys: `pre` and `post`, each containing a map of addresses.

### Default Mode (pre-state only)

Shows the state of every account accessed during execution, as it existed before the transaction.

Running `mega-evme run 0x60016000526001601ff3 --trace --tracer pre-state` produces:

```json
{
  "0x0000000000000000000000000000000000000000": {
    "balance": "0x0",
    "code": "0x60016000526001601ff3"
  },
  "0x4200000000000000000000000000000000000019": {
    "balance": "0x0"
  },
  "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
    "balance": "0x0"
  }
}
```

Only fields that were actually read appear — accounts with no code show only `balance`.

### Diff Mode

Shows both the `pre` (before) and `post` (after) state for each touched account.
Fields that didn't change are omitted from the diff.

Running `WETH.deposit()` with `--trace.prestate.diff-mode` shows how the deposit modified balances and storage:

```json
{
  "post": {
    "0x4200000000000000000000000000000000000006": {
      "balance": "0x222f824a54e20d8d47b",
      "storage": {
        "0x723077b8a1b173adc35e5f0e7e3662fd1208212cb629f9c128551ea7168da722": "0x38d7ea4c68000"
      }
    },
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
      "balance": "0xddd2934f05f16e7",
      "nonce": 1
    }
  },
  "pre": {
    "0x4200000000000000000000000000000000000006": {
      "balance": "0x222f82117cf7c12547b",
      "code": "0x6080604052..."
    },
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
      "balance": "0xde0b6b3a7640000"
    }
  }
}
```

In this example:

- The sender's balance decreased (paid ETH + gas), and nonce incremented to 1.
- WETH's balance increased by the deposited amount, and a new storage slot was written (the sender's balance mapping).
- The `code` field appears in `pre` but not `post` — it didn't change, so diff mode omits it from `post`.

## Examples

Capture pre-state:

```bash
# Capture pre-state for a WETH.balanceOf call
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --trace --tracer pre-state \
  --trace.output prestate.json
```

Capture state diff (before/after):

```bash
# WETH.deposit() modifies balances — diff mode shows the change
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0xd0e30db0 \
  --value 1000000000000000 \
  --trace --tracer pre-state \
  --trace.prestate.diff-mode \
  --trace.output diff.json
```

Compact diff without code or storage:

```bash
# Replace 0xabc123... with a real transaction hash from MegaETH mainnet
mega-evme replay 0xabc123... \
  --rpc https://mainnet.megaeth.com/rpc \
  --trace --tracer pre-state \
  --trace.prestate.diff-mode \
  --trace.prestate.disable-code \
  --trace.prestate.disable-storage
```
