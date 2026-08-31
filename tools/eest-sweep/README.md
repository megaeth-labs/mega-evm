# EEST corpus sweep

Runs the whole [execution-spec-tests](https://github.com/ethereum/execution-spec-tests) state-test
corpus through the mega-evm runner, in one command:

```bash
tools/eest-sweep/run.sh
```

That fetches the pinned fixture release, verifies its hash, unpacks the `state_tests` subtree, and
executes every fixture under the unstable spec **and** under the frozen spec it inherits from,
comparing the two. `.github/workflows/eest-nightly.yml` runs the same command nightly.

## Why a differential sweep

A state-test fixture pins what a transaction must produce — but only for a spec someone has already
computed an expectation for. The corpus records expectations for Ethereum forks, which mega-evm
maps onto `Equivalence`; for `Rex7` there is nothing to compare against, so executing the corpus
under it can only check that nothing crashes.

The frozen spec supplies the missing oracle. `Rex7` states the conditions under which it may _not_
differ from `Rex6` ([`docs/spec/upgrades/rex7.md`](../../docs/spec/upgrades/rex7.md), "Precision
invariant"), and read as a contrapositive that sentence classifies every disagreement: a difference
must come with evidence, read off the execution itself, that one of the invariant's three
hypotheses does not hold. A difference with no such evidence is a defect — in the implementation or
in the invariant.

## What fails the run

Two conditions, and only these two:

- **`PANIC`** — a fixture tripped a debug assertion or an internal invariant.
- **`UNEXPLAINED`** — the two specs disagreed and nothing in either execution licenses it.

Everything else is reported and does not fail:

- **`PASS`** — the two specs agreed on every compared quantity.
- **`EXPLAINED`** — they disagreed, with evidence (a crossed resource limit, a frame that ended in
  an exceptional halt, a `disableVolatileDataAccess` rejection).
- **`SKIPPED`** — neither spec executed the transaction, and both declined it identically. Most of
  this class is the MegaETH intrinsic-gas surcharge putting an Ethereum fixture's gas limit below
  what the transaction now costs.

`baseline.json` records the tally at the time this sweep was written. The nightly compares against
it and warns in the job summary when the coverage numbers move — a corpus that half-unpacked, or a
change that pushed thousands of fixtures out of execution, is worth seeing even though it is not a
defect. Update it deliberately when a move is expected.

## The cached corpus

The archive is verified against the hash in `corpus.env` on every run, and the tree unpacked from
it is verified against a manifest the unpack wrote: every file that was extracted, with that file's
hash. Before a cached tree is swept, that manifest is re-derived from the bytes on disk and
compared; a file missing, added or edited discards the tree and unpacks it again.

The tree is what the sweep actually reads, and a cached one can be short of the corpus in ways
nothing about it announces — an extraction cut off by a cancelled job or a full disk, a CI cache
archived mid-write and restored intact, a stray edit under the cache directory. Each of those
leaves a directory that exists and sweeps clean over a fraction of what the tally claims, which is
the one failure mode a coverage number cannot show.

Unpacking is serialized by an atomic `mkdir` lock, so two runs sharing a cache directory do not
extract into the same destination at once. A run that finds the lock held waits for it, and if the
wait runs out — the lock's owner died, or is very slow — unpacks a private tree of its own rather
than reaching into a directory another process may still be writing. A lock left behind by a dead
run is cleared by removing `<cache-dir>/<release>.unpack.lock`.

`tests/cache_integrity.sh` drives all of this against a synthetic archive and a stub binary; it
runs per-PR in CI and needs neither the corpus nor a build.

## Options

```
--target-spec SPEC   Spec under test (default: Rex7)
--base-spec SPEC     Frozen spec to compare against (default: Rex6)
--mode diff|fill     diff (default) executes both specs and classifies the differences.
                     fill executes the target spec only and recomputes each fixture's `post`
                     on a private copy — the older scan, kept because it exercises the
                     fixture-writing path that diff mode does not touch.
--corpus-dir DIR     Use an already-unpacked `state_tests` tree instead of downloading.
--cache-dir DIR      Where to keep the downloaded archive (default: .eest-cache).
--report-dir DIR     Where to write the report and log (default: .eest-report).
--profile PROFILE    Cargo profile (default: hivetests).
--no-build           Use an already-built binary.
```

The default profile is `hivetests` rather than `release` or `dev` deliberately: it is optimized
_and_ keeps debug assertions live, so the Rex7 gas-conservation cross-checks actually run. A
release build would sweep the corpus without evaluating them; a `dev` build evaluates them at
roughly a tenth of the speed.

## Bumping the corpus

Edit `corpus.env` (release, archive name, sha256), re-run the sweep, and update `baseline.json`
from the new report. The hash is verified on every run, so a mismatch — a re-uploaded asset, a
mirror serving something else — fails loudly instead of silently changing what the sweep covers.
