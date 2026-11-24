#!/usr/bin/env bash
#
# Paired, order-interleaved A/B measurement for one bench target.
#
# Runs the feature and baseline bench binaries back-to-back for R short rounds,
# alternating which side goes first each round (A B / B A / A B ...). Keeping the
# two halves of every pair adjacent in time is the whole point: the runner's slow
# drift (thermal/frequency/noisy-neighbour) hits both halves equally, so the
# per-round paired Δ% computed downstream cancels it. The single sequential
# feature-then-baseline pass it replaces could not — its two halves are minutes
# apart, turning drift into a ±2-5% phantom delta.
#
# Each round uses a deliberately SHORT criterion config; precision is recovered
# across rounds by the bootstrap CI, not within a round. Per round it writes one
# `--output-format bencher` file per side, which bench_compare.py then pairs.
#
# Usage:
#   bench_paired.sh <target> <feature_bin> <baseline_bin> <out_dir> \
#                   <rounds> <sample_size> <warmup_s> <measure_s>
#
# <baseline_bin> may be empty (target absent at the baseline commit) or equal to
# <feature_bin> (the A/A self-check) — both are valid and handled.
set -euo pipefail

target="$1"
feature_bin="$2"
baseline_bin="$3"
out_dir="$4"
rounds="$5"
sample_size="$6"
warmup="$7"
measure="$8"

if [ "$rounds" -lt 2 ] || [ $((rounds % 2)) -ne 0 ]; then
  echo "round count must be an even number >= 2 so feature-first and baseline-first slots balance" >&2
  exit 1
fi

mkdir -p "$out_dir"

run_one() {
  # $1 = binary, $2 = output file
  "$1" --bench --output-format bencher --noplot \
    --sample-size "$sample_size" \
    --warm-up-time "$warmup" \
    --measurement-time "$measure" \
    >"$2"
}

for r in $(seq 1 "$rounds"); do
  rr=$(printf '%02d' "$r")
  feat_out="${out_dir}/feature__${target}__r${rr}.txt"
  base_out="${out_dir}/baseline__${target}__r${rr}.txt"

  # Alternate the within-pair order every round so no side is structurally
  # favoured by always warming the cache / running on a hotter core first.
  # Convention: odd round = feature-first, even round = baseline-first.
  # `bench_compare.py` noise_floor relies on this (same-parity rounds = same
  # execution slot) — keep the two in sync.
  if [ "$baseline_bin" = "" ]; then
    run_one "$feature_bin" "$feat_out"            # feature-only (no baseline)
  elif [ $((r % 2)) -eq 1 ]; then
    run_one "$feature_bin" "$feat_out"
    run_one "$baseline_bin" "$base_out"
  else
    run_one "$baseline_bin" "$base_out"
    run_one "$feature_bin" "$feat_out"
  fi
  echo "round ${rr}/${rounds} done for ${target}"
done
