#!/usr/bin/env bash
# Runs the mega-evm test suite at state and history byte prices other than the constants.
#
# Usage:
#   scripts/price_grid.sh --pr             the pull-request points (PR_POINTS below)
#   scripts/price_grid.sh --full           the whole grid (CPSB_AXIS x CPHB_AXIS, and EXTRA_POINTS)
#   scripts/price_grid.sh CPSB/CPHB ...    the points named, e.g. 312.5/20 5000/300
#
# A point is a cost per state byte and a cost per history byte, as `MEGA_SATIN_CPSB` and
# `MEGA_SATIN_CPHB` take them (a decimal with at most three places); `-` on either side leaves that
# variable unset, which is the constant. The suite is built once with the `satin-price-override`
# feature, which reads the two variables at run time, and run once per point with
# `--no-fail-fast`, so every failing test of every point is reported.
#
# Each point's output lands in $OUT_DIR/<cpsb>_<cphb>.log (default target/price-grid). A summary
# line per point goes to stdout and, when $GITHUB_STEP_SUMMARY is set, a table to the step summary.
# Exits 1 if any point fails.
#
# The grid brackets the prices under consideration and adds the edges the code allows: a price of
# nothing on either axis, the smallest price the code represents (0.001), and a point past the
# dearest candidate on both axes.

set -uo pipefail

CPSB_AXIS="0 312.5 700 1530 2000 5000 10000"
CPHB_AXIS="0 20 50 88 100 200 300 1000"
EXTRA_POINTS="0.001/0.001"
# Both bytes free; the cheapest and the dearest pair under consideration; and a point past every
# candidate. Before the suite was made to hold at any price, these four failed every test the
# full grid failed.
PR_POINTS="0/0 312.5/20 5000/300 10000/1000"

FEATURES="satin-price-override,test-utils"

cd "$(dirname "$0")/.." || exit 1
OUT_DIR="${OUT_DIR:-target/price-grid}"

case "${1:-}" in
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
        sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
    *)
        points="$*"
        ;;
esac

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

echo "Building the test binaries with --features $FEATURES"
if ! cargo test -p mega-evm --features "$FEATURES" --locked --no-run; then
    echo "error: the test binaries do not build" >&2
    exit 1
fi

summary="| CPSB | CPHB | result | passed | failed |"$'\n'"|---|---|---|---:|---:|"
failed_points=""
for point in $points; do
    cpsb="${point%/*}"
    cphb="${point#*/}"
    set -- env -u MEGA_SATIN_CPSB -u MEGA_SATIN_CPHB
    [ "$cpsb" != "-" ] && set -- "$@" "MEGA_SATIN_CPSB=$cpsb"
    [ "$cphb" != "-" ] && set -- "$@" "MEGA_SATIN_CPHB=$cphb"
    log="$OUT_DIR/${cpsb//-/unset}_${cphb//-/unset}.log"

    start=$(date +%s)
    "$@" cargo test -p mega-evm --features "$FEATURES" --locked --no-fail-fast >"$log" 2>&1
    status=$?
    seconds=$(($(date +%s) - start))

    passed=$(grep -E '^test result:' "$log" | sed -E 's/.* ([0-9]+) passed.*/\1/' | awk '{s += $1} END {print s + 0}')
    failed=$(grep -cE '^test .* \.\.\. FAILED$' "$log")
    if [ "$status" -eq 0 ]; then
        result="ok"
    else
        result="FAILED"
        failed_points="$failed_points $point"
    fi
    printf '%-12s %-7s %5s passed %4s failed  %4ss  %s\n' "$point" "$result" "$passed" "$failed" "$seconds" "$log"
    if [ "$status" -ne 0 ]; then
        grep -E '^test .* \.\.\. FAILED$' "$log" | sed 's/^/    /'
        # A failure that is not a test's, such as a build error, shows in the log's tail.
        [ "$failed" -eq 0 ] && tail -20 "$log" | sed 's/^/    /'
    fi
    summary="$summary"$'\n'"| $cpsb | $cphb | $result | $passed | $failed |"
done

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "## mega-evm at other byte prices"
        echo
        echo "$summary"
    } >>"$GITHUB_STEP_SUMMARY"
fi

if [ -n "$failed_points" ]; then
    echo "error: the suite fails at:$failed_points (logs in $OUT_DIR)" >&2
    exit 1
fi
echo "the suite passes at every point"
