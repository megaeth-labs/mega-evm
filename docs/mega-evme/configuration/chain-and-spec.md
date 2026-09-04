---
description: Select which MegaETH spec version and chain ID to use.
---

# Chain and Spec Selection

These options control which MegaETH spec and chain ID the EVM uses during execution.
They are available in the `run` and `tx` commands.
The `replay` command auto-detects the spec from the chain ID and block timestamp (see [replay](../commands/replay.md#spec-auto-detection)).

In all three commands a chosen spec defines the whole execution world, not only the opcode and gas rules: the system contracts predeployed before execution, the block-level resource limits, and the EVM semantics all come from that one spec.
For `run` and `tx` there is nothing else it could mean — there is no historical block to contradict it.
For `replay`, [`--override.spec`](../commands/replay.md#--overridespec-spec) makes the same choice explicitly: the block is replayed as if it had run on a chain at the forced spec, so replaying an old block under a newer spec installs predeploys that never existed at that block, and forcing an older spec withholds or downgrades what its own pre-block operations would have deployed.
That is intentional, and it is what makes the answer a coherent what-if rather than a mixture of two worlds.
The override synthesizes the schedule — the pre-block operations, the resource limits, the EVM behavior — and leaves the forked parent state as history recorded it.
A contract that a later fork had already deployed by that block therefore remains in state under a downgrade, reachable as plain on-chain bytecode without its spec-gated interception, because deleting it would fabricate a parent state that never existed on any chain.
Chain-specific configuration is not synthesized along with it: a fork whose parameters the chain has not published (the `SequencerRegistry` seeds, today) cannot be forced, and the run fails up front naming what is missing.

## Options

| Flag              | Default | Aliases     | Description         |
| ----------------- | ------- | ----------- | ------------------- |
| `--spec <SPEC>`   | `Rex7`  | —           | MegaETH spec to use |
| `--chain-id <ID>` | `6342`  | `--chainid` | Chain ID            |

## Available Specs

Spec names are case-sensitive.

| Name          | Description                                                                                                                   |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `Equivalence` | Optimism Isthmus compatibility mode                                                                                           |
| `MiniRex`     | Initial MegaETH execution model with multidimensional gas                                                                     |
| `MiniRex1`    | Alias rung: executes `Equivalence` behavior (mainnet rollback window)                                                         |
| `MiniRex2`    | Alias rung: executes `MiniRex` behavior (mainnet restoration)                                                                 |
| `Rex`         | Revised storage gas economics and gas forwarding                                                                              |
| `Rex1`        | Compute gas limit reset fix                                                                                                   |
| `Rex2`        | SELFDESTRUCT restored (EIP-6780), KeylessDeploy system contract                                                               |
| `Rex3`        | SLOAD-based oracle detention, increased oracle gas limit                                                                      |
| `Rex4`        | Per-call-frame resource budgets, relative gas detention, storage gas stipend                                                  |
| `Rex5`        | SequencerRegistry, dynamic system address (Oracle v2.0.0), storage-gas-stipend separated allowance, resource-accounting fixes |
| `Rex6`        | Unified gas-metering order, consolidated EIP-7702 accounting, system-tx metering exemption                                    |
| `Rex7`        | No behavioral change over `Rex6` yet (**unstable**)                                                                           |

## Examples

```bash
# Use MiniRex spec
mega-evme run 0x600160005260... --spec MiniRex

# Use Equivalence mode (Optimism Isthmus compatible)
mega-evme tx --spec Equivalence --receiver 0x1234...

# Custom chain ID
mega-evme run 0x600160005260... --chain-id 1
```
