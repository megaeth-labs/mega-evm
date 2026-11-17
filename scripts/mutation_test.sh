#!/usr/bin/env bash
# Mutation-testing driver for mega-evm.
#
# Subcommands:
#   diff  <base-ref>   Mutate only lines changed vs <base-ref> (PR gate mode).
#   full  [args...]    Mutate the whole mega-evm crate (nightly mode; slow).
#                      Extra arguments go to cargo-mutants (e.g. `-f <file>`,
#                      `--shard k/n`).
#   file  <glob>       Mutate files matching <glob> (local iteration).
#   list  [--all] [args...]
#                      Print the mutants `full` would test, one per line, without
#                      building or testing anything (`cargo mutants --list`).
#                      Extra arguments go to cargo-mutants, as for `full`. With
#                      --all, include the mutants of function-scoped suppressions,
#                      which the runs exclude.
#   changed <base-ref> Print mutate=true if `diff <base-ref>` would run (and so
#                      build), mutate=false if it would find no mutatable change
#                      and exit before building. CI uses it to decide whether to
#                      restore and save the build cache.
#
# Every run first builds the unmutated tests into ./target, which cargo-mutants'
# `copy_target` copies into each scratch build directory, then runs cargo-mutants.
# Results land in $OUT_DIR/mutants.out/ (missed.txt, caught.txt, outcomes.json).
# Run scripts/mutation_gate.py afterwards to score + gate the run.
#
# Time budget: MUTANTS_BUDGET=<seconds>, counted from this script's start, bounds
# the warm build and cargo-mutants together (unset: no limit; needs GNU timeout).
# When it runs out, the running phase gets SIGINT, so cargo-mutants stops its
# in-flight mutants and keeps every outcome it has recorded; it gets SIGKILL if it
# is still running MUTANTS_GRACE seconds later. The script then exits 0 with what
# was tested, so the caller can still score and upload it.
#
# The last line printed, also written to $OUT_DIR/status, is
#   MUTANTS-STATUS status=<status> expected=<n|unknown> outcomes=<n>
# expected: mutants planned (unknown if the run stopped before cargo-mutants
# planned them); outcomes: mutants with an outcome; status is one of
#   complete         every planned mutant has an outcome;
#   partial          the budget ran out after some mutants were tested;
#   not-started      the budget ran out before any mutant was tested;
#   baseline-failed  the unmutated tree failed to build or to pass its tests;
#   error            the run itself failed (bad arguments, an internal error).
# Only baseline-failed and error exit non-zero. `mutation_gate.py report --status`
# fails the gate on anything but complete.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/mutants}"
SUPPRESS="${SUPPRESS:-$ROOT_DIR/mutants/suppressions.toml}"
JOBS="${JOBS:-$(nproc)}"
PKG="mega-evm"
BUDGET="${MUTANTS_BUDGET:-}"
GRACE="${MUTANTS_GRACE:-120}"

cd "$ROOT_DIR"

cmd="${1:-}"
shift || true

# Source paths a mutant can live in. `diff` and `changed` scope the diff to them,
# so a PR that touches only tests/, Cargo.toml, etc. has nothing to mutate.
MUTATABLE=('crates/mega-evm/src/**')
mutatable_diff() {
    git diff --no-color "$1"...HEAD -- "${MUTATABLE[@]}"
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

# The mutant set every run and `list` share: the package (only mega-evm, or
# --list would also cover the CLI crates of the workspace's default members),
# minus the functions of function-scoped suppressions (unless called with "all"),
# which become --exclude-re so they are never generated. Line-scoped suppressions
# are filtered later by the gate.
MUTANT_SET=()
load_mutant_set() {
    local exclude_re_output arg
    local exclude_args=()
    if [[ "${1:-}" != all ]]; then
        # Capture into a variable first: reading the helper's output through a
        # process substitution would mask a non-zero exit (a malformed
        # suppressions.toml), letting the run continue without the excludes.
        if ! exclude_re_output="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" exclude-re --suppressions "$SUPPRESS")"; then
            echo "failed to read the suppressions in $SUPPRESS" >&2
            return 1
        fi
        if [[ -n "$exclude_re_output" ]]; then
            while IFS= read -r arg; do exclude_args+=("$arg"); done <<< "$exclude_re_output"
        fi
    fi
    MUTANT_SET=(--package "$PKG" ${exclude_args[@]+"${exclude_args[@]}"})
}

if [[ "$cmd" == list ]]; then
    scope=""
    if [[ "${1:-}" == --all ]]; then
        scope=all
        shift
    fi
    require_cargo_mutants
    load_mutant_set "$scope"
    # --colors never: CI forces CARGO_TERM_COLOR=always, and colour codes in the
    # list would break the exact matching against it.
    exec cargo mutants --list --colors never "${MUTANT_SET[@]}" "$@"
fi

