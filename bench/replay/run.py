#!/usr/bin/env python3
"""Replay-throughput benchmark driver for mega-evme.

Runs a corpus of real, characteristic MegaETH mainnet transactions through
``mega-evme replay --bench-runs`` fully offline (replaying from committed RPC
captures, so it is deterministic and needs no network), and reports per-
transaction EVM throughput.

With a single ``--bin`` it just measures (useful for recording a baseline or a
quick local check). With two ``--bin`` entries it does an **ABBA-interleaved**
base-vs-PR comparison: for each transaction the two binaries are run in
alternating order across several rounds so that slow monotonic drift on the CI
machine cancels out, then the median of each binary's samples is compared. A
transaction whose PR median is more than ``--threshold-pct`` slower than base is
flagged as a regression (and, with ``--fail-on-regression``, fails the run).

The driver shells out to the binaries and parses the single-document JSON that
``replay --json --bench-runs`` prints; it has no third-party dependencies.

Manifest format (JSON)::

    {
      "default_runs": 50,
      "default_warmup": 5,
      "cases": [
        {
          "name": "erc20_transfer",
          "category": "storage+log",
          "capture": "captures/erc20_transfer.cache.json",
          "tx": "0x...",
          "expected_gas": 51234,
          "note": "USDC transfer: 1 log, 2 SSTORE"
        }
      ]
    }

``capture`` paths are resolved relative to the manifest file.
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Measurement:
    """One binary's measured throughput for one transaction."""

    median_ns: float
    mgas_per_sec: float
    gas_used: int


def run_case(binary: str, capture: Path, tx: str, runs: int, warmup: int) -> Measurement:
    """Run one offline replay benchmark and parse its single-document JSON."""
    cmd = [
        binary,
        "replay",
        "--rpc.replay-file",
        str(capture),
        "--bench-runs",
        str(runs),
        "--bench-warmup",
        str(warmup),
        "--json",
        tx,
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if proc.returncode != 0:
        raise RuntimeError(
            f"{binary} replay failed for {tx} (exit {proc.returncode}):\n{proc.stderr.strip()}"
        )
    # stdout must be a single JSON document (regression-guarded in the CLI tests).
    try:
        out = json.loads(proc.stdout.strip())
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"could not parse bench JSON from {binary} for {tx}: {exc}") from exc
    bench = out.get("bench")
    if bench is None:
        raise RuntimeError(f"no `bench` field in output from {binary} for {tx}")
    return Measurement(
        median_ns=float(bench["medianNs"]),
        mgas_per_sec=float(bench["mgasPerSec"]),
        gas_used=int(out["gas_used"]),
    )


def measure_abba(
    binaries: list[tuple[str, str]],
    capture: Path,
    tx: str,
    runs: int,
    warmup: int,
    rounds: int,
) -> dict[str, Measurement]:
    """Collect `rounds` samples per binary, interleaving order each round.

    Round r runs the binaries in normal order when r is even and reversed when r
    is odd (A B / B A / A B ...), so a machine that slowly speeds up or slows
    down over the run does not systematically favor either binary. The reported
    measurement per binary is the median of its per-round samples.
    """
    samples: dict[str, list[Measurement]] = {label: [] for label, _ in binaries}
    for r in range(rounds):
        order = binaries if r % 2 == 0 else list(reversed(binaries))
        for label, path in order:
            samples[label].append(run_case(path, capture, tx, runs, warmup))
    result: dict[str, Measurement] = {}
    for label, _ in binaries:
        ss = samples[label]
        result[label] = Measurement(
            median_ns=statistics.median(s.median_ns for s in ss),
            mgas_per_sec=statistics.median(s.mgas_per_sec for s in ss),
            gas_used=ss[0].gas_used,
        )
    return result


def fmt_ns(ns: float) -> str:
    if ns >= 1_000_000:
        return f"{ns / 1_000_000:.3f} ms"
    if ns >= 1_000:
        return f"{ns / 1_000:.2f} µs"
    return f"{ns:.0f} ns"


def classify(delta_pct: float, threshold: float) -> str:
    """Marker for a median-time delta (positive = slower = regression)."""
    if delta_pct > threshold:
        return "🔴 regression"
    if delta_pct < -threshold:
        return "🟢 improvement"
    return "⚪ noise"


