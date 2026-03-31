#!/usr/bin/env python3
"""Replay benchmark runner.

Usage:
  ./replay_runner.py correctness
  ./replay_runner.py perf <baseline> <feature> [rounds]
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


SCRIPT_DIR = Path(__file__).resolve().parent
CONFIG_PATH = SCRIPT_DIR / "replay_txs.json"
CORRECTNESS_RESULTS_PATH = Path("replay_results.json")
PERF_RESULTS_PATH = Path("replay_perf_results.json")


def load_config() -> dict[str, Any]:
    with CONFIG_PATH.open(encoding="utf-8") as f:
        config = json.load(f)
    # Allow environment variable to override the RPC URL from config.
    rpc_override = os.environ.get("MEGA_RPC_URL")
    if rpc_override:
        config["rpc_url"] = rpc_override
    return config


def write_json(path: Path, payload: Any) -> None:
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def replay_command(
    binary: str, tx_hash: str, rpc_url: str
) -> tuple[bool, dict[str, Any] | None]:
    try:
        proc = subprocess.run(
            [binary, "replay", tx_hash, "--rpc", rpc_url, "--output", "json"],
            capture_output=True,
            text=True,
            check=False,
            timeout=120,
        )
    except FileNotFoundError:
        return False, None
    except subprocess.TimeoutExpired:
        return False, None

    if proc.returncode != 0:
        return False, None

    try:
        return True, json.loads(proc.stdout)
    except json.JSONDecodeError:
        return False, None


def run_correctness(config: dict[str, Any]) -> int:
    rpc_url = config["rpc_url"]
    transactions = config["transactions"]

    print("=== Replay Correctness Check ===")
    print(f"RPC: {rpc_url}")
    print(f"Transactions: {len(transactions)}")
    print()

    failed = False
    results: list[dict[str, Any]] = []

    for tx in transactions:
        name = tx["name"]
        tx_hash = tx["hash"]
        expected_status = tx["expected"]["status"]
        expected_gas = tx["expected"]["gas_used"]

        print(f"--- {name} ({tx_hash}) ---")

        ok, payload = replay_command("mega-evme", tx_hash, rpc_url)
        if not ok or payload is None:
            print("  FAIL: replay command failed")
            failed = True
            results.append(
                {
                    "name": name,
                    "hash": tx_hash,
                    "status_match": False,
                    "gas_match": False,
                    "actual_status": None,
                    "actual_gas": None,
                    "expected_status": expected_status,
                    "expected_gas": expected_gas,
                    "target_tx_ms": None,
                    "mgas_per_sec": None,
                    "error": "replay_failed",
                }
            )
            continue

        actual_status = payload["status"]
        actual_gas = payload["gas_used"]
        target_tx_ms = payload["timing"]["target_tx_ms"]
        mgas_per_sec = payload["performance"]["mgas_per_sec"]

        status_match = actual_status == expected_status
        gas_match = actual_gas == expected_gas

        if not status_match:
            print(
                f"  MISMATCH status: expected={expected_status} actual={actual_status}"
            )
            failed = True

        if not gas_match:
            print(f"  MISMATCH gas: expected={expected_gas} actual={actual_gas}")
            failed = True

        if status_match and gas_match:
            print(f"  OK (target_tx={target_tx_ms}ms, {mgas_per_sec} Mgas/s)")

        results.append(
            {
                "name": name,
                "hash": tx_hash,
                "status_match": status_match,
                "gas_match": gas_match,
                "actual_status": actual_status,
                "actual_gas": actual_gas,
                "expected_status": expected_status,
                "expected_gas": expected_gas,
                "target_tx_ms": target_tx_ms,
                "mgas_per_sec": mgas_per_sec,
            }
        )

    write_json(CORRECTNESS_RESULTS_PATH, results)
    print()
    print(f"Results written to {CORRECTNESS_RESULTS_PATH}")

    if failed:
        print("FAILED: One or more transactions had mismatches")
        return 1

    print("ALL PASSED")
    return 0


def run_single_perf(
    *,
    binary: str,
    tx_hash: str,
    name: str,
    label: str,
    rpc_url: str,
    samples: dict[str, dict[str, list[float]]],
) -> bool:
    ok, payload = replay_command(binary, tx_hash, rpc_url)
    if not ok or payload is None:
        print(f"    {name}: FAILED")
        return False

    try:
        mgas = float(payload["performance"]["mgas_per_sec"])
    except (KeyError, TypeError, ValueError):
        print(f"    {name}: INVALID OUTPUT")
        return False

    print(f"    {name}: {mgas} Mgas/s")
    samples[label][name].append(mgas)
    return True


def run_perf(
    config: dict[str, Any], baseline_bin: str, feature_bin: str, rounds: int
) -> int:
    rpc_url = config["rpc_url"]
    warn_pct = float(config.get("warn_threshold_pct", 20))
    fail_pct = float(config.get("fail_threshold_pct", 40))
    transactions = config["transactions"]

    print(f"=== Replay Performance Comparison (ABBA × {rounds} rounds) ===")
    print(f"Baseline: {baseline_bin}")
    print(f"Feature:  {feature_bin}")
    print(f"RPC: {rpc_url}")
    print(f"Transactions: {len(transactions)}")
    print()

    split = (len(transactions) + 1) // 2
    first_half = transactions[:split]
    second_half = transactions[split:]

    samples: dict[str, dict[str, list[float]]] = {
        "baseline": defaultdict(list),
        "feature": defaultdict(list),
    }
    command_failures = 0

    for round_idx in range(1, rounds + 1):
        print(f"--- Round {round_idx}/{rounds} ---")

        print(f"  Baseline [0..{split})")
        for tx in first_half:
            if not run_single_perf(
                binary=baseline_bin,
                tx_hash=tx["hash"],
                name=tx["name"],
                label="baseline",
                rpc_url=rpc_url,
                samples=samples,
            ):
                command_failures += 1

        print(f"  Feature  [0..{split})")
        for tx in first_half:
            if not run_single_perf(
                binary=feature_bin,
                tx_hash=tx["hash"],
                name=tx["name"],
                label="feature",
                rpc_url=rpc_url,
                samples=samples,
            ):
                command_failures += 1

        if second_half:
            print(f"  Feature  [{split}..{len(transactions)})")
            for tx in second_half:
                if not run_single_perf(
                    binary=feature_bin,
                    tx_hash=tx["hash"],
                    name=tx["name"],
                    label="feature",
                    rpc_url=rpc_url,
                    samples=samples,
                ):
                    command_failures += 1

            print(f"  Baseline [{split}..{len(transactions)})")
            for tx in second_half:
                if not run_single_perf(
                    binary=baseline_bin,
                    tx_hash=tx["hash"],
                    name=tx["name"],
                    label="baseline",
                    rpc_url=rpc_url,
                    samples=samples,
                ):
                    command_failures += 1

    print()
    print("=== Results ===")
    print(f"{'Transaction':<30} {'Baseline':>12} {'Feature':>12} {'Delta':>10}  ")
    print("-" * 72)

    results: list[dict[str, Any]] = []
    missing_samples: list[dict[str, Any]] = []
    any_warn = False
    regression_fail = False

    for tx in transactions:
        name = tx["name"]
        baseline_vals = samples["baseline"].get(name, [])
        feature_vals = samples["feature"].get(name, [])

        if len(baseline_vals) != rounds or len(feature_vals) != rounds:
            missing_samples.append(
                {
                    "name": name,
                    "baseline_runs": len(baseline_vals),
                    "feature_runs": len(feature_vals),
                }
            )
            continue

        baseline_median = statistics.median(baseline_vals)
        feature_median = statistics.median(feature_vals)
        delta_pct = (
            (feature_median - baseline_median) / baseline_median * 100
            if baseline_median > 0
            else 0.0
        )

        icon = "OK"
        if delta_pct < -fail_pct:
            icon = "FAIL"
            regression_fail = True
        elif delta_pct < -warn_pct:
            icon = "WARN"
            any_warn = True

        sign = "+" if delta_pct > 0 else ""
        print(
            f"{name:<30} {baseline_median:>9.2f} M/s {feature_median:>9.2f} M/s {sign}{delta_pct:>8.1f}%  {icon}"
        )

        results.append(
            {
                "name": name,
                "baseline_mgas": round(baseline_median, 4),
                "feature_mgas": round(feature_median, 4),
                "delta_pct": round(delta_pct, 2),
            }
        )

    write_json(PERF_RESULTS_PATH, results)
    print()

    if command_failures:
        print(f"FAILED: {command_failures} replay command(s) failed")

    if missing_samples:
        print("FAILED: Missing replay performance samples detected")
        for sample in missing_samples:
            print(
                f"  {sample['name']}: baseline={sample['baseline_runs']}/{rounds}, "
                f"feature={sample['feature_runs']}/{rounds}"
            )

    if command_failures or missing_samples:
        print("FAILED: Replay performance check did not complete cleanly")
        return 1

    if regression_fail:
        print("FAILED: Significant performance regression detected")
        return 1

    if any_warn:
        print("WARNING: Possible performance regression detected")
    else:
        print("ALL PASSED")

    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Replay benchmark runner")
    subparsers = parser.add_subparsers(dest="mode", required=True)

    subparsers.add_parser("correctness", help="Verify gas/status match")

    perf = subparsers.add_parser("perf", help="ABBA performance comparison")
    perf.add_argument("baseline", help="Baseline binary path")
    perf.add_argument("feature", help="Feature binary path")
    perf.add_argument(
        "rounds", nargs="?", default=3, type=int, help="Number of ABBA rounds"
    )

    return parser


def main() -> int:
    args = build_parser().parse_args()
    config = load_config()

    if args.mode == "correctness":
        return run_correctness(config)

    if args.rounds < 1:
        print("rounds must be >= 1", file=sys.stderr)
        return 2

    return run_perf(config, args.baseline, args.feature, args.rounds)


if __name__ == "__main__":
    raise SystemExit(main())