# How the run ended, classified into a status by `finish`:
# empty (nothing to mutate), ran, stopped (by the budget), baseline-failed.
# Any other exit, including one `set -e` forces, reports status=error.
RESULT=""
# Set once this run has cleared the previous results; until then there is
# nothing of this run's to count.
CLEARED=""

finish() {
    local rc=$? status=error expected=unknown outcomes=0 counts
    if [[ "$RESULT" == empty ]]; then
        expected=0
    elif [[ -n "$CLEARED" ]] && counts="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" progress --results "$OUT_DIR/mutants.out")"; then
        expected="${counts#expected=}"
        expected="${expected%% *}"
        outcomes="${counts##*outcomes=}"
    fi
    case "$RESULT" in
        empty) status=complete ;;
        ran)
            # cargo-mutants exits 0 without an output directory when no mutant
            # intersects the diff: nothing was planned.
            [[ "$expected" == unknown ]] && expected=0
            [[ "$expected" == "$outcomes" ]] && status=complete
            ;;
        stopped)
            if [[ "$expected" == "$outcomes" ]]; then
                status=complete
            elif ((outcomes > 0)); then
                status=partial
            else
                status=not-started
            fi
            ;;
        baseline-failed) status=baseline-failed ;;
    esac
    local line="MUTANTS-STATUS status=$status expected=$expected outcomes=$outcomes"
    mkdir -p "$OUT_DIR"
    echo "$line" > "$OUT_DIR/status"
    echo
    echo "$line"
    if [[ "$status" == error || "$status" == baseline-failed ]] && ((rc == 0)); then
        exit 1
    fi
}
trap finish EXIT

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
load_mutant_set

# Clear the previous run's results up front, so a run stopped before cargo-mutants
# starts cannot report them as its own.
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
CLEARED=1

run_mutants() {
    # Build the unmutated tests into ./target. `copy_target` copies it into every
    # scratch build directory, so the mutant builds reuse the compiled dependencies
    # and, with incremental compilation, the compiled mega-evm too.
    echo "Warm build (CARGO_INCREMENTAL=${CARGO_INCREMENTAL-unset}, budget ${BUDGET:-none}s)"
    local rc=0
    run_bounded cargo nextest run -p "$PKG" --all-features --no-run || rc=$?
    case "$rc" in
        0) ;;
        124) RESULT=stopped && return 0 ;;
        *) RESULT=baseline-failed && return "$rc" ;;
    esac

    # --no-shuffle: test mutants in deterministic source order so runs are
    #   reproducible and comparable (recommended by https://mutants.rs/pr-diff.html).
    # -vV: verbose progress + version banner, for diagnosable CI logs.
    run_bounded cargo mutants \
        "${MUTANT_SET[@]}" \
        --jobs "$JOBS" \
        --output "$OUT_DIR" \
        --no-shuffle \
        -vV \
        "$@" || rc=$?

    # cargo-mutants exit codes (https://mutants.rs/exit-codes.html): 0 = all caught,
    # 2 = missed mutants, 3 = timeouts. Those three are normal run outcomes that the
    # gate (scripts/mutation_gate.py) is responsible for scoring, so swallow them and
    # let the gate be the single source of truth for pass/fail. 4 means the unmutated
    # tree failed its tests. Everything else (1 usage, 5/6 bad --in-diff, 70 internal)
    # is a real failure of the run itself and must abort.
    case "$rc" in
        0 | 2 | 3) RESULT=ran ;;
        124) RESULT=stopped ;;
        4) RESULT=baseline-failed && return "$rc" ;;
        *) return "$rc" ;;
    esac
}

case "$cmd" in
    diff)
        base="${1:?usage: mutation_test.sh diff <base-ref>}"
        diff_file="$OUT_DIR.diff"
        mkdir -p "$(dirname "$diff_file")"
        mutatable_diff "$base" > "$diff_file"
        if [[ ! -s "$diff_file" ]]; then
            echo "No changes under crates/mega-evm vs $base; nothing to mutate." >&2
            RESULT=empty
            exit 0
        fi
        run_mutants --in-diff "$diff_file"
        ;;
    full)
        run_mutants "$@"
        ;;
    file)
        glob="${1:?usage: mutation_test.sh file <glob>}"
        run_mutants -f "$glob"
        ;;
    *)
        echo "usage: mutation_test.sh {diff <base-ref>|full [args...]|file <glob>|list [--all] [args...]|changed <base-ref>}" >&2
        exit 2
        ;;
esac

if [[ "$RESULT" == stopped ]]; then
    echo
    echo "MUTANTS_BUDGET (${BUDGET}s) ran out; the results cover only the mutants tested so far."
fi
echo
echo "Mutation results written to $OUT_DIR/mutants.out/"
echo "Score + gate with: python3 scripts/mutation_gate.py report --results $OUT_DIR/mutants.out --status $OUT_DIR/status"
