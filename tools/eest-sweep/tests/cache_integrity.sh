#!/usr/bin/env bash
#
# Tests for the corpus-cache guards in `run.sh`.
#
# What a sweep reports is a statement about the corpus it read, so the tree it reads has to be the
# whole corpus and nothing else. These cases drive `run.sh` against a small synthetic archive and
# check that every way a cached tree can be wrong — truncated, edited, added to, left over from a
# different archive — is detected and re-extracted, and that two runs sharing a cache directory do
# not extract into each other.
#
# Usage: tools/eest-sweep/tests/cache_integrity.sh
set -uo pipefail

SUITE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN_SH="$SUITE_DIR/../run.sh"
WORK="$(mktemp -d)"
FAILURES=0
CASE=""

cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

fail() {
  echo "  FAIL: $*" >&2
  FAILURES=$((FAILURES + 1))
}

start_case() {
  CASE="$1"
  echo "==> $CASE"
}

# --- fake repository -----------------------------------------------------------------------------

# A repo root holding just what `run.sh` reads: its own directory next to a `corpus.env` naming a
# synthetic archive, and a binary that stands in for `state-test`.
ROOT="$WORK/repo"
CACHE="$WORK/cache"
REPORT="$WORK/report"
STUB_ARGS_LOG="$WORK/stub-args.log"
export STUB_ARGS_LOG
mkdir -p "$ROOT/tools/eest-sweep" "$ROOT/target/stubprofile" "$CACHE"
cp "$RUN_SH" "$ROOT/tools/eest-sweep/run.sh"

cat >"$ROOT/target/stubprofile/state-test" <<'STUB'
#!/usr/bin/env bash
# Stands in for the `state-test` binary: records the path it was handed and prints the shape of
# output `run.sh` parses. The corpus guards are what is under test, not the runner.
printf '%s\n' "${@: -1}" >>"$STUB_ARGS_LOG"
case " $* " in
  *" --fill "*) echo "Fill tally: OK=2 ERR=0 PANIC=0 FILE_ERR=0 SKIP_FILE=0 TOTAL=2" ;;
  *) echo "Differential run: Rex7 vs Rex6 over 2 unit(s)" ;;
esac
exit 0
STUB
chmod +x "$ROOT/target/stubprofile/state-test"

# A two-fixture archive shaped like the real one: `fixtures/state_tests/...`.
SRC="$WORK/src"
mkdir -p "$SRC/fixtures/state_tests/a" "$SRC/fixtures/state_tests/b"
echo '{"unit_a": {}}' >"$SRC/fixtures/state_tests/a/one.json"
echo '{"unit_b": {}}' >"$SRC/fixtures/state_tests/b/two.json"
ARCHIVE_NAME="fixtures_test.tar.gz"
RELEASE="vtest"
tar -czf "$WORK/$ARCHIVE_NAME" -C "$SRC" fixtures
if command -v sha256sum >/dev/null 2>&1; then
  ARCHIVE_SHA="$(sha256sum "$WORK/$ARCHIVE_NAME" | cut -d' ' -f1)"
else
  ARCHIVE_SHA="$(shasum -a 256 "$WORK/$ARCHIVE_NAME" | cut -d' ' -f1)"
fi
cat >"$ROOT/tools/eest-sweep/corpus.env" <<ENV
EEST_RELEASE="$RELEASE"
EEST_ARCHIVE="$ARCHIVE_NAME"
EEST_SHA256="$ARCHIVE_SHA"
EEST_URL_BASE="file:///nonexistent"
ENV
# Pre-place the archive under the name the script caches it as: nothing here downloads.
cp "$WORK/$ARCHIVE_NAME" "$CACHE/$RELEASE-$ARCHIVE_NAME"

CORPUS="$CACHE/$RELEASE/state_tests"
LOCK="$CACHE/$RELEASE.unpack.lock"

# --- helpers -------------------------------------------------------------------------------------

sweep() {
  "$ROOT/tools/eest-sweep/run.sh" --no-build --profile stubprofile \
    --cache-dir "$CACHE" --report-dir "$REPORT" "$@" >"$WORK/last.log" 2>&1
}

expect_ok() {
  local status="$1"
  [ "$status" -eq 0 ] || fail "expected exit 0, got $status: $(tail -n 3 "$WORK/last.log")"
}

# The directory's identity, which a re-extraction replaces: the tree is moved into place, never
# written into.
tree_id() {
  ls -di "$CORPUS" 2>/dev/null | awk '{print $1}'
}

log_has() {
  grep -q "$1" "$WORK/last.log" || fail "log should mention '$1': $(cat "$WORK/last.log")"
}

log_lacks() {
  grep -q "$1" "$WORK/last.log" && fail "log should not mention '$1': $(cat "$WORK/last.log")"
}

corpus_is_whole() {
  [ -f "$CORPUS/a/one.json" ] && [ -f "$CORPUS/b/two.json" ] && [ -f "$CORPUS/.manifest" ]
}

# --- cases ---------------------------------------------------------------------------------------

