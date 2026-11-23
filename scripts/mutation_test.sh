#!/usr/bin/env bash
# Mutation-testing driver for mega-evm.
#
# Subcommands:
#   diff  <base-ref> [--list]
#                      Mutate only lines changed vs <base-ref> (PR gate mode).
#                      With --list, print that mutant set without running it.
#   full  [args...]    Mutate the whole mega-evm crate (nightly mode; slow).
#                      Extra arguments go to cargo-mutants (e.g. `-f <file>`,
#                      `--shard k/n`).
#   file  <glob>       Mutate files matching <glob> (local iteration).
#   infra [args...]    Mutate the test gates' own infrastructure (see INFRA_FILES below).
#   list  [--all] [args...]
#                      Print the mutants `full` would test, one per line, without
#                      building or testing anything (`cargo mutants --list`).
#                      Extra arguments go to cargo-mutants, as for `full`. With
#                      --all, include the mutants of function-scoped suppressions,
#                      which the runs exclude.
#   changed <base-ref> Print mutate=true if `diff <base-ref>` would build, and
#                      mutate=false if it would find no mutatable change. CI uses
#                      it to decide whether to restore and save the build cache.
#
# `diff`, `full` and `file` run the production scope of .cargo/mutants.toml, which excludes test
# helpers as noise. `infra` runs the second scope, .cargo/mutants-infra.toml: the scenario runner
# is helper code by that rule, but the benches and later tests rest on it, so it is mutated on its
# own.
#
# Every run first builds the unmutated tests into ./target (the warm build), which
# cargo-mutants' `copy_target` copies into each scratch build directory; with
# incremental compilation on (CARGO_INCREMENTAL=1, which CI sets: the toolchain
# action turns it off), the copy carries mega-evm's incremental state too, so each
# mutant build recompiles only what its mutation changed. A diff with no mutatable
# change skips the warm build, since nothing will be built.
#
# Results land in $OUT_DIR/mutants.out/ (missed.txt, caught.txt, outcomes.json).
# When cargo-mutants has nothing to mutate and writes no results, which it does
# when --in-diff leaves no mutant, the driver records its saying so there
# instead. A mutant that timed out is re-run once, alone, into
# $OUT_DIR/recheck/mutants.out/; a re-check that exits abnormally has its exit
# recorded there, in run-failed.txt, and the gate refuses it. Run
# scripts/mutation_gate.py afterwards to score + gate the run, passing both and
# the status file below.
#
# Time budget: MUTANTS_BUDGET=<seconds>, counted from this script's start, bounds
# the warm build, cargo-mutants, the re-check and the Mega EEST pass together
# (unset: no limit; needs GNU timeout). When it runs out, the running phase gets SIGINT, so
# cargo-mutants stops its in-flight mutants and keeps every outcome it has
# recorded; it gets SIGKILL if it is still running MUTANTS_GRACE seconds later.
# A follow-up run (the re-check, the Mega EEST pass) the budget stops has that
# recorded in its results (run-failed.txt), and the status says so. The script then exits 0 with what was tested, so the caller
# can still score and upload it.
#
# The last line printed, also written to $OUT_DIR/status, is
#   MUTANTS-STATUS status=<status> expected=<n|unknown> outcomes=<n>
# expected: mutants planned (unknown if the run stopped before cargo-mutants
# planned them); outcomes: mutants with an outcome; status is one of
#   complete          every planned mutant has an outcome, and no follow-up run
#                     was stopped;
#   partial           the budget ran out after some mutants were tested;
#   not-started       the budget ran out before any mutant was tested;
#   followup-stopped  every planned mutant was tested, and the budget ran out in
#                     the re-check of a timed-out one or the Mega EEST pass;
#   baseline-failed   the unmutated tree failed to build or to pass its tests;
#   error             the run itself failed (bad arguments, an internal error).
# Only baseline-failed and error exit non-zero. `mutation_gate.py report --status`
# fails the gate on anything but complete.
#
# MUTANTS_SHARD=k/n runs only shard k (0-based) of n of whichever mutant set the
# subcommand selects (cargo-mutants' own --shard); give each shard its own
# OUT_DIR and score the shards together against the unsharded inventory.
# The nightly full run passes `--shard k/n` to `full` instead.
#
# In the production scope (diff, full, file), a mutant the tests missed — in
# the run, or in the re-check of one that timed out — is run once more against
# the Mega EEST conformance suite, into $OUT_DIR/mega-eest/mutants.out: the test
# command is mega-evm's ignored `mega_eest` test alone, which runs
# scripts/mega_eest.sh (plain mode) on the mutated tree, so a mutant that only a
# Mega EEST case catches counts as caught. Pass that directory to the gate as
# --mega-eest. The gate counts such a kill only when the mutant's log holds the
# suite's own verdict that cases failed (its `MEGA-EEST ... result=FAIL` line),
# so a run that failed before it reached the cases (a build, a fetch) is no
# kill: the gate refuses the results instead. The suite is fetched and its
# adapter built once, on the unmutated tree, before the run, and the mutants
# then run one at a time (--jobs 1), each using the machine's parallelism for
# the suite's cases; a warm-up that fails has its exit recorded in those results
# (run-failed.txt), which the gate refuses. MUTANTS_MEGA_EEST=0 skips the pass,
# and the gate then counts those mutants as survivors.

