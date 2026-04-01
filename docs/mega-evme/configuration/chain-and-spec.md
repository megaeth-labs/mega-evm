---
description: Select which MegaETH spec version and chain ID to use.
---

# Chain and Spec Selection

These options control which MegaETH spec and chain ID the EVM uses during execution.
They are available in the `run` and `tx` commands.
The `replay` command auto-detects the spec from the chain ID and block timestamp (see [replay](../commands/replay.md#spec-auto-detection)).

## Options

| Flag              | Default | Aliases     | Description         |
| ----------------- | ------- | ----------- | ------------------- |
| `--spec <SPEC>`   | `Rex4`  | —           | MegaETH spec to use |
| `--chain-id <ID>` | `6342`  | `--chainid` | Chain ID            |

## Available Specs

Spec names are case-sensitive.

| Name          | Description                                                                  |
| ------------- | ---------------------------------------------------------------------------- |
| `Equivalence` | Optimism Isthmus compatibility mode                                          |
| `MiniRex`     | Initial MegaETH execution model with multidimensional gas                    |
| `Rex`         | Revised storage gas economics and gas forwarding                             |
| `Rex1`        | Compute gas limit reset fix                                                  |
| `Rex2`        | SELFDESTRUCT restored (EIP-6780), KeylessDeploy system contract              |
| `Rex3`        | SLOAD-based oracle detention, increased oracle gas limit                     |
| `Rex4`        | Per-call-frame resource budgets, relative gas detention, storage gas stipend |

## Examples

```bash
# Use MiniRex spec
mega-evme run 0x600160005260... --spec MiniRex

# Use Equivalence mode (Optimism Isthmus compatible)
mega-evme tx --spec Equivalence --receiver 0x1234...

# Custom chain ID
mega-evme run 0x600160005260... --chain-id 1
```
