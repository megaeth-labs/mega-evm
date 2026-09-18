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
# helpers as noise. `infra` runs the second scope, .cargo/mutants-infra.toml: the code that
# implements the gates is helper code by that rule, but every later mechanism's verdict rests on
# it, so it is mutated against both packages' tests.
#
# Results land in $OUT_DIR/mutants.out/ (missed.txt, caught.txt, outcomes.json).
# Run scripts/mutation_gate.py afterwards to score + gate the run.
#
# MUTANTS_SHARD=k/n runs only shard k (0-based) of n of whichever mutant set the
# subcommand selects (cargo-mutants' own --shard), for a diff too large for one
# job; give each shard its own OUT_DIR and gate each one. See REVIEW.md.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/mutants}"
SUPPRESS="${SUPPRESS:-$ROOT_DIR/mutants/suppressions.toml}"
JOBS="${JOBS:-$(nproc)}"
PKG_ARGS=(--package mega-evm)
CONFIG_ARGS=()

# The test gates' own executable logic: the scenario runner's transaction conversion and its
# execute/commit loop, and the differential harness's record, comparison and registry modules.
INFRA_FILES=(
    crates/mega-evm/src/test_utils/scenario.rs
    crates/mega-differential/src/record.rs
    crates/mega-differential/src/diff.rs
    crates/mega-differential/src/registry.rs
)

cd "$ROOT_DIR"

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

run_mutants() {
    rm -rf "$OUT_DIR"
    mkdir -p "$(dirname "$OUT_DIR")" # cargo-mutants creates OUT_DIR itself but not its parents
    # --no-shuffle: test mutants in deterministic source order so runs are
    #   reproducible and comparable (recommended by https://mutants.rs/pr-diff.html).
    # -vV: verbose progress + version banner, for diagnosable CI logs.
    local rc=0
    cargo mutants \
        "${CONFIG_ARGS[@]}" \
        "${PKG_ARGS[@]}" \
        --jobs "$JOBS" \
        --output "$OUT_DIR" \
        --no-shuffle \
        -vV \
        "${EXCLUDE_ARGS[@]}" \
        "${SHARD_ARGS[@]}" \
        "$@" || rc=$?

    # cargo-mutants exit codes (https://mutants.rs/exit-codes.html): 0 = all caught,
    # 2 = missed mutants, 3 = timeouts. Those three are normal run outcomes that the
    # gate (scripts/mutation_gate.py) is responsible for scoring, so swallow them and
    # let the gate be the single source of truth for pass/fail. Everything else
    # (1 usage, 4 baseline broken, 5/6 bad --in-diff, 70 internal) is a real failure
    # of the run itself and must abort.
    case "$rc" in
        0 | 2 | 3) return 0 ;;
        *) return "$rc" ;;
    esac
}

cmd="${1:-}"
shift || true
case "$cmd" in
    diff)
        base="${1:?usage: mutation_test.sh diff <base-ref>}"
        diff_file="$OUT_DIR.diff"
        mkdir -p "$(dirname "$diff_file")"
        # Only src/ is mutatable; scoping the diff there avoids a non-empty diff
        # (and a wasted run) when a PR touches only tests/, Cargo.toml, etc.
        git diff --no-color "$base"...HEAD -- 'crates/mega-evm/src/**' > "$diff_file"
        if [[ ! -s "$diff_file" ]]; then
            echo "No changes under crates/mega-evm vs $base; nothing to mutate." >&2
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
    infra)
        PKG_ARGS=(--package mega-evm --package mega-differential)
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
echo "Score + gate with: python3 scripts/mutation_gate.py report --results $OUT_DIR/mutants.out"
