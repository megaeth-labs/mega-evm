---
description: Capture the nested CALL/CREATE tree with gas usage, return data, and log events.
---

# Call Tracer

The call tracer records the nested tree of CALL, STATICCALL, DELEGATECALL, CALLCODE, and CREATE operations.
It shows which contracts called which, with gas usage and return data at each level.

## Usage

```bash
mega-evme run 0x... --trace --tracer call
```

## Options

| Flag                         | Default | Description                                                |
| ---------------------------- | ------- | ---------------------------------------------------------- |
| `--trace.call.only-top-call` | `false` | Only trace the top-level call — omit nested internal calls |
| `--trace.call.with-log`      | `false` | Include LOG events in the call trace output                |

## Output Format

The call tracer emits a Geth-compatible nested JSON structure.

### Schema

Each call node contains:

| Field     | Type       | Description                                                                      |
| --------- | ---------- | -------------------------------------------------------------------------------- |
| `type`    | string     | Call type: `CALL`, `STATICCALL`, `DELEGATECALL`, `CALLCODE`, `CREATE`, `CREATE2` |
| `from`    | address    | Caller address                                                                   |
| `to`      | address    | Callee address                                                                   |
| `gas`     | hex number | Gas provided to the call                                                         |
| `gasUsed` | hex number | Gas consumed by the call                                                         |
| `input`   | hex bytes  | Calldata                                                                         |
| `output`  | hex bytes  | Return data (present on success)                                                 |
| `value`   | hex number | ETH value transferred (if any)                                                   |
| `error`   | string     | Revert reason (present on failure)                                               |
| `calls`   | array      | Nested child call nodes (recursive)                                              |
| `logs`    | array      | LOG events (only with `--trace.call.with-log`)                                   |

Each log entry (when `--trace.call.with-log` is set):

| Field      | Type       | Description                   |
| ---------- | ---------- | ----------------------------- |
| `address`  | address    | Contract that emitted the log |
| `topics`   | array      | Log topics as hex strings     |
| `data`     | hex bytes  | Log data                      |
| `position` | hex number | Position in the transaction   |

### Example Output

Running `WETH.name()` against MegaETH mainnet with `--tracer call` shows the nested call tree — WETH delegates to L1Block to resolve the chain name:

```json
{
  "from": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
  "gas": "0x97a960",
  "gasUsed": "0x1130f",
  "to": "0x4200000000000000000000000000000000000006",
  "input": "0x06fdde03",
  "output": "0x00000000000000000000000000000000000000000000000000...5772617070656420457468657200...",
  "calls": [
    {
      "from": "0x4200000000000000000000000000000000000006",
      "gas": "0x949600",
      "gasUsed": "0x15c4",
      "to": "0x4200000000000000000000000000000000000015",
      "input": "0xd8444715",
      "output": "0x00000000000000000000000000000000000000000000000000...4574686572...",
      "calls": [
        {
          "from": "0x4200000000000000000000000000000000000015",
          "gas": "0x918a50",
          "gasUsed": "0x247",
          "to": "0xc0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d30015",
          "input": "0xd8444715",
          "output": "0x00000000000000000000000000000000000000000000000000...4574686572...",
          "value": "0x0",
          "type": "DELEGATECALL"
        }
      ],
      "type": "STATICCALL"
    }
  ],
  "value": "0x0",
  "type": "CALL"
}
```

With `--trace.call.with-log`, `WETH.deposit()` shows the emitted `Deposit` event:

```json
{
  "from": "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
  "gas": "0x97a960",
  "gasUsed": "0x171a8",
  "to": "0x4200000000000000000000000000000000000006",
  "input": "0xd0e30db0",
  "logs": [
    {
      "address": "0x4200000000000000000000000000000000000006",
      "topics": [
        "0xe1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c",
        "0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266"
      ],
      "data": "0x00000000000000000000000000000000000000000000000000038d7ea4c68000",
      "position": "0x0"
    }
  ],
  "value": "0x38d7ea4c68000",
  "type": "CALL"
}
```

## Examples

Full call tree:

```bash
# Trace a WETH.balanceOf call against live mainnet state
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --trace --tracer call
```

Top-level call only (useful for simple gas analysis):

```bash
mega-evme tx \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x06fdde03 \
  --trace --tracer call \
  --trace.call.only-top-call
```

Include log events:

```bash
# WETH.deposit() emits a Transfer event — use --with-log to capture it
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0xd0e30db0 \
  --value 1000000000000000 \
  --trace --tracer call \
  --trace.call.with-log
```
