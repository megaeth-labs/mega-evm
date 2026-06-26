#!/usr/bin/env bash
# Mutation-testing driver for mega-evm.
#
# Subcommands:
#   diff  <base-ref>   Mutate only lines changed vs <base-ref> (PR gate mode).
#   full               Mutate the whole mega-evm crate (nightly mode; slow).
#   file  <glob>       Mutate files matching <glob> (local iteration).
#
# Results land in $OUT_DIR/mutants.out/ (missed.txt, caught.txt, outcomes.json).
# Run scripts/mutation_gate.py afterwards to score + gate the run.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/mutants}"
SUPPRESS="${SUPPRESS:-$ROOT_DIR/mutants/suppressions.toml}"
JOBS="${JOBS:-$(nproc)}"
PKG="mega-evm"

cd "$ROOT_DIR"

if ! cargo mutants --version >/dev/null 2>&1; then
    echo "cargo-mutants is required. Install with 'cargo install cargo-mutants --locked'." >&2
    exit 1
fi

# Function-scoped suppressions become --exclude-re so they are never generated
# (saves build/test time). Line-scoped suppressions are filtered later by the gate.
mapfile -t EXCLUDE_ARGS < <(python3 "$ROOT_DIR/scripts/mutation_gate.py" exclude-re --suppressions "$SUPPRESS")

run_mutants() {
    rm -rf "$OUT_DIR"
    mkdir -p "$(dirname "$OUT_DIR")" # cargo-mutants creates OUT_DIR itself but not its parents
    # --no-shuffle: test mutants in deterministic source order so runs are
    #   reproducible and comparable (recommended by https://mutants.rs/pr-diff.html).
    # -vV: verbose progress + version banner, for diagnosable CI logs.
    cargo mutants \
        --package "$PKG" \
        --jobs "$JOBS" \
        --output "$OUT_DIR" \
        --no-shuffle \
        -vV \
        "${EXCLUDE_ARGS[@]}" \
        "$@"
}

cmd="${1:-}"
shift || true
case "$cmd" in
    diff)
        base="${1:?usage: mutation_test.sh diff <base-ref>}"
        diff_file="$OUT_DIR.diff"
        mkdir -p "$(dirname "$diff_file")"
        git diff --no-color "$base"...HEAD -- 'crates/mega-evm/**' > "$diff_file"
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
    *)
        echo "usage: mutation_test.sh {diff <base-ref>|full|file <glob>}" >&2
        exit 2
        ;;
esac

echo
echo "Mutation results written to $OUT_DIR/mutants.out/"
echo "Score + gate with: python3 scripts/mutation_gate.py report --results $OUT_DIR/mutants.out"
