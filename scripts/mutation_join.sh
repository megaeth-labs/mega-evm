#!/usr/bin/env bash
# List and score a four-shard run. Run from the directory holding shards/.
set -euo pipefail

scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scope="${1:-}"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

case "$scope" in
    diff)
        base="${2:?usage: mutation_join.sh diff <base-ref>}"
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
shards=(shards/mutation-results-shard-{0..3})
python3 "$scripts/mutation_gate.py" merge --scope "$scope" --inventory inventory.txt \
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
