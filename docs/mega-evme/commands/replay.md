---
description: Fetch and re-execute an on-chain transaction with optional overrides and tracing.
---

# replay

Re-execute a historical transaction locally using an RPC endpoint or a previously captured fixture file.
In online mode, `mega-evme` fetches the transaction, block environment, and pre-state from the RPC and re-executes locally.
In offline mode (`--rpc.replay-file`), all data is served from a local fixture captured by an earlier run — no network access is required.

## Usage

```
mega-evme replay [OPTIONS] <TX_HASH>
```

## Arguments

### `TX_HASH`

The transaction hash to replay (32-byte hex, required).

`mega-evme` re-executes the transaction locally using state and block context sourced from either an RPC endpoint or a local fixture file.
This gives you a fully reproducible execution without needing a local archive node.

### `--rpc <URL>`

Aliases: `--rpc-url`

The RPC endpoint to fetch the transaction and state from.
Any standard Ethereum JSON-RPC provider works here.
Required for online replay and capture mode; omit when using `--rpc.replay-file`.

```
mega-evme replay --rpc https://mainnet.megaeth.com/rpc <TX_HASH>
```

## RPC Cache File

`mega-evme replay` supports a transport-level JSON-RPC fixture mechanism that records every request/response pair to a single file and serves them back on later runs without touching the network.
It is useful for pinning a reproducible replay (e.g. for regression tests, debugging sessions, or offline review) and for running `replay` in environments that cannot reach the RPC endpoint.

