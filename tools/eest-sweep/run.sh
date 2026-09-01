#!/usr/bin/env bash
#
# Run the EEST state-test corpus through the mega-evm state-test runner.
#
# One command: fetch and verify the pinned fixture release, unpack its `state_tests` subtree, and
# execute every fixture. In the default `diff` mode two gates fail the run — a fixture that panics,
# and a difference between the spec under test and its frozen base that no MegaETH mechanism
# accounts for. Everything else (fixtures the runner declines, differences the classifier explains)
# is reported and does not fail. `chaos` mode has its own gates; see `--mode`.
#
# Usage:
#   tools/eest-sweep/run.sh [options]
#
#   --target-spec SPEC   Spec under test (default: Rex7)
#   --base-spec SPEC     Frozen spec to compare against (default: Rex6)
#   --mode diff|fill|chaos
#                        diff: execute under both specs and classify the differences (default).
#                        fill: execute under the target spec only and recompute each fixture's
#                        `post` in place, on a private copy. `diff` runs the target spec through
#                        the same execution path, so it already covers what `fill` scans for;
#                        `fill` remains available to exercise the fixture-writing path itself.
#                        chaos: execute under the target spec three times per vector — with no
#                        inspector, with a read-only one, and with a deterministic rewriting one —
#                        and check that observation stays free and that nothing the rewriting run
#                        does breaks the gas-accounting cross-checks.
#   --chaos-seed SEED    Global seed for `--mode chaos` (default: 1). Each vector's own seed is
#                        derived from this and the vector's identity, so a flagged vector
#                        reproduces exactly.
#   --chaos-arg ARG      Extra argument passed through to the chaos run; repeatable. Used to
#                        narrow a flagged vector (`--chaos-shapes`, `--chaos-skip-*`).
#   --corpus-dir DIR     Use an already-unpacked `state_tests` tree instead of downloading.
#   --cache-dir DIR      Where to keep the downloaded archive (default: .eest-cache).
#   --report-dir DIR     Where to write the report and log (default: .eest-report).
#   --profile PROFILE    Cargo profile to build with (default: hivetests — optimized, with debug
#                        assertions live, which is what makes the conservation cross-checks fire).
#   --no-build           Use an already-built binary.
#   -h, --help           Show this message.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=tools/eest-sweep/corpus.env
source "$REPO_ROOT/tools/eest-sweep/corpus.env"

TARGET_SPEC="Rex7"
BASE_SPEC="Rex6"
MODE="diff"
CHAOS_SEED="1"
CHAOS_ARGS=()
CORPUS_DIR=""
CACHE_DIR="$REPO_ROOT/.eest-cache"
REPORT_DIR="$REPO_ROOT/.eest-report"
PROFILE="hivetests"
BUILD=1

while [ $# -gt 0 ]; do
  case "$1" in
    --target-spec) TARGET_SPEC="$2"; shift 2 ;;
    --base-spec) BASE_SPEC="$2"; shift 2 ;;
    --mode) MODE="$2"; shift 2 ;;
    --chaos-seed) CHAOS_SEED="$2"; shift 2 ;;
    --chaos-arg) CHAOS_ARGS+=("$2"); shift 2 ;;
    --corpus-dir) CORPUS_DIR="$2"; shift 2 ;;
    --cache-dir) CACHE_DIR="$2"; shift 2 ;;
    --report-dir) REPORT_DIR="$2"; shift 2 ;;
    --profile) PROFILE="$2"; shift 2 ;;
    --no-build) BUILD=0; shift ;;
    # Print the header comment block and stop at the first line that is not one, so the help text
    # can never run past it into the script body.
    -h|--help) sed -n '2,${/^#/!q;s/^# \{0,1\}//p;}' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

case "$MODE" in
  diff|fill|chaos) ;;
  *) echo "--mode must be 'diff', 'fill' or 'chaos', got '$MODE'" >&2; exit 2 ;;
esac

# `sha256sum` on Linux, `shasum -a 256` on macOS.
if command -v sha256sum >/dev/null 2>&1; then
  SHA256=(sha256sum)
