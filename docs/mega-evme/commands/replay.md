---
description: Fetch and re-execute one or many on-chain transactions with optional overrides, tracing, and on-chain receipt verification.
---

# replay

Re-execute historical transactions locally using an RPC endpoint or a previously captured fixture file.
In online mode, `mega-evme` fetches the transaction, block environment, and pre-state from the RPC and re-executes locally.
In offline mode (`--rpc.replay-file`), all data is served from a local fixture captured by an earlier run — no network access is required.

`replay` has two modes.
The single-transaction mode replays the transaction named by the positional `TX_HASH` and supports the full option set (overrides, tracing, state dumps, fixture dumps).
[Batch mode](#batch-replay) (`--tx-file` / `--block`) replays many transactions in one process and reports one summary per transaction.

## Usage

```
mega-evme replay [OPTIONS] <TX_HASH|--tx-file <PATH>|--block <N>>
```

Exactly one replay target is required: the positional `TX_HASH`, `--tx-file`, or `--block`.
The three are mutually exclusive.

## Arguments

### `TX_HASH`

The transaction hash to replay (32-byte hex).

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

## Batch Replay

Replaying a corpus of transactions one process at a time pays for provider construction, chain-id resolution, and RPC cache parsing once per transaction — work that dominates the actual EVM execution.
Batch mode does all of it once.

A batch run builds a single provider and a single RPC cache, groups the requested transactions by their containing block, and processes the blocks in ascending order.
Each block is executed exactly once: state is forked at the parent block, pre-execution changes are applied, and every transaction of the block runs in order, with each requested transaction's result recorded before it is committed.
The RPC cache is persisted once, on exit, even if some transactions failed — the captured responses are the artifact you need to debug the failure offline.

Batch mode issues the same RPC calls as single-transaction replay, so an offline envelope captured by single-transaction runs serves a batch run without a cache miss.

### `--tx-file <PATH>`

Replay every transaction hash listed in `<PATH>`, one per line.

Blank lines and lines whose first non-whitespace character is `#` are ignored.
A hash listed more than once is replayed once.
A line that is not a valid 32-byte hex hash aborts the run before any network access, naming the offending line number.

### `--block <N>`

Replay every transaction of block `N`, given in decimal or `0x`-prefixed hex.

### Restrictions

Batch mode reports one summary per transaction and has no meaningful semantics for per-transaction artifacts or what-if knobs, so the following are rejected up front with an explanatory error rather than silently ignored:

- `--dump-fixture`
- Transaction overrides (`--override.gas-limit`, `--override.value`, `--override.input`, `--override.input-file`)
- `--override.spec` — each block's spec is auto-detected from its timestamp
- All trace options (`--trace`, `--trace.output`, `--tracer`, `--trace.*`)
- All state dump options (`--dump`, `--dump.output`)

Single-transaction replay keeps accepting all of them.

### Output

With `--json`, batch mode writes NDJSON: exactly one compact, single-line JSON object per requested transaction, in processing order (ascending block, then transaction index).

A transaction that executed is reported as its `tx_hash`, `block_number`, and `tx_index`, followed by the same fields the single-transaction JSON output carries (`success`, `gas_used`, `logs_count`, and the optional `output` / `contract_address` / `revert_reason` / `halt_reason`) and its `receipt`.
Both shapes below are expanded for readability; on the wire each object occupies exactly one line.

```json
{
  "tx_hash": "0x…",
  "block_number": 22945844,
  "tx_index": 3,
  "success": true,
  "gas_used": 81740,
  "logs_count": 0,
  "receipt": { "…": "…" }
}
```

A transaction that could not be executed is reported as an error entry instead:

```json
{
  "tx_hash": "0x…",
  "error": { "kind": "not_found", "message": "Transaction not found" }
}
```

`kind` is one of `not_found` (unknown hash), `pending` (mined into no block yet), `rpc` (an RPC call failed), or `execution` (block setup or the block executor rejected the transaction).
Execution outcomes are not errors: a reverted or halted transaction is a normal result line with `success: false`.

Without `--json`, each transaction is printed with a header naming its hash, block, and index, followed by the same summary and receipt the single-transaction mode prints.
A final one-line summary (transactions replayed, transactions failed, elapsed time) is logged at `INFO` level, so pass `-vvv` to see it.

With [`--verify-receipt`](#receipt-verification), each result line additionally carries a `verification` object.

### Exit Status

A batch run exits `0` when every requested transaction produced an execution result, and `1` when any of them produced an error entry.
The NDJSON stream is written to stdout in both cases; diagnostics go to stderr.

### Examples

Replay a whole block offline and stream the results as NDJSON:

```bash
mega-evme replay --rpc.replay-file ./fixtures/blocks.json --block 22945844 --json
```

Replay a corpus of transactions against a live RPC, one process for the lot:

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --tx-file ./corpus.txt --json > results.ndjson
```

Where `corpus.txt` looks like:

```
# regression corpus, refreshed 2026-08-03
0xde3d56dc739484166b8af1bea757bf7e3e9a4b9a0fb62d722703345570dfc1d6
0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef
```

Count the transactions that did not succeed:

```bash
jq -c 'select(.error != null or .success == false)' results.ndjson | wc -l
```

## Receipt Verification

Replaying a transaction only proves that the local EVM produced _some_ result; equivalence verification needs that result checked against what the chain recorded.
`--verify-receipt` builds that check into the tool: it fetches the on-chain receipt of every replayed transaction and compares it against the receipt the replay produced, so verifying an upgrade is one command over one transaction list instead of a replay run plus a separate receipt-diffing pipeline.

### `--verify-receipt`

Verify every replayed transaction against its on-chain receipt.
Supported in both single-transaction and [batch](#batch-replay) mode.

Three dimensions are compared:

- **Status** — the success flag.
- **Gas used** — the transaction's gas, not the block's cumulative gas.
- **Logs** — the number of logs, and each log's `address`, `topics`, and `data`.

Logs are compared explicitly rather than inferred from gas: `LOG` gas depends on topic count and data length, never on content, so two executions can burn identical gas yet emit different log payloads.

The receipt is fetched with the same call the [fixture dump](#self-validating-fixture-dump) uses, so a run with `--rpc.capture-file` records it and a later `--rpc.replay-file` run verifies the same transaction offline.
An envelope captured without `--verify-receipt` (or by any earlier run that never needed a receipt) holds no receipts, so verifying against it fails the receipt fetch — capture once online with the flag, then re-verify offline as often as you like.

### Verified, Unverified, and Mismatched

A transaction is only reported as mismatched when both receipts were compared and disagreed.
Anything that prevents the comparison from running is an infrastructure failure — the transaction is _unverified_, which is a different finding from a divergence:

- The endpoint fails the receipt call, or has pruned the receipt below its retention height (common on non-archive endpoints): reported as an `rpc` failure.
- The receipt describes a different inclusion than the replayed block (its `blockHash` differs — a reorg in progress, or a load-balanced endpoint serving divergent views): reported as an `rpc` failure, because comparing against it would compare the replay to the wrong on-chain execution.
- The target is a pending transaction, which has no receipt yet: rejected up front in single-transaction mode, and reported as a `pending` error entry in batch mode.

In batch mode each of these becomes an error entry for that transaction, exactly like any other infrastructure failure.

Transaction overrides and `--override.spec` are still accepted with `--verify-receipt`, but they make the replay a what-if that the chain never executed, so the comparison will normally report a mismatch.

### Output

With `--json`, the verdict is a `verification` object — added to the single-transaction summary, and to each batch result line.
The field is absent entirely without the flag.

A match carries nothing else:

```json
{ "match": true }
```

A mismatch carries a `diff` holding only the dimensions that disagreed, each as `{"onchain": …, "replay": …}`:

```json
{
  "match": false,
  "diff": {
    "status": { "onchain": true, "replay": false },
    "gas_used": { "onchain": 75514, "replay": 75500 },
    "logs": {
      "count": { "onchain": 2, "replay": 1 },
      "first_mismatch": {
        "index": 0,
        "field": "address",
        "onchain": "0x00000000000000000000000000000000000000aa",
        "replay": "0x00000000000000000000000000000000000000bb"
      }
    }
  }
}
```

Under `logs`, `count` is present when the two sides emitted a different number of logs, and `first_mismatch` names the first log both sides emitted whose contents differ — its `field` is `address`, `topics`, or `data`, and the two values are that field's contents on each side.
Both can appear at once, which distinguishes truncated logs from rewritten ones.

Without `--json`, each transaction gets one verdict line after its usual output:

```
verification: MATCH
verification: MISMATCH (gas_used: onchain 75514 vs replay 75500)
```

The mismatch line names every dimension that disagreed, comma-separated.

### Exit Status

A run in which every target replayed and every verification matched exits `0`.
A verification mismatch exits non-zero through a dedicated error (`Receipt verification mismatch: N of M verified transaction(s) did not reproduce the on-chain receipt`), reported after every result line has been written.
Infrastructure failures keep their own non-zero exit and take precedence in a batch run: a target that never replayed was also never verified, so reporting it as a mismatch would overstate what the run found.

### Examples

Verify one transaction against a live RPC:

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --verify-receipt 0xabc123...
```

Verify a whole corpus in one process and collect the divergences:

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc \
  --tx-file ./corpus.txt --verify-receipt --json > results.ndjson

jq -c 'select(.verification.match == false)' results.ndjson    # mismatched
jq -c 'select(.error != null)' results.ndjson                  # unverified
```

Capture once online, then re-verify the same corpus offline:

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc \
  --rpc.capture-file ./corpus.cache.json --tx-file ./corpus.txt --verify-receipt --json

mega-evme replay --rpc.replay-file ./corpus.cache.json \
  --tx-file ./corpus.txt --verify-receipt --json
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

`--rpc.capture-file` is mutually exclusive with `--rpc.replay-file`, `--rpc.cache-dir`, `--rpc.clear-cache`, `--rpc.no-cache-file`, and `--rpc.cache-max-entries`.

### `--rpc.replay-file <PATH>`

Replay mode. Requires neither `--rpc` nor network access.

The envelope at `<PATH>` is loaded and serves as the only source of JSON-RPC responses.
Any request that is not present in the fixture aborts the run with a hard error — there is no fall-through to a live RPC endpoint.

Bucket-capacity data is read from the fixture envelope, so `--bucket-capacity` is neither required nor accepted with `--rpc.replay-file`.
Passing `--bucket-capacity` together with `--rpc.replay-file` is rejected; to regenerate a fixture with new capacities, re-run in capture mode.

`--rpc.replay-file` is mutually exclusive with `--rpc`, `--rpc.capture-file`, `--rpc.cache-dir`, `--rpc.clear-cache`, `--rpc.no-cache-file`, and `--rpc.cache-max-entries`.

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

**Fidelity gate.** Before building the fixture, the dump fetches the transaction's on-chain receipt and requires the local replay to reproduce the receipt's `gasUsed`, success status, and logs root exactly.
Logs are compared explicitly rather than inferred from gas: `LOG` gas depends on topic count and data length, never content, so two executions can burn identical gas yet emit different log payloads.
A mismatch aborts the dump (no file is written) with a clear error — this catches a wrong spec or hardfork config, which self-validation alone cannot, because the fixture is validated under the same spec it was dumped with.
It then additionally cross-checks the isolated execution against the full replay (gas, status — including the exact halt reason for halted transactions — output, and logs root), so any gas-, output-, or log-visible divergence — including across the L1 data fee, which the isolated run zeroes but the full replay charges — aborts the dump.
One channel stays open by construction: the isolated run's sender balance is shifted by the zeroed L1 fee, so a contract that stores a balance-derived value bakes that shifted value into `post` (and the sender's final balance in `post` likewise differs from the chain).
The fixture still self-validates and reproduces gas exactly; only such balance-derived state values differ.

`--dump-fixture` cannot be combined with transaction overrides or `--override.spec` (a forced spec would record a what-if, not the on-chain transaction), and deposit transactions are not supported.
A target transaction that reads a block hash via `BLOCKHASH` is also rejected: fixtures carry no historical block hashes, so the isolated re-execution could not reproduce the values the replay observed.
Block hash reads by preceding transactions in the same block do not matter — only the target transaction's reads are checked.
Because the fidelity gate reads the receipt, an offline dump (`--rpc.replay-file`) requires the receipt to be present in the capture — so capture and dump together in the online run, then re-dump offline reproducibly.
When combined with `--rpc.capture-file`, the capture file is written even if execution or the fidelity gate fails, so the captured RPC responses remain available for debugging the failure offline.

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

For any other chain, `replay` enables all hardforks at genesis (currently equivalent to `Rex7`).

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
Options marked _(single transaction only)_ are rejected in [batch mode](#batch-replay).

- **Batch replay** — Replay many transactions in one process via `--tx-file` / `--block`.
  See [Batch Replay](#batch-replay) above.
- **Receipt verification** — Check every replayed transaction against its on-chain receipt via `--verify-receipt`.
  See [Receipt Verification](#receipt-verification) above.
- **SALT buckets** — Configure SALT bucket capacity for dynamic storage gas pricing.
  See [SALT Buckets](../configuration/salt-buckets.md).
- **State dump** _(single transaction only)_ — Dump or load pre/post-state snapshots.
  See [State Management](../configuration/state-management.md).
- **RPC cache file** — Single-file JSON-RPC capture and offline replay via `--rpc.capture-file` / `--rpc.replay-file`.
  See [RPC Cache File](#rpc-cache-file) above.
- **RPC cache / retry** — Per-chain response cache, retry, and rate-limit settings.
  See [RPC Cache and Retry](../configuration/state-management.md#rpc-cache-and-retry).
- **Tracing** _(single transaction only)_ — Emit execution traces (call traces, opcode traces, gas profiles, etc.).
  See [Tracing Overview](../tracing/overview.md).
- **Fixture dump** _(single transaction only)_ — Write a self-validating EEST state-test fixture via `--dump-fixture`.
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

**Replay a whole block as NDJSON**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --block 22945844 --json
```

**Verify a whole block against its on-chain receipts**

```bash
mega-evme replay --rpc https://mainnet.megaeth.com/rpc --block 22945844 --verify-receipt --json
```

## See Also

- [`run`](./run.md) — Execute raw EVM bytecode locally without fetching from RPC
- [`tx`](./tx.md) — Send a new transaction against a local state
- [Tracing Overview](../tracing/overview.md)
- [SALT Buckets](../configuration/salt-buckets.md)
- [State Management](../configuration/state-management.md)
