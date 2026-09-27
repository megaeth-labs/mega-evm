#!/usr/bin/env bash
# Mutation-testing driver for mega-evm.
#
# Subcommands:
#   diff  <base-ref>   Mutate only lines changed vs <base-ref> (PR gate mode).
#   full               Mutate the whole mega-evm crate (nightly mode; slow).
#   file  <glob>       Mutate files matching <glob> (local iteration).
#   infra              Mutate the test gates' own infrastructure (see INFRA_FILES below).
#
# `diff`, `full` and `file` run the production scope of .cargo/mutants.toml, which excludes test
# helpers as noise. `infra` runs the second scope, .cargo/mutants-infra.toml: the scenario runner
# is helper code by that rule, but the benches and later tests rest on it, so it is mutated on its
# own.
#
# Results land in $OUT_DIR/mutants.out/ (missed.txt, caught.txt, outcomes.json).
# When cargo-mutants has nothing to mutate and writes no results, which it does
# when --in-diff leaves no mutant, the driver records its saying so there
# instead. A mutant that timed out is re-run once, alone, into
# $OUT_DIR/recheck/mutants.out/. Run scripts/mutation_gate.py afterwards to score
# + gate the run, passing both.
#
# MUTANTS_SHARD=k/n runs only shard k (0-based) of n of whichever mutant set the
# subcommand selects (cargo-mutants' own --shard), for a diff too large for one
# job; give each shard its own OUT_DIR and gate each one. See REVIEW.md.

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
SUPPRESS="${SUPPRESS:-$ROOT_DIR/mutants/suppressions.toml}"
JOBS="${JOBS:-$(nproc)}"
PKG_ARGS=(--package mega-evm)
CONFIG_ARGS=()

# The test gates' own executable logic: the scenario runner's transaction conversion and its
# execute/commit loop.
INFRA_FILES=(
    crates/mega-evm/src/test_utils/scenario.rs
)

cd "$ROOT_DIR"

# Clear the results before anything that can fail, so a run that stops early
# leaves nothing behind for the gate to mistake for its results.
rm -rf "$OUT_DIR"

if ! cargo mutants --version >/dev/null 2>&1; then
    echo "cargo-mutants is required. Install with 'cargo install cargo-mutants --locked'." >&2
    exit 1
fi

# Function-scoped suppressions become --exclude-re so they are never generated
# (saves build/test time). Line-scoped suppressions are filtered later by the gate.
# Capture into a variable first: `mapfile < <(...)` would mask a non-zero exit
# from the helper (a malformed suppressions.toml), letting the run continue
# without the function-scoped excludes.
if ! exclude_re_output="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" exclude-re --suppressions "$SUPPRESS")"; then
    echo "failed to parse function suppressions from $SUPPRESS" >&2
    exit 1
fi
EXCLUDE_ARGS=()
[[ -n "$exclude_re_output" ]] && mapfile -t EXCLUDE_ARGS <<< "$exclude_re_output"

SHARD_ARGS=()
[[ -n "${MUTANTS_SHARD:-}" ]] && SHARD_ARGS=(--shard "$MUTANTS_SHARD")

# cargo-mutants exit codes (https://mutants.rs/exit-codes.html): 0 = all caught,
# 2 = missed mutants, 3 = timeouts. Those three are normal run outcomes that the
# gate (scripts/mutation_gate.py) is responsible for scoring, so swallow them and
# let the gate be the single source of truth for pass/fail. Everything else
# (1 usage, 4 baseline broken, 5/6 bad --in-diff, 70 internal) is a real failure
# of the run itself and must abort.
run_outcome() {
    case "$1" in
        0 | 2 | 3) return 0 ;;
        *) return "$1" ;;
    esac
}

run_mutants() {
    rm -rf "$OUT_DIR"
    mkdir -p "$(dirname "$OUT_DIR")" # cargo-mutants creates OUT_DIR itself but not its parents
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
    # A listing writes no results: there is nothing to record or re-check.
    if [[ " $* " == *" --list "* ]]; then
        cargo mutants "${args[@]}"
        return
    fi
    local rc=0
    cargo mutants "${args[@]}" 2>&1 | tee "$OUT_DIR.log" || rc=$?
    run_outcome "$rc" || return
    python3 "$ROOT_DIR/scripts/mutation_gate.py" note-empty \
        --log "$OUT_DIR.log" --results "$OUT_DIR/mutants.out"
    recheck_timeouts
}

# A mutant that timed out was tested beside $JOBS others, each running the whole
# suite on the same cores, so it may have timed out on the contention alone.
# Re-run each once, alone; the gate takes that outcome for it. One that is
# caught or survives alone is decided; one that times out again stays
# inconclusive. Suppressed timeouts are not re-run.
recheck_timeouts() {
    local re_output
    if ! re_output="$(python3 "$ROOT_DIR/scripts/mutation_gate.py" timeout-re \
        --results "$OUT_DIR/mutants.out" --suppressions "$SUPPRESS")"; then
        echo "failed to list the timed-out mutants in $OUT_DIR/mutants.out" >&2
        return 1
    fi
    [[ -n "$re_output" ]] || return 0
    local re_args=()
    mapfile -t re_args <<< "$re_output"
    echo "Re-checking $((${#re_args[@]} / 2)) timed-out mutant(s) one at a time" >&2
    local rc=0
    cargo mutants \
        "${CONFIG_ARGS[@]}" \
        "${PKG_ARGS[@]}" \
        --jobs 1 \
        --output "$OUT_DIR/recheck" \
        --no-shuffle \
        -vV \
        "${re_args[@]}" 2>&1 | tee "$OUT_DIR/recheck.log" || rc=$?
    # A re-check that fails as a run leaves the timeouts standing, and the gate
    # reports its results as it finds them rather than the job ending here.
    if ! run_outcome "$rc"; then
        echo "the re-check of the timed-out mutants failed (exit $rc); see $OUT_DIR/recheck.log" >&2
    fi
}

cmd="${1:-}"
shift || true
case "$cmd" in
    diff)
        base="${1:?usage: mutation_test.sh diff <base-ref>}"
        diff_file="$OUT_DIR.diff"
        mkdir -p "$(dirname "$diff_file")"
        # Only src/ is mutatable; scoping the diff there avoids a non-empty diff
        # (and a wasted run) when a PR touches only tests/, Cargo.toml, etc. An
        # empty diff still goes to cargo-mutants, which says it has nothing to
        # mutate, so the gate passes on its word rather than on missing results.
        git diff --no-color "$base"...HEAD -- 'crates/mega-evm/src/**' > "$diff_file"
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
        echo "usage: mutation_test.sh {diff <base-ref>|full|file <glob>|infra}" >&2
        exit 2
        ;;
esac

echo
echo "Mutation results written to $OUT_DIR/mutants.out/"
echo "Score + gate with: python3 scripts/mutation_gate.py report --results $OUT_DIR/mutants.out --recheck $OUT_DIR/recheck/mutants.out"