# Bash 5 or newer: the driver relies on mapfile and on expanding empty arrays
# under `set -u`, which bash 3.2 (macOS's /bin/bash) does not have. Written in
# POSIX sh so the refusal itself runs under any shell.
case "${BASH_VERSION:-}" in
    [5-9].* | [1-9][0-9].*) ;;
    *)
        echo "scripts/mutation_test.sh needs bash 5 or newer; this shell is ${BASH_VERSION:+bash }${BASH_VERSION:-not bash}." >&2
        echo "Install a newer bash (on macOS: brew install bash) and run it as 'bash scripts/mutation_test.sh ...' with that bash first on PATH." >&2
        exit 1
        ;;
esac
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/mutants}"

cd "$ROOT_DIR"

SUPPRESS="${SUPPRESS:-$ROOT_DIR/mutants/suppressions.toml}"
# The files cargo-mutants can mutate: the crate's Rust sources, at any depth. The
# `:(glob)` magic lets `**/` match no directory, so the files at the top of src/
# are in; git's default pathspec would need a second `/` there and leave them out.
SRC_PATHSPEC=':(glob)crates/mega-evm/src/**/*.rs'
# macOS has no nproc. The count then comes from sysctl, then from POSIX getconf.
JOBS="${JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || getconf _NPROCESSORS_ONLN)}"
PKG_ARGS=(--package mega-evm)
CONFIG_ARGS=()
BUDGET="${MUTANTS_BUDGET:-}"
GRACE="${MUTANTS_GRACE:-120}"

# The test gates' own executable logic: the scenario runner's transaction conversion and its
# execute/commit loop.
INFRA_FILES=(
    crates/mega-evm/src/test_utils/scenario.rs
)

cmd="${1:-}"
shift || true

# The diff `diff` mutates: only the crate's Rust sources are mutatable, so a PR
# that touches only tests/, Cargo.toml or a module guide under src/ has nothing
# to mutate.
mutatable_diff() {
    git diff --no-color "$1"...HEAD -- "$SRC_PATHSPEC"
}

if [[ "$cmd" == changed ]]; then
    base="${1:?usage: mutation_test.sh changed <base-ref>}"
    # Read the diff into a variable: piping it into `grep -q` would fail the
    # pipeline under pipefail when grep exits early.
    changes="$(mutatable_diff "$base")"
    if [[ -n "$changes" ]]; then echo "mutate=true"; else echo "mutate=false"; fi
    exit 0
fi

require_cargo_mutants() {
    if ! cargo mutants --version >/dev/null 2>&1; then
        echo "cargo-mutants is required. Install with 'cargo install cargo-mutants --locked'." >&2
        exit 1
    fi
}

# Function-scoped suppressions become --exclude-re so they are never generated
# (saves build/test time), unless called with "all"; line-scoped suppressions are
# filtered later by the gate. Capture into a variable first: `mapfile < <(...)`
# would mask a non-zero exit from the helper (a malformed suppressions.toml),
# letting the run continue without the function-scoped excludes.
EXCLUDE_ARGS=()
load_excludes() {
    local exclude_re_output
    EXCLUDE_ARGS=()
    [[ "${1:-}" != all ]] || return 0
    if ! exclude_re_output="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" exclude-re --suppressions "$SUPPRESS")"; then
        echo "failed to read the suppressions in $SUPPRESS" >&2
        exit 1
    fi
    [[ -n "$exclude_re_output" ]] && mapfile -t EXCLUDE_ARGS <<< "$exclude_re_output"
    return 0
}

