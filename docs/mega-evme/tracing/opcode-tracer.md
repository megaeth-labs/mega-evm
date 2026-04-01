---
description: Log every instruction with gas, stack, memory, and storage at each step.
---

# Opcode Tracer

The opcode tracer records every EVM instruction executed, along with gas costs, stack state, memory, and storage changes.
This is the default tracer when you pass `--trace` without specifying `--tracer`.

## Usage

```bash
mega-evme run 0x60016000526001601ff3 --trace --tracer opcode
```

## Options

These flags control what the opcode tracer captures.
Disabling memory and stack capture significantly reduces trace size for large executions.

| Flag | Default | Description |
|------|---------|-------------|
| `--trace.opcode.disable-memory` | `false` | Omit memory snapshots from each step |
| `--trace.opcode.disable-stack` | `false` | Omit stack snapshots from each step |
| `--trace.opcode.disable-storage` | `false` | Omit storage change records |
| `--trace.opcode.enable-return-data` | `false` | Include return data in each step (off by default for size) |

## Output Format

The opcode tracer emits Geth-compatible JSON.
The top-level object wraps the trace:

### Schema

| Field | Type | Description |
|-------|------|-------------|
| `failed` | bool | Whether the transaction reverted |
| `gas` | number | Total gas used |
| `returnValue` | hex string | Return data from the transaction |
| `structLogs` | array | Array of per-opcode step objects |

Each entry in `structLogs`:

| Field | Type | Description |
|-------|------|-------------|
| `pc` | number | Program counter |
| `op` | string | Opcode name (e.g., `PUSH1`, `SSTORE`) |
| `gas` | number | Gas remaining before this step |
| `gasCost` | number | Gas consumed by this step |
| `depth` | number | Call depth |
| `stack` | array | Stack contents as hex strings (unless disabled) |
| `memory` | array | Memory contents as 32-byte hex chunks (unless disabled) |
| `storage` | object | Storage changes as key-value hex pairs (unless disabled) |
| `returnData` | hex string | Return data at this step (only if enabled) |

### Example Output

Running `mega-evme run 0x60016000526001601ff3 --trace --tracer opcode` produces:

```json
{
  "failed": false,
  "gas": 60018,
  "returnValue": "0x01",
  "structLogs": [
    {
      "pc": 0,
      "op": "PUSH1",
      "gas": 9940000,
      "gasCost": 3,
      "depth": 1,
      "stack": [],
      "memory": []
    },
    {
      "pc": 2,
      "op": "PUSH1",
      "gas": 9939997,
      "gasCost": 3,
      "depth": 1,
      "stack": [
        "0x1"
      ],
      "memory": []
    },
    {
      "pc": 4,
      "op": "MSTORE",
      "gas": 9939994,
      "gasCost": 6,
      "depth": 1,
      "stack": [
        "0x1",
        "0x0"
      ],
      "memory": []
    },
    {
      "pc": 5,
      "op": "PUSH1",
      "gas": 9939988,
      "gasCost": 3,
      "depth": 1,
      "stack": [],
      "memory": [
        "0000000000000000000000000000000000000000000000000000000000000001"
      ]
    },
    {
      "pc": 7,
      "op": "PUSH1",
      "gas": 9939985,
      "gasCost": 3,
      "depth": 1,
      "stack": [
        "0x1"
      ],
      "memory": [
        "0000000000000000000000000000000000000000000000000000000000000001"
      ]
    },
    {
      "pc": 9,
      "op": "RETURN",
      "gas": 9939982,
      "gasCost": 0,
      "depth": 1,
      "stack": [
        "0x1",
        "0x1f"
      ],
      "memory": [
        "0000000000000000000000000000000000000000000000000000000000000001"
      ]
    }
  ]
}
```

## Examples

Minimal trace (no memory or stack, for compact output):

```bash
mega-evme run 0x60016000526001601ff3 \
  --trace --tracer opcode \
  --trace.opcode.disable-memory \
  --trace.opcode.disable-stack
```

Full trace to file with return data:

```bash
mega-evme run 0x60016000526001601ff3 \
  --trace --tracer opcode \
  --trace.opcode.enable-return-data \
  --trace.output trace.json
```
