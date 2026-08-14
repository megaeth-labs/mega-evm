---
description: Merge RPC cache envelopes offline.
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

Every input must be a cache envelope, the single on-disk format `mega-evme` reads and writes:

| Field          | Meaning                                                                                       |
| -------------- | --------------------------------------------------------------------------------------------- |
| `version`      | Envelope schema version; every input must carry the version this build writes.                |
| `chain_id`     | Chain the entries were recorded against; every input must name the same one.                  |
| `cache`        | The transport-level `{key, value}` entries.                                                   |
| `external_env` | Optional external-environment snapshot (SALT bucket capacities); present in capture fixtures. |

Both writers produce that shape: `--rpc.capture-file` fixtures and the per-chain `--rpc.cache-dir` files.
An online cache file additionally carries a `kind: "cache"` marker, which the merge reads past.

An input that is not an envelope is refused with an error naming the file.
The overwhelmingly likely case is the bare JSON array an older `mega-evme` wrote at `--rpc.cache-dir`, and such a file cannot be converted: it stores only hashed request keys, this build hashes requests differently, and the method and params behind each entry cannot be recovered from the file.
Delete it, or re-record the responses with `--rpc.capture-file`.

Every input is read and shape-checked before the output is locked, so a refused merge writes nothing and leaves no lock sidecar behind.

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
An existing output that parses but cannot be folded — an unrecognized JSON shape, the retired array format, a different `chain_id`, a different envelope `version` — is a hard error that names the output path and leaves the file untouched.
A mistyped `--output` should not destroy a file the merge cannot read as its own.

Warnings about a merge that may be silently lossy — an unreadable output being replaced, and the entries it held discarded — are printed on stderr regardless of verbosity.
They do not depend on `-v` flags or `RUST_LOG`, which only add the structured log event alongside them.
Stdout carries the summary line only, so it stays parseable.

### Merge rules

- Every input must use the current envelope `version` and the same `chain_id` (else hard error naming the mismatch).
  An envelope already at `--output` must agree with them too.
  Chain identity comes from the envelope body, so it holds whatever the files are named.
- Union the `cache` arrays by key; later inputs win on collision, and the inputs win over entries already in `--output`.
- `external_env`: if two inputs carry non-identical snapshots, hard error; otherwise propagate the non-null snapshot.
  A snapshot already at `--output` is held to the same rule.
- Output is a pretty-printed envelope, written atomically under the output lock.

The merged output carries no `kind` marker, so it is not an online cache file.
Pointing `--rpc.cache-dir` at a directory holding one makes that run warn, leave the file untouched, and start with an empty cache.
Merging consolidates fixtures; it is not a way to seed the per-chain online cache.

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
