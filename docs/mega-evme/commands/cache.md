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

### Output locking

`--output` may be a file a live `mega-evme` run is persisting to.
The merge therefore uses the same protocol that clean-exit persist uses (see [State Management](../configuration/state-management.md#concurrent-cache-dir-sharing)):

1. Take the exclusive advisory lock on the output's sidecar (`<output>.lock`), blocking until it is free.
2. Under that lock, read whatever the output file holds now and fold it into the union as one more input.
3. Write via temp file + atomic rename, then release the lock.

Folding the current output in is what makes the lock worth taking: entries a concurrent process wrote while the merge waited are carried into the merged result instead of being overwritten.
The merge's own inputs win where their keys collide with the output's prior entries.

If the lock cannot be acquired at all (for example the sidecar path is not writable), the merge fails with an error and writes nothing.
An unlocked write would silently drop a concurrent process's entries, which is the failure the merge exists to prevent.

An existing output that cannot be parsed at all (corrupt JSON) is replaced by the merged inputs, with a warning.
An existing output that parses but cannot be folded — the other cache shape, an unrecognized JSON shape, a different `chain_id`, a different envelope `version` — is a hard error that names the output path and leaves the file untouched.
Both shapes classify it the same way: a mistyped `--output` should not destroy a file the merge cannot read as its own.

### Provider-cache merge

- Union entries by `key`.
- Later inputs win on collision; the inputs win over entries already in `--output`.
- Output is a provider-cache-shaped JSON array, written atomically (temp file + rename) under the output lock.
- Chain identity is taken only from the standard filename `rpc-cache-{chain_id}.json` (provider-cache bodies have no chain field).
  Every input path and `--output` that matches that pattern must name the same chain id; a mismatch is a hard error that names the conflicting files.
  Paths that do not match the pattern emit a warning that chain identity cannot be validated for them, and the merge proceeds for those paths without a filename-based check.

### Envelope merge

- Every input must use the current envelope `version` and the same `chain_id` (else hard error naming the mismatch).
  An envelope already at `--output` must agree with them too.
- Union the `cache` arrays by key; later inputs win on collision, and the inputs win over entries already in `--output`.
- `external_env`: if two inputs carry non-identical snapshots, hard error; otherwise propagate the non-null snapshot.
  A snapshot already at `--output` is held to the same rule.
- Output is a pretty-printed envelope, written atomically under the output lock.

### Summary

On success, `cache merge` prints one line and exits 0:

```
Merged 3 inputs (120 entries in) → 95 unique entries out
```

When the output file already held entries, they are counted separately, so the arithmetic still adds up:

```
Merged 3 inputs (120 entries in + 12 already in the output) → 101 unique entries out
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