else
  SHA256=(shasum -a 256)
fi
sha256_of() {
  "${SHA256[@]}" "$1" | cut -d' ' -f1
}

# How long to wait for another run that is already unpacking this corpus, before giving up on the
# shared tree and unpacking a private one. A full extraction is well under a minute; the override
# is for the tests, and for a machine whose lock is known to be stale.
LOCK_WAIT_SECS="${EEST_UNPACK_LOCK_WAIT_SECS:-900}"

# Name of the manifest inside an unpacked tree. Excluded from its own listing.
MANIFEST_NAME=".manifest"

# Every file of an unpacked tree with its hash, in a byte-stable order.
corpus_manifest() {
  (cd "$1" && find . -type f ! -name "$MANIFEST_NAME" -print0 |
    LC_ALL=C sort -z |
    xargs -0 "${SHA256[@]}")
}

# The manifest covers regular files, which is everything the fixture archive holds. An entry of
# any other kind is outside what it can speak for — a symlink is not hashed, so swapping its
# target would leave the manifest matching — so such a tree is rejected rather than described.
has_irregular_entry() {
  [ -n "$(find "$1" ! -type d ! -type f -print -quit)" ]
}

# Whether a tree is exactly what this archive unpacks to: the manifest names this archive, and
# every file in the tree still hashes to what the manifest recorded — with no file added, removed
# or rewritten since. This is what the sweep's coverage rests on, so it is re-derived from the
# bytes on every run rather than trusted from a marker a previous run left behind.
corpus_is_intact() {
  local root="$1" manifest="$1/$MANIFEST_NAME" actual status
  [ -f "$manifest" ] || return 1
  [ "$(head -n 1 "$manifest")" = "archive-sha256 $EEST_SHA256" ] || return 1
  has_irregular_entry "$root" && return 1
  actual="$(mktemp)"
  if ! corpus_manifest "$root" >"$actual" 2>/dev/null; then
    rm -f "$actual"
    return 1
  fi
  status=0
  tail -n +2 "$manifest" | cmp -s - "$actual" || status=1
  rm -f "$actual"
  return "$status"
}

# Unpack the archive into $1, manifest and all, via a scratch directory and one rename: the
# destination either does not exist or holds a tree this function extracted whole.
unpack_corpus() {
  local dest="$1" stage="$CACHE_DIR/.unpack.$$"
  rm -rf "$stage"
  mkdir -p "$stage"
  # Only `state_tests` is unpacked: the runner reads the state-test format, and the archive's
  # blockchain-test subtrees are several times larger.
  tar -xzf "$ARCHIVE" -C "$stage" --strip-components=1 fixtures/state_tests
  if has_irregular_entry "$stage/state_tests"; then
    echo "the archive unpacks to something other than a tree of regular files, which the corpus" >&2
    echo "manifest cannot describe; teach corpus_manifest about it before sweeping." >&2
    exit 1
  fi
  {
    echo "archive-sha256 $EEST_SHA256"
    corpus_manifest "$stage/state_tests"
  } >"$stage/state_tests/$MANIFEST_NAME"
  mkdir -p "$(dirname "$dest")"
  rm -rf "$dest"
  mv "$stage/state_tests" "$dest"
  rm -rf "$stage"
}

# The unpack lock and any private tree this run extracted, released however the run ends.
UNPACK_LOCK=""
LOCK_HELD=0
PRIVATE_ROOT=""
cleanup() {
  if [ "$LOCK_HELD" -eq 1 ]; then
    rm -rf "$UNPACK_LOCK"
    LOCK_HELD=0
  fi
  if [ -n "$PRIVATE_ROOT" ]; then
    rm -rf "$PRIVATE_ROOT"
  fi
}
trap cleanup EXIT

mkdir -p "$REPORT_DIR"

# --- corpus -----------------------------------------------------------------------------------

