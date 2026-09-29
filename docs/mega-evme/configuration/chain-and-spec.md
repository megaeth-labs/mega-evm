---
description: Select which MegaETH spec version and chain ID to use.
---

# Chain and Spec Selection

These options control which MegaETH spec and chain ID the EVM uses during execution, and so which of the two engines runs it.
They are available in the `run` and `tx` commands.
The `replay` command auto-detects the spec from the chain ID and block timestamp (see [replay](../commands/replay.md#spec-auto-detection)).

## Options

| Flag                             | Default     | Aliases     | Description                                                                               |
| -------------------------------- | ----------- | ----------- | ----------------------------------------------------------------------------------------- |
| `--spec <SPEC>`                  | `Rex6`      | —           | MegaETH spec to use                                                                       |
| `--chain-id <ID>`                | `6342`      | `--chainid` | Chain ID                                                                                  |
| `--override.limits <JSON\|FILE>` | the chain's | —           | Satin only: protocol limits to run under instead, see [Protocol limits](#protocol-limits) |

## Available Specs

Spec names are case-sensitive.

| Name          | Description                                                                                                                   |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `Satin`       | The Satin engine: EIP-8037 state gas, history gas, and the Satin resource limits; not yet activated on any chain              |
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

`Rex7` is refused: it was the legacy line's unstable spec, it never activated on a chain, and Satin supersedes it.

## Engines

Every spec belongs to exactly one engine, so the spec a command runs names the engine it runs on; there is no separate engine flag.

| Specs                   | Engine                                                                      |
| ----------------------- | --------------------------------------------------------------------------- |
| `Satin`                 | The Satin engine                                                            |
| `Equivalence` to `Rex6` | The legacy engine: the released `mega-evme` and `mega-evm` 1.7.1, linked in |

A command on a legacy spec is handed, with its arguments unchanged, to the released 1.7.1 tool: what it prints is what 1.7.1 printed.

### Satin output

On Satin, every output field keeps its legacy name and meaning, and one field is added: `satin`, with what only the Satin engine counts.

| Field                 | Description                                                                                                                   |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `regular_gas`         | Execution gas, before the refund                                                                                              |
| `state_gas`           | EIP-8037 state gas for the state the transaction added, net of refills                                                        |
| `history_gas`         | Gas for the bytes the transaction appended to history, net of refills                                                         |
| `history_bytes`       | The history bytes it appended: its body, one 40-byte record per kept write, its logs but the EIP-7708 transfer logs, its code |
| `reservoir_remaining` | The EIP-8037 reservoir left unspent, which the sender gets back                                                               |
| `floor_gas`           | The EIP-7623 floor the receipt's gas used cannot fall below                                                                   |
| `data_size`           | Data-size bytes the transaction kept                                                                                          |
| `write_records`       | Account and storage write records it kept: its KV count                                                                       |
| `limit_exceeded`      | `null`, or the limit that stopped it: `kind` (`data_size`, `kv_update`, `compute_gas`, `state_growth`), `limit`, `used`       |

In text mode the same numbers follow the summary under `=== Satin Gas ===`.
A legacy run has no `satin` field.

### Protocol limits

A Satin run is held to the protocol limits of the chain it names, read from the chain's schedule at the block's timestamp, as a node holds a block of that chain.
On a chain whose schedule runs Satin at that timestamp, these are the chain's own limits.
Any other Satin run is a counterfactual: MegaETH mainnet and testnet, which do not schedule Satin yet, the default chain `6342`, and any chain the tool does not know run on Satin's default limits.

| Limit                                                                    | Default    |
| ------------------------------------------------------------------------ | ---------- |
| A transaction's data size (`txDataSizeLimit`)                            | 13,107,200 |
| A block's data size (`blockTxsDataLimit`)                                | 13,107,200 |
| Compute after a block-environment read (`blockEnvAccessComputeGasLimit`) | 20,000,000 |
| Compute after an Oracle read (`oracleAccessComputeGasLimit`)             | 20,000,000 |
| Every other limit                                                        | unlimited  |

The defaults are provisional, as Satin is.
A chain whose schedule runs Satin but does not carry its limits is refused: the tool does not guess a chain's limits.
`run` and `tx` hold the transaction to the per-transaction limits; `replay` holds a whole block to them and to the block's budgets.

`--override.limits` runs under other limits, a counterfactual.
It takes a JSON object in the shape a chain configuration carries the limits in, inline or as the path of a file.
The fields it names replace the chain's, and every other stays.
The whole shape, at the defaults:

```json
{
  "txRuntimeLimits": {
    "txDataSizeLimit": 13107200,
    "frameDataSizeLimit": 18446744073709551615,
    "txKvUpdateLimit": 18446744073709551615,
    "frameKvUpdateLimit": 18446744073709551615,
    "txStateGasLimit": 18446744073709551615,
    "blockEnvAccessComputeGasLimit": 20000000,
    "oracleAccessComputeGasLimit": 20000000
  },
  "blockExecutionGasLimit": 18446744073709551615,
  "blockStateGasLimit": 18446744073709551615,
  "blockTxsDataLimit": 13107200,
  "blockKvUpdateLimit": 18446744073709551615
}
```

`18446744073709551615` (`u64::MAX`) leaves a limit unlimited.
The limits an override leaves are held to what a chain configuration is: an unknown field, a zero limit, a transaction data-size limit below the 310 bytes every transaction's body counts, or a compute cap no transaction reaches is refused.
A transaction a limit stops reports it in `satin.limit_exceeded`.
A command on a legacy spec is refused with `--override.limits`: the legacy engine's limits are its spec's.

## Examples

```bash
# Use MiniRex spec
mega-evme run 0x600160005260... --spec MiniRex

# Use Equivalence mode (Optimism Isthmus compatible)
mega-evme tx --spec Equivalence --receiver 0x1234...

# Custom chain ID
mega-evme run 0x600160005260... --chain-id 1

# The same bytecode on Satin
mega-evme run 0x600160005260... --spec Satin

# On Satin, with a transaction allowed to keep one write record
mega-evme run 0x600160005260... --spec Satin --override.limits '{"txRuntimeLimits":{"txKvUpdateLimit":1}}'
```
