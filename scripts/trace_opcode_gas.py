#!/usr/bin/env python3
"""
Opcode gas aggregation for EVM opcode-level traces.

This script reads a geth-style `debug_traceTransaction` JSON trace that contains
`structLogs` entries like:

  {"pc": 0, "op": "PUSH1", "gas": 1699300, "gasCost": 3, "depth": 1, ...}

and aggregates the *reported* per-step `gasCost` by opcode, producing:

- `count`: number of times the opcode appears
- `total`: sum of `gasCost` across occurrences
- `avg`: `total / count`
- `min`/`max`: min/max `gasCost` seen for that opcode

Trace format assumptions
------------------------
- The JSON has a top-level `structLogs` array.
- Each element has at least `op` (string) and `gasCost` (number).
- Optional filtering by `depth` uses the element's `depth` field.

Important caveats (what "gasCost" means)
----------------------------------------
`gasCost` is whatever the tracer reports for that step. In many tracers this
includes *dynamic* components (e.g. memory expansion) and, for CALL-like
opcodes (CALL/DELEGATECALL/STATICCALL/CALLCODE), it can include gas forwarded
to the callee. That means:

- CALL-like opcodes may dominate totals even though much of that gas is spent
  in the callee; use `--depth` to separate caller vs callee contributions.
- Some clients/tracers differ slightly in how they attribute dynamic costs.

Performance notes
-----------------
Traces can be very large. This script avoids loading the full JSON into memory
by streaming tokens via `jq --stream` (so `jq` is required).

For CLI usage examples (including how to generate `trace.json` with `mega-evme`)
run:

    scripts/trace_opcode_gas.py --help
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Stats:
    count: int = 0
    total: int = 0
    min_cost: int = 1 << 62
    max_cost: int = 0

    def add(self, cost: int) -> None:
        self.count += 1
        self.total += cost
        if cost < self.min_cost:
            self.min_cost = cost
        if cost > self.max_cost:
            self.max_cost = cost


def _jq_cmd(depth_filter: int | None, trace_path: Path) -> list[str]:
    if depth_filter is None:
        jq_filter = (
            'select(.[0][0]=="structLogs" and (.[0][2]=="op" or .[0][2]=="gasCost"))'
            ' | [.[0][1], .[0][2], .[1]] | @tsv'
        )
    else:
        jq_filter = (
            'select(.[0][0]=="structLogs" and (.[0][2]=="op" or .[0][2]=="gasCost" or .[0][2]=="depth"))'
            ' | [.[0][1], .[0][2], .[1]] | @tsv'
        )

    return ["jq", "--stream", "-r", jq_filter, str(trace_path)]


def parse_args(argv: list[str]) -> argparse.Namespace:
    description = """\
Aggregate per-opcode gasCost from a geth-style opcode trace JSON.

The input is expected to contain `.structLogs[]` entries with at least:
  - `op` (string)
  - `gasCost` (number)
Optionally:
  - `depth` (number), used by `--depth`

Output columns:
  op, count, total, avg, min, max

Caveat: `gasCost` is tracer-reported per step; for CALL-like opcodes it can
include gas forwarded to the callee. Use `--depth` to analyze by call frame.
"""
    epilog = """\
Requirements:
  - `jq` on PATH (used for streaming parse via `jq --stream`)

Examples:
  scripts/trace_opcode_gas.py trace.json
  scripts/trace_opcode_gas.py trace.json --limit 30
  scripts/trace_opcode_gas.py trace.json --sort count --limit 50
  scripts/trace_opcode_gas.py trace.json --depth 1
  scripts/trace_opcode_gas.py trace.json --tsv > opcode_gas.tsv

Generating the trace (mega-evme):
  mega-evme replay <txhash> \\
    --rpc https://mainnet.megaeth.com/rpc \\
    --trace --tracer opcode \\
    --trace.output trace.json