if [[ "$cmd" == list ]]; then
    scope=""
    if [[ "${1:-}" == --all ]]; then
        scope=all
        shift
    fi
    require_cargo_mutants
    load_excludes "$scope"
    # --colors never: CI forces CARGO_TERM_COLOR=always, and colour codes in the
    # list would break the exact matching against it.
    exec cargo mutants --list --colors never "${PKG_ARGS[@]}" "${EXCLUDE_ARGS[@]}" "$@"
fi

# A listing (`diff --list` or `infra --list`) writes no results and has no status.
LISTING=""
[[ " $* " == *" --list "* ]] && LISTING=1

SHARD_ARGS=()
[[ -n "${MUTANTS_SHARD:-}" ]] && SHARD_ARGS=(--shard "$MUTANTS_SHARD")

# cargo-mutants exit codes (https://mutants.rs/exit-codes.html): 0 = all caught,
# 2 = missed mutants, 3 = timeouts. Those three are normal run outcomes that the
# gate (scripts/mutation_gate.py) is responsible for scoring. 4 means the unmutated
# tree failed its tests. Everything else (1 usage, 5/6 bad --in-diff, 70 internal)
# is a real failure of the run itself.
run_outcome() {
    case "$1" in
        0 | 2 | 3) return 0 ;;
        *) return "$1" ;;
    esac
}

# How the run ended, classified into a status by `finish`: ran, stopped (by the
# budget), baseline-failed. Any other exit, including one `set -e` forces, reports
# status=error. FOLLOWUP_STOPPED is set when the budget stopped a follow-up run.
RESULT=""
FOLLOWUP_STOPPED=""
# Set once this run has cleared the previous results; until then there is
# nothing of this run's to count.
CLEARED=""

# shellcheck disable=SC2329 # the EXIT trap below runs it
finish() {
    local rc=$? status=error expected=unknown outcomes=0 counts counted=1
    if [[ -n "$CLEARED" ]]; then
        if counts="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" progress --results "$OUT_DIR/mutants.out")"; then
            expected="${counts#expected=}"
            expected="${expected%% *}"
            outcomes="${counts##*outcomes=}"
        else
            # A record that cannot be counted (cut off while written) says nothing of how far the
            # run got: the status is error, whatever the run's exit.
            counted=""
        fi
    fi
    case "$RESULT" in
        ran)
            # cargo-mutants exits 0 without an output directory when no mutant
            # intersects the diff: nothing was planned.
            [[ "$expected" == unknown && -n "$counted" ]] && expected=0
            [[ "$expected" == "$outcomes" ]] && status=complete
            ;;
        stopped)
            if [[ "$cmd" != diff && "$expected" == "$outcomes" ]]; then
                status=complete
            elif ((outcomes > 0)); then
                status=partial
            else
                status=not-started
            fi
            ;;
        baseline-failed) status=baseline-failed ;;
    esac
    [[ -n "$counted" ]] || status=error
    [[ "$status" == complete && -n "$FOLLOWUP_STOPPED" ]] && status=followup-stopped
    if [[ "$cmd" == diff && "$status" == complete ]] && ((rc != 0)); then status=error; fi
    local line="MUTANTS-STATUS status=$status expected=$expected outcomes=$outcomes"
    mkdir -p "$OUT_DIR"
    echo "$line" > "$OUT_DIR/status"
    echo
    echo "$line"
    if [[ "$status" == error || "$status" == baseline-failed ]] && ((rc == 0)); then
        exit 1
    fi
}

# Run "$@" within what is left of MUTANTS_BUDGET. Returns 124 if the budget
# stopped it (or had already run out), otherwise the command's exit status.
TIMEOUT=""
run_bounded() {
    [[ -n "$BUDGET" ]] || {
        "$@"
        return
    }
    local left=$((BUDGET - SECONDS)) rc=0
    ((left > 0)) || return 124
    # GNU timeout signals the command's whole process group. cargo-mutants runs
    # its cargo and test processes in their own groups and stops them itself.
    "$TIMEOUT" --signal=INT --kill-after="$GRACE" "$left" "$@" || rc=$?
    # 124: stopped by SIGINT; 137: killed after the grace period.
    if ((rc == 137 && SECONDS >= BUDGET)); then rc=124; fi
    return "$rc"
}
if [[ -n "$BUDGET" ]]; then
    TIMEOUT="$(command -v timeout || command -v gtimeout || true)"
    if [[ -z "$TIMEOUT" ]]; then
        echo "MUTANTS_BUDGET needs GNU timeout (coreutils)." >&2
        exit 1
    fi
