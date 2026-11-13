#!/usr/bin/env bash
# Runs the mega-evm test suite at state and history byte prices other than the constants.
#
# Usage:
#   scripts/price_grid.sh --pr             five quick points (PR_POINTS below)
#   scripts/price_grid.sh --full           the whole grid (CPSB_AXIS x CPHB_AXIS, and EXTRA_POINTS)
#   scripts/price_grid.sh CPSB/CPHB ...    the points named, e.g. 312.5/20 5000/300
#
# Any of them takes `--shard I/N` (1 <= I <= N) to run only every Nth point of the selection,
# starting at the Ith, so N runs with I = 1..N cover the selection exactly once between them: the
# CI job runs the full grid as four such shards in parallel. `--list` prints the selected points,
# one per line, and exits without building or running anything.
#
# A point is a cost per state byte and a cost per history byte, as `MEGA_SATIN_CPSB` and
# `MEGA_SATIN_CPHB` take them (a decimal with at most three places); `-` on either side leaves that
# variable unset, which is the constant. The suite is built once with the `satin-price-override`
# feature, which reads the two variables at run time, and run once per point with
# `--no-fail-fast`, so every failing test of every point is reported.
#
# Each point's output lands in $OUT_DIR/<cpsb>_<cphb>.log (default target/price-grid), and the
# notes its tests left in <cpsb>_<cphb>.guards, one per line: the tests a price guard held back
# (`note_price_guard`), and apart from them the tests that skipped only a snapshot comparison,
# since snapshots are pinned at the spec's prices (`note_snapshot_skipped`). A summary line per
# point, with how many passed, were guarded, skipped a snapshot and failed, goes to stdout and,
# when $GITHUB_STEP_SUMMARY is set, a table to the step summary. Exits 1 if any point fails.
#
# The grid brackets the prices under consideration and adds the edges the code allows: a price of
# nothing on either axis, the smallest price the code represents (0.001), the cheapest price that
# charges a byte at all (1), and a point past the dearest candidate on both axes. The suite holds
# at a price of nothing and at any price from 1 up to that point; below 1 the schedule's entries
# round to nothing one at a time, and no test is held there.

set -uo pipefail

CPSB_AXIS="0 1 312.5 700 1530 2000 5000 10000"
CPHB_AXIS="0 1 20 50 88 100 200 300 1000"
EXTRA_POINTS="0.001/0.001"
# Both bytes free; both at one gas, where a premise that a charge cannot be paid holds only if a
# byte is dear; the cheapest and the dearest pair under consideration; and a point past every
# candidate. Between them they have failed every test the full grid failed.
PR_POINTS="0/0 1/1 312.5/20 5000/300 10000/1000"

FEATURES="satin-price-override,test-utils"
# The reason `note_snapshot_skipped` leaves (`test_utils::SNAPSHOT_SKIPPED`): a test that skipped a
# snapshot comparison ran every assertion it makes, so it is counted apart from the guards.
SNAPSHOT_SKIPPED="snapshot comparison skipped"

cd "$(dirname "$0")/.." || exit 1
OUT_DIR="${OUT_DIR:-target/price-grid}"

shard=""
list_only=false
selection=()
while [ "$#" -gt 0 ]; do
    case "$1" in
        --shard)
            shard="${2:-}"
            [ "$#" -ge 2 ] || { echo "error: --shard takes I/N" >&2; exit 2; }
            shift 2
            ;;
        --list)
            list_only=true
            shift
            ;;
        *)
            selection+=("$1")
            shift
            ;;
    esac
done
if [ -n "$shard" ]; then
    shard_index="${shard%/*}"
    shard_count="${shard#*/}"
    if ! [[ "$shard_index" =~ ^[0-9]+$ && "$shard_count" =~ ^[0-9]+$ ]] ||
        [ "$shard_index" -lt 1 ] || [ "$shard_index" -gt "$shard_count" ]; then
        echo "error: --shard takes I/N with 1 <= I <= N, got '$shard'" >&2
        exit 2
    fi
