#!/usr/bin/env python3
"""Render an EEST sweep report as a markdown summary and compare it to the committed baseline.

The sweep's own exit code is the gate: it fails on a fixture that panicked and on a difference no
MegaETH mechanism accounts for, and on nothing else. This script never fails a run. What it adds
is drift: the counts of fixtures the runner declined and of differences it explained are the
sweep's coverage, and a silent move in either — a corpus that half-unpacked, a change that pushed
thousands of fixtures out of execution — is worth seeing even though it is not a defect.
"""

import argparse
import json
import sys

CLASSES = ["PASS", "EXPLAINED", "UNEXPLAINED", "SKIPPED", "PANIC"]


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def delta(current, base):
    d = current - base
    if d == 0:
        return f"{current}"
    return f"{current} ({d:+d})"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("report", help="diff-report.json produced by the sweep")
    ap.add_argument("--baseline", help="committed baseline tally to compare against")
    ap.add_argument("--out", help="write the markdown summary here (default: stdout)")
    args = ap.parse_args()

    report = load(args.report)
    base = load(args.baseline) if args.baseline else None
    base_classes = (base or {}).get("classes", {})

    lines = [
        f"## EEST sweep — {report['targetSpec']} vs {report['baseSpec']}",
        "",
        f"{report['total']} units judged"
        + (f", {report['skippedFiles']} file(s) skipped by filename" if report.get("skippedFiles") else "")
        + ".",
        "",
        "| class | units |",
        "|---|--:|",
    ]
    for name in CLASSES:
        count = report["classes"].get(name, 0)
        cell = delta(count, base_classes[name]) if name in base_classes else str(count)
        lines.append(f"| {name} | {cell} |")

    if report.get("mechanisms"):
        lines += ["", "| mechanism (explained differences) | units |", "|---|--:|"]
        for name, count in sorted(report["mechanisms"].items()):
            lines.append(f"| `{name}` | {count} |")

    if report.get("explainedFields"):
        lines += ["", "| disagreeing quantities | units |", "|---|--:|"]
        for shape, count in sorted(report["explainedFields"].items()):
            lines.append(f"| `{shape}` | {count} |")

    flagged = report.get("flagged", [])
    if flagged:
        lines += ["", "### Flagged units", "", "| class | fixture | quantities | detail |", "|---|---|---|---|"]
        # Cap the table: an unexplained class in the thousands is one finding to investigate, not
        # thousands of rows to scroll. The full list is in the uploaded report.
        for item in flagged[:50]:
            fixture = f"{item['path'].split('state_tests/')[-1]}::{item['name']}"
            lines.append(
                f"| {item['class']} | `{fixture[:160]}` | `{','.join(item['fields'])}` |"
                f" {(item.get('detail') or '-')[:160]} |"
            )
        if len(flagged) > 50:
            lines.append(f"| … | {len(flagged) - 50} more in the uploaded report | | |")

    if report.get("fileErrors"):
        lines += ["", "### Files the sweep could not judge", ""]
        for err in report["fileErrors"][:20]:
            lines.append(f"- `{err[:200]}`")

    if base_classes:
        drifted = [n for n in CLASSES if n in base_classes and report["classes"].get(n, 0) != base_classes[n]]
        lines += [""]
        if drifted:
            lines.append(
                "> :warning: Coverage drifted from the committed baseline in: "
                + ", ".join(f"`{n}`" for n in drifted)
                + ". Not a failure — update `tools/eest-sweep/baseline.json` if the move is expected."
            )
        else:
            lines.append("> Coverage matches the committed baseline exactly.")

    text = "\n".join(lines) + "\n"
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()