fi

require_cargo_mutants
load_excludes

if [[ -z "$LISTING" ]]; then
    trap finish EXIT
    # Clear the previous run's results before anything that can fail, so a run that
    # stops early leaves nothing behind for the gate to mistake for its results.
    rm -rf "$OUT_DIR"
    mkdir -p "$OUT_DIR"
    CLEARED=1
fi

# The arguments that restrict the first run (an `--in-diff` file, `-f` globs), which the
# re-check and the Mega EEST run take too, without the first run's `--shard`. The follow-ups
# select their mutants by name (`--re`), and cargo-mutants 27.1.0 adds every struct-field-deletion
# mutant without matching it against `--re`; under the first run's restriction those extras are at
# most the ones the first run tested, whose outcomes there the gate does not read. A shard, though,
# is applied after the selection: it would cut the follow-ups' own short list, extras included,
# and leave out mutants they were asked to decide, so it is never passed on. A run with no
# restriction (the full run, sharded or not) has the follow-ups name the files of their mutants
# instead (FOLLOWUP_FILES), which keeps the extras to those files.
SCOPE_ARGS=()
FOLLOWUP_FILES=()

# WARM=0 skips the warm build: a diff with no mutatable change builds nothing.
WARM=1

# The first run's arguments without `--shard k/n` or `--shard=k/n`; and whether they restrict
# the mutants to files or a diff.
set_scope() {
    local arg skip="" restricted=""
    SCOPE_ARGS=()
    for arg in "$@"; do
        if [[ -n "$skip" ]]; then
            skip=""
            continue
        fi
        case "$arg" in
            --shard) skip=1 ;;
            --shard=*) ;;
            *) SCOPE_ARGS+=("$arg") ;;
        esac
        case "$arg" in
            -f | --file | --file=* | --in-diff | --in-diff=*) restricted=1 ;;
        esac
    done
    FOLLOWUP_FILES=()
    [[ -n "$restricted" ]] || FOLLOWUP_FILES=(--with-files)
}

