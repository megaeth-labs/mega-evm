---
description: Merge provider-cache files or capture envelopes offline.
---

# cache

Offline utilities for RPC cache files produced by `mega-evme`.

Today the only subcommand is `merge`, which consolidates multiple cache files into one without contacting a network.

## Usage

```
mega-evme cache merge <INPUT>... --output <FILE>
```

## `cache merge`

Union one or more input files into a single output file.

Inputs are auto-detected by JSON shape:

| Shape            | On-disk form                                | Produced by                             |
| ---------------- | ------------------------------------------- | --------------------------------------- |
| Provider cache   | JSON array of `{key, value}`                | `--rpc.cache-dir` per-chain files       |
| Capture envelope | `{version, chain_id, cache, external_env?}` | `--rpc.capture-file` / offline fixtures |

All inputs in one invocation must share the same shape.
Mixing a provider-cache file with a capture envelope is a hard error that names the offending path.

### Provider-cache merge

- Union entries by `key`.
- Later inputs win on collision.
- Output is a provider-cache-shaped JSON array, written atomically (temp file + rename).

### Envelope merge

- Every input must use the current envelope `version` and the same `chain_id` (else hard error naming the mismatch).
- Union the `cache` arrays by key; later inputs win on collision.
- `external_env`: if two inputs carry non-identical snapshots, hard error; otherwise propagate the non-null snapshot.
- Output is a pretty-printed envelope, written atomically.

### Summary

On success, `cache merge` prints one line and exits 0:

```
Merged 3 inputs (120 entries in) → 95 unique entries out
```

### Examples

Merge sharded worker provider caches after a multi-process campaign:

```bash
mega-evme cache merge \
  worker0/rpc-cache-4326.json \
  worker1/rpc-cache-4326.json \
  worker2/rpc-cache-4326.json \
  --output ./rpc-cache-4326.json
```

Merge two capture envelopes for the same chain:

```bash
mega-evme cache merge \
  capture-a.json \
  capture-b.json \
  -o merged-capture.json
```

## See also

- [State Management](../configuration/state-management.md#rpc-cache-and-retry) — live `--rpc.cache-dir` behavior and concurrent sharing
- [replay](replay.md#rpc-cache-file) — capture and offline replay fixtures
