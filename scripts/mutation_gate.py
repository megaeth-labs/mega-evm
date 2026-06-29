#!/usr/bin/env python3
"""Score and gate a cargo-mutants run for mega-evm.

Two subcommands:

  exclude-re  --suppressions <toml>
        Print one `--exclude-re <regex>` pair per line for every function-scoped
        suppression. Consumed by scripts/mutation_test.sh so suppressed
        functions are never generated as mutants.

  report      --results <mutants.out dir> [--suppressions <toml>]
              [--comment <path>] [--summary <path>]
        Read the run outcomes, apply line-scoped suppressions, compute the
        mutation score, write a Markdown report, and exit non-zero if any
        unsuppressed survivor remains (the "no new survivors" gate).

The gate is intended to run diff-scoped (cargo mutants --in-diff), so every
mutant it sees lives on a line the PR changed; an unsuppressed survivor there is
a test gap the PR introduced.
"""
from __future__ import annotations

import argparse
import sys
import tomllib
from pathlib import Path

# Cap how many survivors are rendered inline in the PR comment (GitHub caps a
# single comment at 65536 chars). The rest live in the run artifacts.
MAX_SURVIVORS_SHOWN = 20


def load_suppressions(path: Path) -> tuple[list[dict], list[dict]]:
    """Return (function_scoped, line_scoped) suppression entries."""
    if not path or not path.exists():
        return [], []
    data = tomllib.loads(path.read_text())
    entries = data.get("suppress", [])
    func = [e for e in entries if e.get("kind") == "function"]
    line = [e for e in entries if e.get("kind") == "line"]
    return func, line


def cmd_exclude_re(args: argparse.Namespace) -> int:
    func, _ = load_suppressions(Path(args.suppressions))
    for e in func:
        pattern = e.get("pattern")
        if not pattern:
            print(f"suppression for {e.get('file', '?')} missing 'pattern'", file=sys.stderr)
            return 1
        # Emitted as two lines so `mapfile` in the driver yields separate argv items.
        print("--exclude-re")
        print(pattern)
    return 0


def read_lines(path: Path) -> list[str]:
    if not path.exists():
        return []
    return [ln.strip() for ln in path.read_text().splitlines() if ln.strip()]


def mutant_body(line: str) -> str:
    """Strip the leading 'file:line:col: ' locator, leaving the mutation text."""
    parts = line.split(": ", 1)
    return parts[1] if len(parts) == 2 else line


def cmd_report(args: argparse.Namespace) -> int:
    results = Path(args.results)

    # Guard against a silent pass when the run produced no results. We must tell
    # apart two cases that both leave the outcome lists empty:
    #   * the results dir is absent  -> no run happened (e.g. the diff had no
    #     mutatable changes); benign, report "nothing tested" and pass.
    #   * the results dir exists but the expected files are missing -> the run
    #     aborted, wrote elsewhere, or the tool renamed its output. Refusing to
    #     report a 100% pass here is the whole point.
    if not results.exists():
        report = "## 🧬 Mutation testing\n\nNo results at "
        report += f"`{results}` — nothing was mutated (e.g. no mutatable changes).\n"
        if args.comment:
            Path(args.comment).write_text(report)
        if args.summary:
            with open(args.summary, "a") as fh:
                fh.write(report)
        print(report)
        return 0
    for required in ("caught.txt", "missed.txt"):
        if not (results / required).exists():
            print(
                f"ERROR: {results / required} is missing although {results} exists. "
                f"The mutation run aborted or changed its output format — refusing to "
                f"report a passing gate on incomplete results.",
                file=sys.stderr,
            )
            return 2

    missed = read_lines(results / "missed.txt")
    caught = read_lines(results / "caught.txt")
    unviable = read_lines(results / "unviable.txt")
    timeout = read_lines(results / "timeout.txt")

    _, line_supp = load_suppressions(Path(args.suppressions)) if args.suppressions else ([], [])
    # A line suppression matches either the bare mutation text (`mutant` written
    # without a locator) or the full `file:line:col: text` line. The latter lets
    # two mutants that share identical source text be suppressed independently.
    supp = {e["mutant"].strip() for e in line_supp if "mutant" in e}

    suppressed, real_survivors = [], []
    for m in missed:
        matched = m in supp or mutant_body(m) in supp
        (suppressed if matched else real_survivors).append(m)

    viable = len(caught) + len(missed)
    scored = viable - len(suppressed)  # equivalents/dead-code excluded from denominator
    killed = len(caught)
    score = (killed / scored * 100.0) if scored else 100.0
    gate_pass = not real_survivors

    # ---- Markdown report (PR comment + step summary) ----
    status = "✅ PASS" if gate_pass else "❌ FAIL"
    md = [
        f"## 🧬 Mutation testing — {status}",
        "",
        f"**Diff mutation score: {score:.1f}%** ({killed}/{scored} viable mutants killed)",
        "",
        f"- caught: {len(caught)}",
        f"- survived (real gaps): **{len(real_survivors)}**",
        f"- suppressed (equivalent/dead-code): {len(suppressed)}",
        f"- unviable: {len(unviable)} · timeout: {len(timeout)}",
        "",
    ]
    if real_survivors:
        md += [
            "### Survivors needing attention",
            "",
            (
                "Each mutation below changed the code but **no test failed**. Add a test "
                "that kills it, or — if it is provably equivalent/dead — add a justified "
                "entry to `mutants/suppressions.toml`."
            ),
            "",
        ]
        # Cap the rendered list: GitHub truncates comments at 65536 chars, and a
        # PR that introduces dozens of survivors would otherwise dump an unreadable
        # wall. The full set is always in the uploaded artifacts.
        shown = real_survivors[:MAX_SURVIVORS_SHOWN]
        md += [f"- `{m}`" for m in shown]
        if len(real_survivors) > MAX_SURVIVORS_SHOWN:
            md.append(
                f"- … and {len(real_survivors) - MAX_SURVIVORS_SHOWN} more "
                f"(see the `missed.txt` in the run artifacts)."
            )
        md += ["", "_Tip: run `/improve-mutation-score` to triage and fix these._"]
    else:
        md.append("No new test gaps introduced by this change. 🎉")
    report = "\n".join(md) + "\n"

    if args.comment:
        Path(args.comment).write_text(report)
    if args.summary:
        with open(args.summary, "a") as fh:
            fh.write(report)
    print(report)

    return 0 if gate_pass else 1


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    pe = sub.add_parser("exclude-re")
    pe.add_argument("--suppressions", required=True)
    pe.set_defaults(func=cmd_exclude_re)

    pr = sub.add_parser("report")
    pr.add_argument("--results", required=True)
    pr.add_argument("--suppressions", default=None)
    pr.add_argument("--comment", default=None, help="write Markdown report here")
    pr.add_argument("--summary", default=None, help="append report here (GITHUB_STEP_SUMMARY)")
    pr.set_defaults(func=cmd_report)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