Unlike the generic [RPC Cache](../configuration/state-management.md#rpc-cache-and-retry), which is keyed on a small allow-list of cacheable methods and stored per chain under the platform cache directory, the cache file covers every single (non-batch) JSON-RPC call issued during the replay and lives at a user-chosen path.

The mechanism has two modes, selected by two mutually exclusive flags.

### `--rpc.capture-file <PATH>`

Capture mode. Requires `--rpc`.

On the first run, every JSON-RPC request and response issued while serving the replay is captured in memory and the complete envelope is written to `<PATH>` atomically on clean exit.
On subsequent runs the existing file is loaded, its entries are merged into the in-memory transport cache, and only missing requests are fetched from the RPC endpoint.
The updated set of entries is persisted back to the same file on clean exit.

The file also embeds an external-environment snapshot — currently the set of `--bucket-capacity` values in effect — so the captured fixture is self-contained.
If `--bucket-capacity` is not passed on a subsequent run, the previous envelope's values are reused; passing `--bucket-capacity` overrides them.

`--rpc.capture-file` is mutually exclusive with `--rpc.replay-file`, `--rpc.cache-dir`, `--rpc.clear-cache`, `--rpc.no-cache-file`, and `--rpc.cache-size`.

### `--rpc.replay-file <PATH>`

Replay mode. Requires neither `--rpc` nor network access.

The envelope at `<PATH>` is loaded and serves as the only source of JSON-RPC responses.
Any request that is not present in the fixture aborts the run with a hard error — there is no fall-through to a live RPC endpoint.

Bucket-capacity data is read from the fixture envelope, so `--bucket-capacity` is neither required nor accepted with `--rpc.replay-file`.
Passing `--bucket-capacity` together with `--rpc.replay-file` is rejected; to regenerate a fixture with new capacities, re-run in capture mode.

`--rpc.replay-file` is mutually exclusive with `--rpc`, `--rpc.capture-file`, `--rpc.cache-dir`, `--rpc.clear-cache`, `--rpc.no-cache-file`, and `--rpc.cache-size`.

### Examples

Capture a transaction's RPC traffic to a fixture file (first run hits the endpoint, later runs with the same flag reuse and top up the file):

```bash
mega-evme replay \
  --rpc https://mainnet.megaeth.com/rpc \
  --rpc.capture-file ./fixtures/tx.json \
  0xabc123...
```

Replay the captured transaction fully offline:

```bash
mega-evme replay --rpc.replay-file ./fixtures/tx.json 0xabc123...
```

## Self-Validating Fixture Dump

`--dump-fixture <FILE>` turns a single replay into a self-validating regression fixture.
The fixture is written in the same EEST state-test schema that the `state-test` runner consumes (`TestUnit { env, pre, transaction, post, out }`), so re-running it through `state-test` re-executes the transaction and checks the result — no RPC access required.

### `--dump-fixture <FILE>`

Dump a self-validating fixture for the replayed transaction to `<FILE>`.

The fixture captures everything needed to deterministically re-execute the target transaction in isolation:

- `pre` — the pre-state read closure (every account and storage slot the transaction touched), with their pre-transaction values.
- `env` — the block environment (number, timestamp, coinbase, base fee, gas limit, prevrandao, excess blob gas).
- `transaction` — the target transaction.
- `megaEnv` — a MegaETH-specific extension carrying the SALT bucket capacities and oracle storage in effect, so dynamic storage-gas pricing reproduces exactly. Pure-Ethereum fixtures omit this field.
- `post` — the expected result for the executed spec: state-root and logs-root, plus explicit `megaGasUsed` and `megaStatus` expectations that produce readable diffs on mismatch.

The `post` expectation is computed by the `state-test` runner itself — the exact code path that later validates the fixture — so a dumped fixture is self-consistent by construction.

**Fidelity gate.** Before building the fixture, the dump fetches the transaction's on-chain receipt and requires the local replay to reproduce the receipt's `gasUsed` and success status exactly (gas already implies log fidelity, since `LOG` opcodes are metered).
A mismatch aborts the dump (no file is written) with a clear error — this catches a wrong spec or hardfork config, which self-validation alone cannot, because the fixture is validated under the same spec it was dumped with.
It then additionally cross-checks the isolated execution against the full replay (gas, status, output), so the recorded `megaGasUsed` equals the real on-chain gas.

`--dump-fixture` cannot be combined with transaction overrides or `--override.spec` (a forced spec would record a what-if, not the on-chain transaction), and deposit transactions are not supported.
Because the fidelity gate reads the receipt, an offline dump (`--rpc.replay-file`) requires the receipt to be present in the capture — so capture and dump together in the online run, then re-dump offline reproducibly.

```bash
# Online: fetch + dump in one shot (records the receipt into the capture file):
mega-evme replay --rpc https://mainnet.megaeth.com/rpc \
  --rpc.capture-file ./cap.json --dump-fixture ./fixtures/0xabc123.json 0xabc123...

# Offline: re-dump reproducibly from the capture (receipt already captured):
mega-evme replay --rpc.replay-file ./cap.json --dump-fixture ./fixtures/0xabc123.json 0xabc123...

# Validate the fixture (and detect any gas/status/result drift):
state-test ./fixtures/0xabc123.json
```

## Throughput Benchmark

To benchmark a replayed transaction, dump it to a fixture and time the fixture with the `state-test` runner — there is no `replay`-side benchmark flag:

```bash
mega-evme replay --rpc <url> --dump-fixture /tmp/tx.json 0xabc123...
state-test --bench /tmp/tx.json
```

`state-test --bench` reports `min` / `median` / `mean` time and throughput (Mgas/s), timing only the EVM `transact` call. A committed corpus of characteristic transactions and a base-vs-PR comparison driver build on this to track real-transaction throughput across changes; see `bench/replay/` and the `replay-bench` CI workflow.

## Spec Auto-Detection

The EVM spec controls which opcodes, gas rules, and MegaETH-specific behaviors are active during execution.
`replay` auto-detects the spec from the chain ID and the block timestamp of the replayed transaction.
Hardcoded hardfork configs exist for:

- **Chain 6343** — MegaETH testnet v2
- **Chain 4326** — MegaETH mainnet

For any other chain, `replay` enables all hardforks at genesis (currently equivalent to `Rex5`).

### `--override.spec <SPEC>`

Override the auto-detected spec.
Useful when you want to test how the transaction would behave under a different spec, or when replaying against a chain that isn't recognized.

```
mega-evme replay --override.spec Rex2 <TX_HASH>
```

## Transaction Overrides

Override flags let you modify the transaction before re-executing it.
The original on-chain transaction is unchanged; only the local replay is affected.
This is useful for "what-if" testing: what happens with less gas, different calldata, or a different ETH value?

| Flag                           | Aliases                | Description                                                        |
| ------------------------------ | ---------------------- | ------------------------------------------------------------------ |
| `--override.gas-limit <GAS>`   | `--override.gaslimit`  | Replace the transaction's gas limit                                |
| `--override.value <VALUE>`     |                        | Replace the ETH value sent (supports `1ether`, `100gwei` suffixes) |
| `--override.input <HEX>`       | `--override.data`      | Replace the calldata with a hex-encoded byte string                |
| `--override.input-file <FILE>` | `--override.data-file` | Replace the calldata with the contents of a file                   |

Note that `replay` does not accept `--chain`, block environment flags, or pre-state flags.
All of that context comes from the RPC.

## Options

`replay` supports the following shared option groups.
See the linked pages for full details.

- **SALT buckets** — Configure SALT bucket capacity for dynamic storage gas pricing.
  See [SALT Buckets](../configuration/salt-buckets.md).
- **State dump** — Dump or load pre/post-state snapshots.
  See [State Management](../configuration/state-management.md).
- **RPC cache file** — Single-file JSON-RPC capture and offline replay via `--rpc.capture-file` / `--rpc.replay-file`.
  See [RPC Cache File](#rpc-cache-file) above.
- **RPC cache / retry** — Per-chain response cache, chain-id override, retry and rate-limit settings.
  See [RPC Cache and Retry](../configuration/state-management.md#rpc-cache-and-retry).
- **Tracing** — Emit execution traces (call traces, opcode traces, gas profiles, etc.).
  See [Tracing Overview](../tracing/overview.md).
- **Fixture dump** — Write a self-validating EEST state-test fixture via `--dump-fixture`.
  See [Self-Validating Fixture Dump](#self-validating-fixture-dump) above.
- **Throughput benchmark** — Dump a fixture (`--dump-fixture`) and time it with `state-test --bench`.
  See [Throughput Benchmark](#throughput-benchmark) above.

## Examples

In all examples below, replace `0xabc123...` with a real transaction hash from MegaETH mainnet.
Every `replay` invocation requires one of `--rpc <URL>`, `--rpc.capture-file <PATH>`, or `--rpc.replay-file <PATH>`.

**Replay against a live RPC**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc 0xabc123...
```

**Replay with call tracing**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --trace --tracer call 0xabc123...
```

**Replay with a tighter gas limit**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --override.gas-limit 50000 0xabc123...
```

**Replay with different calldata**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --override.input 0xdeadbeef 0xabc123...
```

**Replay under a specific spec**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --override.spec Rex2 0xabc123...
```

## See Also

- [`run`](./run.md) — Execute raw EVM bytecode locally without fetching from RPC
- [`tx`](./tx.md) — Send a new transaction against a local state
- [Tracing Overview](../tracing/overview.md)
- [SALT Buckets](../configuration/salt-buckets.md)
- [State Management](../configuration/state-management.md)