start_case "a cold cache unpacks the corpus and sweeps it"
sweep
expect_ok "$?"
log_has "unpacking state_tests"
corpus_is_whole || fail "the tree is not whole after a cold run"
grep -q "^archive-sha256 $ARCHIVE_SHA$" "$CORPUS/.manifest" ||
  fail "the manifest should name the archive it came from"
[ "$(grep -c . "$CORPUS/.manifest")" -eq 3 ] || fail "manifest should list both fixtures"
FIRST_ID="$(tree_id)"

start_case "a warm cache is verified against the manifest, not re-extracted"
sweep
expect_ok "$?"
log_has "verified against its manifest"
log_lacks "unpacking state_tests"
[ "$(tree_id)" = "$FIRST_ID" ] || fail "the tree was replaced despite being intact"

start_case "a fixture edited under the cache is detected and the tree re-extracted"
echo '{"tampered": true}' >"$CORPUS/a/one.json"
sweep
expect_ok "$?"
log_has "unpacking state_tests"
[ "$(cat "$CORPUS/a/one.json")" = '{"unit_a": {}}' ] || fail "the edit survived the re-extraction"
[ "$(tree_id)" != "$FIRST_ID" ] || fail "the tree should have been replaced"

start_case "a fixture missing from the cache is detected"
rm "$CORPUS/b/two.json"
sweep
expect_ok "$?"
log_has "unpacking state_tests"
corpus_is_whole || fail "the missing fixture should be back"

start_case "a file added under the cache is detected"
echo '{"stray": true}' >"$CORPUS/a/stray.json"
sweep
expect_ok "$?"
log_has "unpacking state_tests"
[ -f "$CORPUS/a/stray.json" ] && fail "the stray fixture should be gone"

start_case "a tree left by a different archive is not reused"
# The manifest describes this tree correctly and names another archive: it is a complete corpus,
# but not the one the sweep is pinned to.
sed -i.bak "1s/.*/archive-sha256 0000000000000000000000000000000000000000000000000000000000000000/" \
  "$CORPUS/.manifest"
rm -f "$CORPUS/.manifest.bak"
sweep
expect_ok "$?"
log_has "unpacking state_tests"
grep -q "^archive-sha256 $ARCHIVE_SHA$" "$CORPUS/.manifest" ||
  fail "the re-extracted tree should name the pinned archive"

start_case "a truncated tree cannot be swept as a whole one"
# What an interrupted extraction leaves behind, in the shape a cache restore would preserve.
rm -rf "${CORPUS:?}/b"
sweep
expect_ok "$?"
corpus_is_whole || fail "the truncated tree should have been replaced"

start_case "a run that finds the lock held falls back to a private tree"
# Nobody holds this lock, but a live producer is indistinguishable from a dead one, and the
# waiter's job is the same either way: never write into a destination another process owns.
rm -rf "$CORPUS"
mkdir -p "$LOCK"
: >"$STUB_ARGS_LOG"
EEST_UNPACK_LOCK_WAIT_SECS=1 sweep
expect_ok "$?"
log_has "unpacking a private tree"
[ -d "$CORPUS" ] && fail "the waiter must not create the shared tree"
SWEPT="$(tail -n 1 "$STUB_ARGS_LOG")"
case "$SWEPT" in
  *"/.private."*) ;;
  *) fail "the sweep should have run against the private tree, got '$SWEPT'" ;;
esac
[ -e "$SWEPT" ] && fail "the private tree should be removed when the run ends"
rmdir "$LOCK"

start_case "two runs sharing a cache do not extract into each other"
rm -rf "$CORPUS"
sweep &
FIRST=$!
"$ROOT/tools/eest-sweep/run.sh" --no-build --profile stubprofile \
  --cache-dir "$CACHE" --report-dir "$WORK/report2" >"$WORK/second.log" 2>&1 &
SECOND=$!
wait "$FIRST"
FIRST_STATUS=$?
wait "$SECOND"
SECOND_STATUS=$?
[ "$FIRST_STATUS" -eq 0 ] || fail "the first concurrent run failed: $(tail -n 3 "$WORK/last.log")"
[ "$SECOND_STATUS" -eq 0 ] || fail "the second concurrent run failed: $(tail -n 3 "$WORK/second.log")"
corpus_is_whole || fail "the shared tree is not whole after two concurrent runs"
[ -d "$LOCK" ] && fail "the lock should be released"
ls -d "$CACHE"/.private.* >/dev/null 2>&1 && fail "no private tree should be left behind"
ls -d "$CACHE"/.unpack.* >/dev/null 2>&1 && fail "no scratch directory should be left behind"

start_case "fill mode runs against a private copy of the verified tree"
: >"$STUB_ARGS_LOG"
sweep --mode fill
expect_ok "$?"
log_has "verified against its manifest"
SWEPT="$(tail -n 1 "$STUB_ARGS_LOG")"
case "$SWEPT" in
  "$REPORT/fill-corpus"*) ;;
  *) fail "fill mode should sweep its own copy, got '$SWEPT'" ;;
esac

# --- verdict -------------------------------------------------------------------------------------

if [ "$FAILURES" -ne 0 ]; then
  echo "$FAILURES check(s) failed" >&2
  exit 1
fi
echo "all corpus-cache checks passed"
