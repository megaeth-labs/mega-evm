#!/usr/bin/env bash
# List and score four shards from MUTATION_INPUT_DIR into MUTATION_OUTPUT_DIR.
set -euo pipefail

scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
checkout="$(dirname "$scripts")"
input="$(cd "${MUTATION_INPUT_DIR:?set MUTATION_INPUT_DIR}" && pwd -P)"
output="$(cd "${MUTATION_OUTPUT_DIR:?set MUTATION_OUTPUT_DIR}" && pwd -P)"
scope="${1:-}"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

if [[ "$input/" == "$checkout/"* || "$output/" == "$checkout/"* ||
      "$output/" == "$input/"* || "$input/" == "$output/"* ]]; then
    echo "ERROR: the join requires separate input and output directories outside the checkout" >&2
    exit 2
fi
shopt -s nullglob dotglob
entries=("$output"/*)
if ((${#entries[@]})); then
    echo "ERROR: output directory is not empty: $output" >&2
    exit 2
fi

identity=()
cd "$output"
case "$scope" in
    diff)
        base="${2:?usage: mutation_join.sh diff <base-ref>}"
        head="$(git -C "$checkout" rev-parse HEAD)"
        base="$(git -C "$checkout" rev-parse --verify "${base}^{commit}")"
        identity=(--head "$head" --base "$base")
        if [[ -n "${GITHUB_RUN_ID:-}" ]]; then identity+=(--run-id "$GITHUB_RUN_ID"); fi
        if [[ -n "${GITHUB_RUN_ATTEMPT:-}" ]]; then identity+=(--run-attempt "$GITHUB_RUN_ATTEMPT"); fi
        MUTANTS_SHARD='' "$scripts/mutation_test.sh" diff "$base" --list > inventory.txt
        ;;
    full)
        "$scripts/mutation_test.sh" list > inventory.txt
        ;;
    *)
        echo "usage: mutation_join.sh {diff <base-ref>|full}" >&2
        exit 2
        ;;
esac
echo "inventory: $(wc -l < inventory.txt) mutants"

# Name every shard by its matrix job index, even when it uploaded nothing.
shards=("$input"/mutation-results-shard-{0..3})
python3 "$scripts/mutation_gate.py" merge --scope "$scope" "${identity[@]}" --inventory inventory.txt \
    --out target/mutants "${shards[@]}" | tee -a "$summary"
python3 "$scripts/mutation_gate.py" merge-runs --scope "$scope" \
    --out target/mutants/recheck/mutants.out "${shards[@]/%//recheck/mutants.out}"
python3 "$scripts/mutation_gate.py" merge-runs --scope "$scope" \
    --out target/mutants/mega-eest/mutants.out "${shards[@]/%//mega-eest/mutants.out}"
python3 "$scripts/mutation_gate.py" report --scope "$scope" \
    --results target/mutants/mutants.out \
    --recheck target/mutants/recheck/mutants.out \
    --mega-eest target/mutants/mega-eest/mutants.out \
    --status target/mutants/status \
    --suppressions "$scripts/../mutants/suppressions.toml" \
    --comment mutation-report.md \
    --summary "$summary"
