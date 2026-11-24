#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COV_DIR="${COV_DIR:-$ROOT_DIR/target/coverage/mega-evm}"
HTML_INDEX="$COV_DIR/html/index.html"
LCOV_PATH="$COV_DIR/lcov.info"
REPORT="$COV_DIR/report.txt"
IGNORE_FILENAME_REGEX="${IGNORE_FILENAME_REGEX:-(/tests/|/benches/|/examples/|/src/test_utils/|/external/test_utils\\.rs|/\\.cargo/registry/|/rustc/)}"

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "cargo-llvm-cov is required. Install it with 'cargo install cargo-llvm-cov --locked'." >&2
    exit 1
fi

cd "$ROOT_DIR"
rm -rf "$COV_DIR"
mkdir -p "$COV_DIR"

cargo llvm-cov clean --workspace

cargo llvm-cov \
    --locked \
    --package mega-evm \
    --all-features \
    --branch \
    --lib \
    --tests \
    --no-fail-fast \
    --no-report

# The Mega EEST cases (scripts/mega_eest.sh) through the suite's revm adapter, built against this
# tree with mega-evm and the adapter's own crate instrumented as the tests are — mega-evm's generic
# code is compiled where the adapter uses it — and the crates.io and git crates not, so a line only
# a Mega EEST case runs counts as covered. The adapter's and the suite's own sources are left out of
# the report. The adapter's profiles go where cargo-llvm-cov merges the tests' (the target directory
# of the run above), merged online into one file per pool slot, as the suite runs one adapter
# process per case; the run must leave a non-empty profile, or the suite contributed nothing. The
# report reads only the executables of this workspace's targets, which the adapter is not, so a
# copy of the adapter this run built (the path the script records) is placed among them under the
# library's name; the lcov report's own command line is then checked to have read it. The pin's
# provenance is the gate's to check, not this report's.
# COVERAGE_MEGA_EEST=0 leaves the suite out, for a quicker local report of the tests alone.
ADAPTER_OBJECT=""
if [ "${COVERAGE_MEGA_EEST:-1}" != 0 ]; then
    LLVM_COV_TARGET="${CARGO_LLVM_COV_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps \
        | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')/llvm-cov-target}"
    EEST_WORK="$ROOT_DIR/target/mega-eest-coverage"
    IGNORE_FILENAME_REGEX="$IGNORE_FILENAME_REGEX|^$(python3 -c 'import re, sys; print(re.escape(sys.argv[1]))' "$EEST_WORK")/"
    rm -f "$LLVM_COV_TARGET"/mega-eest-*.profraw
    # Line tables are debug info enough for a panic's backtrace; the report reads the coverage
    # mapping, not the debug info.
    CARGO_PROFILE_DEV_DEBUG=line-tables-only \
        LLVM_PROFILE_FILE="$LLVM_COV_TARGET/mega-eest-%15m.profraw" \
        MEGA_EEST_WORK="$EEST_WORK" \
        MEGA_EEST_SKIP_PROVENANCE=1 \
        MEGA_EEST_ENGINE_RUSTFLAGS="-Cinstrument-coverage -Zcoverage-options=branch --cfg=coverage --cfg=coverage_nightly" \
        "$ROOT_DIR/scripts/mega_eest.sh"
    profiles=$(find "$LLVM_COV_TARGET" -maxdepth 1 -name 'mega-eest-*.profraw' -size +0c | wc -l)
    if [ "$profiles" -eq 0 ]; then
        echo "the Mega EEST run left no coverage profile in $LLVM_COV_TARGET: the adapter it ran was" \
            "not instrumented, or its profiles went elsewhere, and the report would leave the suite out" >&2
        exit 1
    fi
    ADAPTER_OBJECT="mega_evm-00000000ee57ada7"
    cp "$(cat "$EEST_WORK/adapter.path")" "$LLVM_COV_TARGET/debug/deps/$ADAPTER_OBJECT"
fi

status=0
cargo llvm-cov report \
    --verbose \
    --package mega-evm \
    --branch \
    --ignore-filename-regex "$IGNORE_FILENAME_REGEX" \
    --lcov \
    --output-path "$LCOV_PATH" 2> "$COV_DIR/lcov-report.log" || status=$?
cat "$COV_DIR/lcov-report.log" >&2
[ "$status" -eq 0 ] || exit "$status"
if [ -n "$ADAPTER_OBJECT" ] && ! grep -q -- "$ADAPTER_OBJECT" "$COV_DIR/lcov-report.log"; then
    echo "the lcov report did not read the Mega EEST adapter ($ADAPTER_OBJECT): cargo-llvm-cov no" \
        "longer picks up executables as this script expects, and the report would leave the suite out" >&2
    exit 1
fi

cargo llvm-cov report \
    --package mega-evm \
    --branch \
    --ignore-filename-regex "$IGNORE_FILENAME_REGEX" \
    --html \
    --output-dir "$COV_DIR"

cargo llvm-cov report \
    --package mega-evm \
    --branch \
    --ignore-filename-regex "$IGNORE_FILENAME_REGEX" > "$REPORT"

echo
echo "Coverage artifacts written to $COV_DIR"
echo "Open $HTML_INDEX to inspect the HTML report."
echo "Upload $LCOV_PATH to Codecov to annotate pull requests."