def build_report(
    cases: list[dict],
    results: list[dict[str, Measurement]],
    labels: list[str],
    threshold: float,
) -> tuple[str, dict, int]:
    """Render a markdown table and a machine-readable summary.

    Returns (markdown, json_summary, regression_count). Comparison columns are
    only emitted when exactly two binaries were measured (labels[0] = base,
    labels[1] = pr).
    """
    compare = len(labels) == 2
    lines: list[str] = []
    if compare:
        base, pr = labels
        lines.append(f"| transaction | category | gas | `{base}` median | `{pr}` median | Δ time | `{base}` Mgas/s | `{pr}` Mgas/s | verdict |")
        lines.append("|---|---|--:|--:|--:|--:|--:|--:|---|")
    else:
        only = labels[0]
        lines.append(f"| transaction | category | gas | `{only}` median | `{only}` Mgas/s |")
        lines.append("|---|---|--:|--:|--:|")

    regressions = 0
    improvements = 0
    json_cases = []
    for case, res in zip(cases, results):
        name, cat, gas = case["name"], case.get("category", ""), case["expected_gas"]
        if compare:
            base, pr = labels
            b, p = res[base], res[pr]
            delta = (p.median_ns - b.median_ns) / b.median_ns * 100.0
            verdict = classify(delta, threshold)
            regressions += verdict.startswith("🔴")
            improvements += verdict.startswith("🟢")
            lines.append(
                f"| {name} | {cat} | {gas:,} | {fmt_ns(b.median_ns)} | {fmt_ns(p.median_ns)} | "
                f"{delta:+.1f}% | {b.mgas_per_sec:,.0f} | {p.mgas_per_sec:,.0f} | {verdict} |"
            )
            json_cases.append(
                {
                    "name": name,
                    "category": cat,
                    "gas": gas,
                    "base_median_ns": b.median_ns,
                    "pr_median_ns": p.median_ns,
                    "delta_pct": delta,
                    "base_mgas_per_sec": b.mgas_per_sec,
                    "pr_mgas_per_sec": p.mgas_per_sec,
                    "verdict": verdict,
                }
            )
        else:
            only = labels[0]
            m = res[only]
            lines.append(
                f"| {name} | {cat} | {gas:,} | {fmt_ns(m.median_ns)} | {m.mgas_per_sec:,.0f} |"
            )
            json_cases.append(
                {
                    "name": name,
                    "category": cat,
                    "gas": gas,
                    "median_ns": m.median_ns,
                    "mgas_per_sec": m.mgas_per_sec,
                }
            )

    md = "\n".join(lines)
    if compare:
        summary = (
            f"\n\n**{len(cases)} transactions — "
            f"{regressions} regression(s), {improvements} improvement(s)** "
            f"(threshold ±{threshold:.1f}% on median time; ABBA-interleaved)."
        )
        md += summary
    json_summary = {
        "threshold_pct": threshold,
        "labels": labels,
        "regressions": regressions,
        "improvements": improvements,
        "cases": json_cases,
    }
    return md, json_summary, regressions


def parse_bin(spec: str) -> tuple[str, str]:
    if "=" not in spec:
        raise argparse.ArgumentTypeError(f"--bin must be LABEL=PATH, got {spec!r}")
    label, path = spec.split("=", 1)
    return label, path


def main() -> int:
    here = Path(__file__).resolve().parent
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest", type=Path, default=here / "manifest.json")
    ap.add_argument(
        "--bin",
        type=parse_bin,
        action="append",
        required=True,
        metavar="LABEL=PATH",
        help="binary to measure; pass twice (base then pr) to compare",
    )
    ap.add_argument("--runs", type=int, default=None, help="timed iterations per invocation")
    ap.add_argument("--warmup", type=int, default=None, help="discarded warmup iterations")
    ap.add_argument("--rounds", type=int, default=5, help="ABBA rounds per transaction")
    ap.add_argument("--threshold-pct", type=float, default=5.0)
    ap.add_argument("--json-out", type=Path, default=None)
    ap.add_argument("--markdown-out", type=Path, default=None)
    ap.add_argument("--fail-on-regression", action="store_true")
    args = ap.parse_args()

    if len(args.bin) > 2:
        ap.error("at most two --bin entries (base and pr) are supported")

    manifest = json.loads(args.manifest.read_text())
    runs = args.runs or manifest.get("default_runs", 50)
    warmup = args.warmup if args.warmup is not None else manifest.get("default_warmup", 5)
    cases = manifest["cases"]
    labels = [label for label, _ in args.bin]
    manifest_dir = args.manifest.resolve().parent

    results: list[dict[str, Measurement]] = []
    for case in cases:
        capture = (manifest_dir / case["capture"]).resolve()
        if not capture.exists():
            raise SystemExit(f"capture not found for case {case['name']}: {capture}")
        res = measure_abba(args.bin, capture, case["tx"], runs, warmup, args.rounds)
        # Sanity: every binary must reproduce the recorded on-chain gas, or the
        # comparison is meaningless (different work being timed).
        for label, m in res.items():
            if m.gas_used != case["expected_gas"]:
                raise SystemExit(
                    f"{label} replayed {case['name']} with gas {m.gas_used} != "
                    f"expected {case['expected_gas']} — corpus/binary mismatch"
                )
        results.append(res)
        print(f"  measured {case['name']} ({len(labels)} bin × {args.rounds} rounds)", file=sys.stderr)

    md, summary, regressions = build_report(cases, results, labels, args.threshold_pct)
    title = "## Replay throughput benchmark\n\n"
    print(title + md)
    if args.markdown_out:
        args.markdown_out.write_text(title + md + "\n")
    if args.json_out:
        args.json_out.write_text(json.dumps(summary, indent=2))

    if args.fail_on_regression and regressions:
        print(f"\n::error::{regressions} transaction(s) regressed beyond {args.threshold_pct:.1f}%", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
