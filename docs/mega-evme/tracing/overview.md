---
description: Record step-by-step EVM execution with opcode, call, and pre-state tracers.
---

# Execution Tracing

`mega-evme` can record detailed execution traces during EVM runs.
Traces are useful for debugging reverts, analyzing gas consumption, and understanding call flow.

## Enabling Tracing

Add `--trace` to any command to enable tracing:

```bash
mega-evme run 0x60016000526001601ff3 --trace
```

By default, this uses the [opcode tracer](opcode-tracer.md).
Use `--tracer` to pick a different tracer.

## Tracers

| Tracer | `--tracer` value | Description |
|--------|-----------------|-------------|
| [Opcode](opcode-tracer.md) | `opcode` (default) | Step-by-step opcode execution log with gas, stack, memory, and storage |
| [Call](call-tracer.md) | `call` | Nested call tree showing CALL/CREATE hierarchy, gas, and return data |
| [Pre-State](prestate-tracer.md) | `pre-state` (alias: `prestate`) | Account state accessed during execution, with optional diff mode |

## Common Options

These flags apply to all tracers:

| Flag | Default | Description |
|------|---------|-------------|
| `--trace` | `false` | Enable tracing |
| `--trace.output <PATH>` | stdout | Write trace output to a file instead of the console |
| `--tracer <TRACER>` | `opcode` | Select which tracer to use |

## Output

Trace output is JSON.
When `--trace.output` is not set, the trace is printed to stdout after the execution result.
When `--trace.output` is set, the trace is written to the specified file and only the execution result appears on stdout.

```bash
# Trace to file
mega-evme run 0x60016000526001601ff3 \
  --trace --tracer opcode \
  --trace.output trace.json

# Trace to stdout (default)
mega-evme run 0x60016000526001601ff3 --trace
```