run_mutants() {
    set_scope "$@"
    # --no-shuffle: test mutants in deterministic source order so runs are
    #   reproducible and comparable (recommended by https://mutants.rs/pr-diff.html).
    # -vV: verbose progress + version banner, for diagnosable CI logs.
    local args=(
        "${CONFIG_ARGS[@]}"
        "${PKG_ARGS[@]}"
        --jobs "$JOBS"
        --output "$OUT_DIR"
        --no-shuffle
        -vV
        "${EXCLUDE_ARGS[@]}"
        "${SHARD_ARGS[@]}"
        "$@"
    )
    # A listing writes no results: there is nothing to build, record or re-check.
    if [[ -n "$LISTING" ]]; then
        cargo mutants "${args[@]}"
        return
    fi
    local rc=0
    if [[ "$WARM" == 1 ]]; then
        # Build the unmutated tests into ./target. `copy_target` copies it into every
        # scratch build directory, so the mutant builds reuse the compiled dependencies
        # and, with incremental compilation, the compiled mega-evm too.
        echo "Warm build (CARGO_INCREMENTAL=${CARGO_INCREMENTAL-unset}, budget ${BUDGET:-none}s)"
        run_bounded cargo nextest run "${PKG_ARGS[@]}" --all-features --no-run || rc=$?
        case "$rc" in
            0) ;;
            124) RESULT=stopped && return 0 ;;
            *) RESULT=baseline-failed && return "$rc" ;;
        esac
    fi
    run_bounded cargo mutants "${args[@]}" 2>&1 | tee "$OUT_DIR.log" || rc=$?
    case "$rc" in
        0 | 2 | 3) RESULT=ran ;;
        124)
            RESULT=stopped
            return 0
            ;;
        4) RESULT=baseline-failed && return "$rc" ;;
        *) return "$rc" ;;
    esac
    python3 "$ROOT_DIR/scripts/mutation_gate.py" note-empty \
        --log "$OUT_DIR.log" --results "$OUT_DIR/mutants.out"
    recheck_timeouts
    # The infrastructure scope mutates the scenario runner, which the suite's adapter never runs.
    if [[ ${#CONFIG_ARGS[@]} -eq 0 && "${MUTANTS_MEGA_EEST:-1}" != 0 ]]; then
        run_missed_against_mega_eest
    fi
}

# A follow-up run that did not finish as a run proves nothing by the files it
# left, which can look like a finished run's: its exit, or the budget that
# stopped it, goes into its results, which the gate then refuses to score, saying
# why, rather than the job ending here without a report.
record_failed_followup() {
    local results=$1 rc=$2 what=$3
    mkdir -p "$results"
    if [[ "$rc" == 124 ]]; then
        FOLLOWUP_STOPPED=1
        echo "$what was stopped by MUTANTS_BUDGET" >&2
        echo "stopped by MUTANTS_BUDGET (${BUDGET}s)" > "$results/run-failed.txt"
    else
        echo "$what failed (exit $rc)" >&2
        echo "cargo-mutants exited $rc" > "$results/run-failed.txt"
    fi
}

# A mutant that timed out was tested beside $JOBS others, each running the whole
# suite on the same cores, so it may have timed out on the contention alone.
# Re-run each once, alone; the gate takes that outcome for it. One that is
# caught or survives alone is decided; one that times out again stays
# inconclusive. Suppressed timeouts are not re-run.
recheck_timeouts() {
    local re_output
    if ! re_output="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" timeout-re \
        --results "$OUT_DIR/mutants.out" --suppressions "$SUPPRESS" "${FOLLOWUP_FILES[@]}")"; then
        echo "failed to list the timed-out mutants in $OUT_DIR/mutants.out" >&2
        return 1
    fi
    [[ -n "$re_output" ]] || return 0
    local re_args=()
    mapfile -t re_args <<< "$re_output"
    echo "Re-checking $(grep -c '^--re$' <<< "$re_output") timed-out mutant(s) one at a time" >&2
    local rc=0
    run_bounded cargo mutants \
        "${CONFIG_ARGS[@]}" \
        "${PKG_ARGS[@]}" \
        --jobs 1 \
        --output "$OUT_DIR/recheck" \
        --no-shuffle \
        -vV \
        "${SCOPE_ARGS[@]}" \
        "${re_args[@]}" 2>&1 | tee "$OUT_DIR/recheck.log" || rc=$?
    if ! run_outcome "$rc"; then
        record_failed_followup "$OUT_DIR/recheck/mutants.out" "$rc" \
            "the re-check of the timed-out mutants (see $OUT_DIR/recheck.log)"
    fi
}

# A mutant the tests missed may still change an outcome a Mega EEST case pins. Run
# each such mutant once more with mega-evm's ignored `mega_eest` test as the only
# test (`--test mega_eest`, nextest's `--run-ignored only`); the gate takes that
# outcome for it. The suite is fetched and its adapter built on the unmutated
# tree first, which also proves the suite passes there; every build directory
# then shares that checkout, read-only, and builds the adapter against its own
# mutated mega-evm.
run_missed_against_mega_eest() {
    local re_output
    if ! re_output="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" missed-re \
        --results "$OUT_DIR/mutants.out" --recheck "$OUT_DIR/recheck/mutants.out" \
        --suppressions "$SUPPRESS" "${FOLLOWUP_FILES[@]}")"; then
        echo "failed to list the missed mutants in $OUT_DIR/mutants.out" >&2
        return 1
    fi
    [[ -n "$re_output" ]] || return 0
    local re_args=()
    mapfile -t re_args <<< "$re_output"
    local results="$OUT_DIR/mega-eest/mutants.out"
    echo "Running $(grep -c '^--re$' <<< "$re_output") mutant(s) the tests missed against Mega EEST" >&2
    local rc=0
    # MEGA_EEST_SCRIPT stands in for the script in the driver's own tests. The warm-up checks the
    # pin's provenance; no case set is compared here, the Mega EEST gate does that. It counts
    # against the budget, as the whole pass does.
    run_bounded env -u MEGA_EEST_BASE_REV -u MEGA_EEST_BASE_REPO -u MEGA_EEST_BASE_REPO_MOVED_FROM \
        MEGA_EEST_MODES=plain \
        "${MEGA_EEST_SCRIPT:-$ROOT_DIR/scripts/mega_eest.sh}" \
        > "$OUT_DIR/mega-eest-warmup.log" 2>&1 || rc=$?
    if [[ $rc -eq 124 ]]; then
        record_failed_followup "$results" "$rc" "the Mega EEST warm-up (see $OUT_DIR/mega-eest-warmup.log)"
        return 0
    elif [[ $rc -ne 0 ]]; then
        echo "Mega EEST does not pass on the unmutated tree (exit $rc); see $OUT_DIR/mega-eest-warmup.log" >&2
        mkdir -p "$results"
        echo "scripts/mega_eest.sh exited $rc on the unmutated tree" > "$results/run-failed.txt"
        return 0
    fi
    # Every build directory reads the checkout the warm-up used, and writes its adapter workspace,
    # logs and build in its own target directory: a work or target directory set for the warm-up
    # would be shared by builds running at once. The warm-up checked the pin's provenance, so the
    # runs do not fetch the suite's history again. Each build directory starts from the warm
    # target, the warm-up's adapter build included, and builds incrementally; the script runs
    # the adapter its own build reports, so the warm-up's unmutated binary in the copy is never
    # what runs.
    #
    # One mutant at a time (--jobs 1), each running the suite's cases with the machine's whole
    # parallelism, as the baseline did: mutants run side by side would each build mega-evm and run
    # the cases on the same cores, against a timeout the baseline set alone. On a 4-vCPU runner a
    # mutant is about three minutes (an incremental build of mega-evm for the test and again for
    # the adapter, and the 930 plain cases), estimated from local runs, not measured there.
    run_bounded env -u MEGA_EEST_WORK -u MEGA_EEST_TARGET_DIR \
        -u MEGA_EEST_BASE_REV -u MEGA_EEST_BASE_REPO -u MEGA_EEST_BASE_REPO_MOVED_FROM \
        MEGA_EEST_DIR="${MEGA_EEST_DIR:-${MEGA_EEST_WORK:-$ROOT_DIR/target/mega-eest}/src}" \
        MEGA_EEST_MODES=plain MEGA_EEST_SKIP_PROVENANCE=1 cargo mutants \
        "${PKG_ARGS[@]}" \
        --jobs 1 \
        --output "$OUT_DIR/mega-eest" \
        --no-shuffle \
        -vV \
        --cargo-arg=--test \
        --cargo-arg=mega_eest \
        "${SCOPE_ARGS[@]}" \
        "${re_args[@]}" \
        -- --run-ignored only 2>&1 | tee "$OUT_DIR/mega-eest.log" || rc=$?
    # As for the re-check: a run that fails as a run, or that the budget stops, proves nothing by
    # the files it left.
    if ! run_outcome "$rc"; then
        record_failed_followup "$results" "$rc" "the Mega EEST run of the missed mutants (see $OUT_DIR/mega-eest.log)"
    fi
}