if [ -z "$CORPUS_DIR" ]; then
  mkdir -p "$CACHE_DIR"
  ARCHIVE="$CACHE_DIR/$EEST_RELEASE-$EEST_ARCHIVE"
  if [ ! -f "$ARCHIVE" ]; then
    echo "==> downloading EEST $EEST_RELEASE / $EEST_ARCHIVE"
    # Download beside the target and rename on success, so an interrupted download can never be
    # mistaken for a cached archive on the next run.
    curl --fail --location --show-error --silent \
      --output "$ARCHIVE.part" \
      "$EEST_URL_BASE/$EEST_RELEASE/$EEST_ARCHIVE"
    mv "$ARCHIVE.part" "$ARCHIVE"
  fi

  ACTUAL="$(sha256_of "$ARCHIVE")"
  if [ "$ACTUAL" != "$EEST_SHA256" ]; then
    echo "corpus hash mismatch for $ARCHIVE" >&2
    echo "  expected $EEST_SHA256" >&2
    echo "  actual   $ACTUAL" >&2
    echo "Delete the cached archive and re-run, or update tools/eest-sweep/corpus.env." >&2
    exit 1
  fi
  echo "==> corpus hash verified: $EEST_SHA256"

  CORPUS_DIR="$CACHE_DIR/$EEST_RELEASE/state_tests"
  # What the sweep reports is a statement about the corpus it read, so the tree it reads has to be
  # the whole corpus and nothing else. A cached tree can be short of that in ways nothing about it
  # announces: an extraction interrupted by a cancelled job or a full disk, a cache archived
  # mid-write and restored intact, a stray edit under the cache directory. Each leaves a directory
  # that exists, sweeps clean, and covers a fraction of what the tally claims.
  #
  # So a cached tree is re-verified against its own manifest — every file, hashed — before it is
  # used, and discarded and re-extracted when it does not match.
  if corpus_is_intact "$CORPUS_DIR"; then
    echo "==> corpus tree verified against its manifest"
  else
    # Two runs sharing a cache directory would otherwise extract into the same destination at the
    # same time, and the loser's rename lands inside the winner's tree. An atomic `mkdir` picks
    # one producer; the other waits for it and, if the wait runs out, extracts a private tree
    # rather than reaching into a directory a live process may still own.
    UNPACK_LOCK="$CACHE_DIR/$EEST_RELEASE.unpack.lock"
    mkdir -p "$CACHE_DIR"
    if mkdir "$UNPACK_LOCK" 2>/dev/null; then
      LOCK_HELD=1
      echo "$$" >"$UNPACK_LOCK/pid" 2>/dev/null || true
      echo "==> unpacking state_tests"
      unpack_corpus "$CORPUS_DIR"
      rm -rf "$UNPACK_LOCK"
      LOCK_HELD=0
    else
      echo "==> another run is unpacking this corpus; waiting up to ${LOCK_WAIT_SECS}s"
      WAITED=0
      while [ -d "$UNPACK_LOCK" ] && [ "$WAITED" -lt "$LOCK_WAIT_SECS" ]; do
        sleep 5
        WAITED=$((WAITED + 5))
      done
      if ! corpus_is_intact "$CORPUS_DIR"; then
        PRIVATE_ROOT="$CACHE_DIR/.private.$$"
        echo "==> shared corpus is not usable; unpacking a private tree at $PRIVATE_ROOT"
        echo "    (a lock left behind by a dead run is cleared by removing $UNPACK_LOCK)" >&2
        unpack_corpus "$PRIVATE_ROOT/state_tests"
        CORPUS_DIR="$PRIVATE_ROOT/state_tests"
      fi
    fi
  fi
fi

if [ ! -d "$CORPUS_DIR" ]; then
  echo "corpus directory not found: $CORPUS_DIR" >&2
  exit 1
fi
FIXTURE_COUNT="$(find "$CORPUS_DIR" -name '*.json' | wc -l | tr -d ' ')"
echo "==> corpus: $CORPUS_DIR ($FIXTURE_COUNT fixture files)"
if [ "$FIXTURE_COUNT" -eq 0 ]; then
  echo "corpus holds no fixtures: $CORPUS_DIR" >&2
  exit 1
fi

# --- binary -----------------------------------------------------------------------------------

