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

finish_gate() {
    local verdict=$1 message=$2
    printf '## 🧬 Mutation testing — %s\n\n%s\n' "$verdict" "$message" \
        | tee mutation-report.md | tee -a "$summary"
}

fail_gate() {
    finish_gate '❌ FAIL' "$1"
    exit 1
}

case "$scope" in
    diff)
        base="${2:?usage: mutation_join.sh diff <base-ref>}"
        matrix="${MUTATION_MATRIX_OUTCOME-success}"
        if [[ "${MUTATION_SCOPE_OUTCOME-success}" != success ]]; then
            fail_gate 'Diff scope decision did not succeed.'
        fi
        case "${MUTATION_NEEDED-true}" in
            false)
                [[ "$matrix" == skipped ]] || fail_gate 'The shard jobs ran despite the no-run decision.'
                finish_gate '✅ PASS' '**Nothing to test**. The diff changes no mutatable source.'
                exit 0
                ;;
            true) ;;
            *) fail_gate 'Diff scope decision did not say whether mutation testing was needed.' ;;
        esac
        [[ "$matrix" != skipped ]] || fail_gate 'The shard jobs did not run despite a mutatable diff.'
        [[ "${MUTATION_DOWNLOAD_OUTCOME-success}" == success ]] \
            || fail_gate 'Artifact download did not succeed. Shard results may be incomplete.'
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
if [[ "$scope" == full ]] || grep -q '^MUTANTS-STATUS status=complete ' target/mutants/status; then
    python3 "$scripts/mutation_gate.py" merge-runs --scope "$scope" \
        --out target/mutants/recheck/mutants.out "${shards[@]/%//recheck/mutants.out}"
    python3 "$scripts/mutation_gate.py" merge-runs --scope "$scope" \
        --out target/mutants/mega-eest/mutants.out "${shards[@]/%//mega-eest/mutants.out}"
fi
status=0
python3 "$scripts/mutation_gate.py" report --scope "$scope" \
    --results target/mutants/mutants.out \
    --recheck target/mutants/recheck/mutants.out \
    --mega-eest target/mutants/mega-eest/mutants.out \
    --status target/mutants/status \
    --suppressions "$scripts/../mutants/suppressions.toml" \
    --comment mutation-report.md > /dev/null || status=$?
if [[ "$scope" == diff && "$status" == 0 && "$matrix" != success ]]; then
    fail_gate 'At least one shard job failed after producing results. See the shard jobs in this workflow run.'
fi
if [[ -f mutation-report.md ]]; then
    tee -a "$summary" < mutation-report.md
fi
exit "$status"