case "$cmd" in
    diff)
        base="${1:?usage: mutation_test.sh diff <base-ref> [--list]}"
        if [[ -n "$LISTING" ]]; then
            diff_file="$(mktemp)"
            trap 'rm -f "$diff_file"' EXIT
            mutatable_diff "$base" > "$diff_file"
            run_mutants --in-diff "$diff_file" --list --colors never
            exit 0
        fi
        diff_file="$OUT_DIR.diff"
        mkdir -p "$(dirname "$diff_file")"
        mutatable_diff "$base" > "$diff_file"
        # An empty diff still goes to cargo-mutants, which says it has nothing to
        # mutate, so the gate passes on its word rather than on missing results; it
        # builds nothing, so the warm build is skipped.
        [[ -s "$diff_file" ]] || WARM=0
        run_mutants --in-diff "$diff_file"
        ;;
    full)
        run_mutants "$@"
        ;;
    file)
        glob="${1:?usage: mutation_test.sh file <glob>}"
        run_mutants -f "$glob"
        ;;
    infra)
        CONFIG_ARGS=(--config "$ROOT_DIR/.cargo/mutants-infra.toml")
        FILE_ARGS=()
        for file in "${INFRA_FILES[@]}"; do FILE_ARGS+=(-f "$file"); done
        run_mutants "${FILE_ARGS[@]}" "$@"
        ;;
    *)
        echo "usage: mutation_test.sh {diff <base-ref> [--list]|full [args...]|file <glob>|infra [args...]|list [--all] [args...]|changed <base-ref>}" >&2
        exit 2
        ;;
esac

[[ -n "$LISTING" ]] && exit 0
if [[ "$RESULT" == stopped ]]; then
    echo
    echo "MUTANTS_BUDGET (${BUDGET}s) ran out; the results cover only the mutants tested so far."
fi
echo
echo "Mutation results written to $OUT_DIR/mutants.out/"
echo "Score + gate with: python3 scripts/mutation_gate.py report --results $OUT_DIR/mutants.out --recheck $OUT_DIR/recheck/mutants.out --mega-eest $OUT_DIR/mega-eest/mutants.out --status $OUT_DIR/status"