fi

case "${selection[0]:-}" in
    --pr)
        points="$PR_POINTS"
        ;;
    --full)
        points=""
        for cpsb in $CPSB_AXIS; do
            for cphb in $CPHB_AXIS; do
                points="$points $cpsb/$cphb"
            done
        done
        points="$points $EXTRA_POINTS"
        ;;
    "" | -h | --help)
        sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
    *)
        points="${selection[*]}"
        ;;
esac

if [ -n "$shard" ]; then
    kept=""
    position=0
    for point in $points; do
        if [ $((position % shard_count + 1)) -eq "$shard_index" ]; then
            kept="$kept $point"
        fi
        position=$((position + 1))
    done
    points="$kept"
fi

if $list_only; then
    for point in $points; do
        echo "$point"
    done
    exit 0
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
# The tests run in their package's directory, so the guard file is named by its absolute path.
out_abs="$(cd "$OUT_DIR" && pwd)"

echo "Building the test binaries with --features $FEATURES"
if ! cargo test -p mega-evm --features "$FEATURES" --locked --no-run; then
    echo "error: the test binaries do not build" >&2
    exit 1
fi

summary="| CPSB | CPHB | result | passed | guarded | snapshot skips | failed |"$'\n'"|---|---|---|---:|---:|---:|---:|"
failed_points=""
for point in $points; do
    cpsb="${point%/*}"
    cphb="${point#*/}"
    set -- env -u MEGA_SATIN_CPSB -u MEGA_SATIN_CPHB
    [ "$cpsb" != "-" ] && set -- "$@" "MEGA_SATIN_CPSB=$cpsb"
    [ "$cphb" != "-" ] && set -- "$@" "MEGA_SATIN_CPHB=$cphb"
    name="${cpsb//-/unset}_${cphb//-/unset}"
    log="$OUT_DIR/$name.log"
    guards="$out_abs/$name.guards"
    set -- "$@" "MEGA_PRICE_GUARD_LOG=$guards"

    start=$(date +%s)
    "$@" cargo test -p mega-evm --features "$FEATURES" --locked --no-fail-fast >"$log" 2>&1
    status=$?
    seconds=$(($(date +%s) - start))

    passed=$(grep -E '^test result:' "$log" | sed -E 's/.* ([0-9]+) passed.*/\1/' | awk '{s += $1} END {print s + 0}')
    failed=$(grep -cE '^test .* \.\.\. FAILED$' "$log")
    guarded=0
    snapshot_skips=0
    if [ -f "$guards" ]; then
        snapshot_skips=$(grep -c "^$SNAPSHOT_SKIPPED: " "$guards")
        guarded=$(grep -vc "^$SNAPSHOT_SKIPPED: " "$guards")
    fi
    if [ "$status" -eq 0 ]; then
        result="ok"
    else
        result="FAILED"
        failed_points="$failed_points $point"
    fi
    printf '%-12s %-7s %5s passed %4s guarded %4s snapshot skips %4s failed  %4ss  %s\n' \
        "$point" "$result" "$passed" "$guarded" "$snapshot_skips" "$failed" "$seconds" "$log"
    if [ "$status" -ne 0 ]; then
        grep -E '^test .* \.\.\. FAILED$' "$log" | sed 's/^/    /'
        # A failure that is not a test's, such as a build error, shows in the log's tail.
        [ "$failed" -eq 0 ] && tail -20 "$log" | sed 's/^/    /'
    fi
    summary="$summary"$'\n'"| $cpsb | $cphb | $result | $passed | $guarded | $snapshot_skips | $failed |"
done

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "## mega-evm at other byte prices${shard:+ (shard $shard)}"
        echo
        echo "$summary"
    } >>"$GITHUB_STEP_SUMMARY"
fi

if [ -n "$failed_points" ]; then
    echo "error: the suite fails at:$failed_points (logs in $OUT_DIR)" >&2
    exit 1
fi
echo "the suite passes at every point"