BIN="$REPO_ROOT/target/$PROFILE/state-test"
if [ "$BUILD" -eq 1 ]; then
  echo "==> building state-test (profile: $PROFILE)"
  (cd "$REPO_ROOT" && cargo build --profile "$PROFILE" -p state-test)
fi
if [ ! -x "$BIN" ]; then
  echo "state-test binary not found at $BIN" >&2
  exit 1
fi

# --- run --------------------------------------------------------------------------------------

LOG="$REPORT_DIR/sweep.log"
STATUS=0
if [ "$MODE" = "diff" ]; then
  echo "==> differential sweep: $TARGET_SPEC vs $BASE_SPEC"
  "$BIN" \
    --bench-spec "$TARGET_SPEC" \
    --diff-spec "$BASE_SPEC" \
    --diff-report "$REPORT_DIR/diff-report.json" \
    "$CORPUS_DIR" >"$LOG" 2>&1 || STATUS=$?
elif [ "$MODE" = "chaos" ]; then
  echo "==> chaos sweep under $TARGET_SPEC, seed $CHAOS_SEED"
  "$BIN" \
    --bench-spec "$TARGET_SPEC" \
    --chaos-seed "$CHAOS_SEED" \
    --chaos-report "$REPORT_DIR/chaos-report.json" \
    "${CHAOS_ARGS[@]+"${CHAOS_ARGS[@]}"}" \
    "$CORPUS_DIR" >"$LOG" 2>&1 || STATUS=$?
else
  # `--fill` rewrites each fixture in place, so it runs on a private copy and never touches the
  # cached corpus other runs share.
  WORK="$REPORT_DIR/fill-corpus"
  echo "==> fill sweep under $TARGET_SPEC (private copy at $WORK)"
  rm -rf "$WORK"
  mkdir -p "$WORK"
  cp -R "$CORPUS_DIR" "$WORK/"
  "$BIN" \
    --fill --force --keep-going \
    --bench-spec "$TARGET_SPEC" \
    "$WORK" >"$LOG" 2>&1 || STATUS=$?
fi

# The tally, plus anything that needs a human. Per-unit `ERR` lines are the expected noise floor
# (thousands of fixtures the runner declines before execution) and are left in the log only.
grep -vE '^ERR\b' "$LOG" | tail -n 60
echo "==> full log: $LOG"

if [ "$MODE" = "diff" ]; then
  echo "==> report: $REPORT_DIR/diff-report.json"
  # The CLI already fails on a panic or an unexplained difference, and on nothing else — fixtures
  # it declines and differences it explains leave it at 0. Pass that verdict straight through
  # rather than re-deriving it from parsed output.
  exit "$STATUS"
fi

if [ "$MODE" = "chaos" ]; then
  echo "==> report: $REPORT_DIR/chaos-report.json"
  # Same reasoning as diff mode: the CLI's own gate is the verdict.
  exit "$STATUS"
fi

# `--fill` has no notion of an expected failure: it exits non-zero for every unit it could not
# fill, and thousands of them are fixtures neither spec would execute. Re-derive the gate from the
# tally so `fill` mode fails on the same two conditions `diff` mode does.
TALLY="$(grep -m1 '^Fill tally:' "$LOG" || true)"
if [ -z "$TALLY" ]; then
  echo "fill run produced no tally line; treating as a failure" >&2
  exit 1
fi
field() { echo "$TALLY" | tr ' ' '\n' | grep "^$1=" | cut -d= -f2; }
PANICS="$(field PANIC)"
FILE_ERRS="$(field FILE_ERR)"
TOTAL="$(field TOTAL)"
# A run that reached no unit at all reports zero panics and zero file errors, truthfully, and
# says nothing. It is the one tally that must never pass.
if [ "${TOTAL:-0}" -eq 0 ]; then
  echo "gate failed: the sweep judged no unit (TOTAL=0)" >&2
  exit 1
fi
if [ "${PANICS:-0}" -gt 0 ] || [ "${FILE_ERRS:-0}" -gt 0 ]; then
  echo "gate failed: PANIC=$PANICS FILE_ERR=$FILE_ERRS" >&2
  exit 1
fi
exit 0