Interpretation notes:
  - Totals may exceed the transaction's gasUsed because CALL-like opcodes can
    include gas forwarded into nested calls; use `--depth` to break it down.
"""
    p = argparse.ArgumentParser(
        description=description,
        epilog=epilog,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("trace", type=Path, help="Path to trace JSON (e.g. trace.json)")
    p.add_argument(
        "--depth",
        type=int,
        default=None,
        help="Only include steps with this call depth (e.g. 1 for top-level). Default: include all depths.",
    )
    p.add_argument(
        "--sort",
        choices=["total", "count", "avg", "op"],
        default="total",
        help="Sort output by: total gas, count, avg gas, or opcode.",
    )
    p.add_argument("--limit", type=int, default=0, help="Limit rows (0 = no limit).")
    p.add_argument(
        "--tsv",
        action="store_true",
        help="Output as TSV: op<TAB>count<TAB>total<TAB>avg<TAB>min<TAB>max.",
    )
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    if shutil.which("jq") is None:
        print("error: `jq` not found on PATH (required for streaming parse).", file=sys.stderr)
        return 2

    if not args.trace.exists():
        print(f"error: trace not found: {args.trace}", file=sys.stderr)
        return 2

    cmd = _jq_cmd(args.depth, args.trace)
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True)
    assert proc.stdout is not None

    per_op: dict[str, Stats] = {}
    pending: dict[int, dict[str, str]] = {}
    required = {"op", "gasCost"} | ({"depth"} if args.depth is not None else set())

    try:
        for line in proc.stdout:
            line = line.rstrip("\n")
            if not line:
                continue
            idx_str, key, val = line.split("\t", 2)
            idx = int(idx_str)
            state = pending.get(idx)
            if state is None:
                state = {}
                pending[idx] = state
            state[key] = val

            if not required.issubset(state):
                continue

            if args.depth is not None and int(state["depth"]) != args.depth:
                del pending[idx]
                continue

            op = state["op"]
            try:
                cost = int(state["gasCost"])
            except ValueError:
                del pending[idx]
                continue

            per_op.setdefault(op, Stats()).add(cost)
            del pending[idx]
    finally:
        proc.stdout.close()
        rc = proc.wait()

    if rc != 0:
        print(f"error: jq exited with code {rc}", file=sys.stderr)
        return rc

    def avg(s: Stats) -> float:
        return (s.total / s.count) if s.count else 0.0

    rows = [(op, st) for op, st in per_op.items()]
    if args.sort == "total":
        rows.sort(key=lambda r: r[1].total, reverse=True)
    elif args.sort == "count":
        rows.sort(key=lambda r: r[1].count, reverse=True)
    elif args.sort == "avg":
        rows.sort(key=lambda r: avg(r[1]), reverse=True)
    else:
        rows.sort(key=lambda r: r[0])

    if args.limit and args.limit > 0:
        rows = rows[: args.limit]

    if args.tsv:
        for op, st in rows:
            print(f"{op}\t{st.count}\t{st.total}\t{avg(st):.1f}\t{st.min_cost}\t{st.max_cost}")
        return 0

    op_w = max([2] + [len(op) for op, _ in rows])
    count_w = max([5] + [len(str(st.count)) for _, st in rows])
    total_w = max([5] + [len(str(st.total)) for _, st in rows])
    avg_w = 10
    min_w = max([3] + [len(str(st.min_cost)) for _, st in rows]) if rows else 3
    max_w = max([3] + [len(str(st.max_cost)) for _, st in rows]) if rows else 3

    print(
        f"{'op':<{op_w}}  {'count':>{count_w}}  {'total':>{total_w}}  {'avg':>{avg_w}}  {'min':>{min_w}}  {'max':>{max_w}}"
    )
    for op, st in rows:
        print(
            f"{op:<{op_w}}  {st.count:>{count_w}}  {st.total:>{total_w}}  {avg(st):>{avg_w}.1f}  {st.min_cost:>{min_w}}  {st.max_cost:>{max_w}}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
