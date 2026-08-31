#!/usr/bin/env bash
#
# Run the EEST state-test corpus through the mega-evm state-test runner.
#
# One command: fetch and verify the pinned fixture release, unpack its `state_tests` subtree, and
# execute every fixture. Two gates fail the run — a fixture that panics, and a difference between
# the spec under test and its frozen base that no MegaETH mechanism accounts for. Everything else
# (fixtures the runner declines, differences the classifier explains) is reported and does not
# fail.
#
# Usage:
#   tools/eest-sweep/run.sh [options]
#
#   --target-spec SPEC   Spec under test (default: Rex7)
#   --base-spec SPEC     Frozen spec to compare against (default: Rex6)
#   --mode diff|fill     diff: execute under both specs and classify the differences (default).
#                        fill: execute under the target spec only and recompute each fixture's
#                        `post` in place, on a private copy. `diff` runs the target spec through
#                        the same execution path, so it already covers what `fill` scans for;
#                        `fill` remains available to exercise the fixture-writing path itself.
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
  diff|fill) ;;
  *) echo "--mode must be 'diff' or 'fill', got '$MODE'" >&2; exit 2 ;;
esac

# `sha256sum` on Linux, `shasum -a 256` on macOS.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

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
  if [ ! -d "$CORPUS_DIR" ]; then
    echo "==> unpacking state_tests"
    mkdir -p "$CACHE_DIR/$EEST_RELEASE"
    # Only `state_tests` is unpacked: the runner reads the state-test format, and the archive's
    # blockchain-test subtrees are several times larger.
    tar -xzf "$ARCHIVE" -C "$CACHE_DIR/$EEST_RELEASE" --strip-components=1 fixtures/state_tests
  fi
fi

if [ ! -d "$CORPUS_DIR" ]; then
  echo "corpus directory not found: $CORPUS_DIR" >&2
  exit 1
fi
FIXTURE_COUNT="$(find "$CORPUS_DIR" -name '*.json' | wc -l | tr -d ' ')"
echo "==> corpus: $CORPUS_DIR ($FIXTURE_COUNT fixture files)"

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
if [ "${PANICS:-0}" -gt 0 ] || [ "${FILE_ERRS:-0}" -gt 0 ]; then
  echo "gate failed: PANIC=$PANICS FILE_ERR=$FILE_ERRS" >&2
  exit 1
fi
exit 0
