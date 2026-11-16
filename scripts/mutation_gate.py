#!/usr/bin/env python3
"""Score and gate a cargo-mutants run for mega-evm.

Subcommands:

  exclude-re  --suppressions <toml>
        Print one `--exclude-re <regex>` pair per line for every function-scoped
        suppression. Consumed by scripts/mutation_test.sh so suppressed
        functions are never generated as mutants.

  progress    --results <mutants.out dir>
        Print `expected=<n|unknown> outcomes=<n>`: how many mutants the run
        planned (unknown before cargo-mutants wrote its plan) and how many have
        an outcome. Used by scripts/mutation_test.sh for its status line.

  merge       --inventory <file> --out <dir> <shard dir>...
        Combine the results of a run split into shards (cargo mutants --shard)
        into <dir>/mutants.out and <dir>/status, counted against <inventory>
        (`cargo mutants --list` of the whole run). Each <shard dir> holds one
        shard's mutants.out/ and status. A shard that is missing or stopped
        early leaves inventory mutants without an outcome: status partial.
        Every shard outcome is kept. An outcome the inventory does not plan,
        or a second outcome for one mutant, means the shards and the inventory
        disagree on the mutant set: status mismatch, which never passes.

  report      --results <mutants.out dir> [--suppressions <toml>]
              [--status <file>] [--comment <path>] [--summary <path>]
        Read the run outcomes, apply line-scoped suppressions, compute the
        mutation score, write a Markdown report, and exit non-zero if any
        unsuppressed survivor remains (the "no new survivors" gate). With
        --status (the driver's MUTANTS-STATUS line), a run that did not test
        every planned mutant also fails: what it did test is still reported.

The gate is intended to run diff-scoped (cargo mutants --in-diff), so every
mutant it sees lives on a line the PR changed; an unsuppressed survivor there is
a test gap the PR introduced.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from collections import Counter
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
        # Two lines per pair: the driver reads one argument per line.
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


# One file per cargo-mutants outcome; every tested mutant lands in exactly one.
OUTCOME_FILES = ("caught.txt", "missed.txt", "timeout.txt", "unviable.txt")


def progress(results: Path) -> tuple[int | None, int]:
    """Return (planned mutants or None if no plan was written, mutants with an outcome)."""
    plan = results / "mutants.json"
    expected = len(json.loads(plan.read_text())) if plan.exists() else None
    outcomes = sum(len(read_lines(results / name)) for name in OUTCOME_FILES)
    return expected, outcomes


def cmd_progress(args: argparse.Namespace) -> int:
    expected, outcomes = progress(Path(args.results))
    print(f"expected={'unknown' if expected is None else expected} outcomes={outcomes}")
    return 0


def read_status(path: Path) -> dict[str, str] | None:
    """Parse the driver's `MUTANTS-STATUS key=value ...` line; None if absent."""
    if not path.exists():
        return None
    for line in reversed(path.read_text().splitlines()):
        if line.startswith("MUTANTS-STATUS "):
            return dict(f.split("=", 1) for f in line.split()[1:] if "=" in f)
    return None


def incomplete_note(status: dict[str, str]) -> str:
    """Describe a run that did not test every planned mutant."""
    state, expected, outcomes = status.get("status"), status.get("expected"), status.get("outcomes")
    if state == "partial":
        return (f"**Incomplete run** — the time budget ran out after {outcomes} of {expected} "
                f"planned mutants; the rest were not tested. The results below cover only "
                f"the tested mutants, and an incomplete run never passes.")
    if state == "not-started":
        return ("**Incomplete run** — the time budget ran out before any mutant was tested "
                f"({expected} planned). Nothing below is a result, and an incomplete run never passes.")
    if state == "mismatch":
        why = []
        if status.get("unplanned", "0") != "0":
            why.append(f"{status['unplanned']} shard outcome(s) are for mutants the inventory "
                       f"does not list")
        if status.get("duplicated", "0") != "0":
            why.append(f"{status['duplicated']} mutant(s) have more than one outcome")
        return (f"**Shards and inventory disagree** — the inventory lists {expected} mutants, "
                f"{outcomes} of them have an outcome, and {'; '.join(why)}. The shards did not "
                f"test the inventory's mutant set, so this run never passes. Every shard outcome "
                f"is still reported below; `unplanned.txt` in the run artifacts lists the "
                f"outcomes that do not fit the inventory.")
    return (f"**Run did not complete** (status `{state}`, {outcomes} of {expected} mutants tested); "
            f"see the run log.")


def cmd_merge(args: argparse.Namespace) -> int:
    ansi = re.compile(r"\x1b\[[0-9;]*m")
    inventory = [ansi.sub("", m) for m in read_lines(Path(args.inventory))]
    if not inventory:
        print(f"ERROR: empty mutant inventory at {args.inventory}", file=sys.stderr)
        return 2
    planned = Counter(inventory)
    # Every shard outcome goes into the merged lists, planned or not: a survivor
    # must never be dropped on the way to the report.
    merged: dict[str, list[str]] = {name: [] for name in OUTCOME_FILES}
    seen: Counter[str] = Counter()
    print("| shard | status |")
    print("|---|---|")
    for shard in args.shards:
        shard = Path(shard)
        status = read_status(shard / "status")
        line = " ".join(f"{k}={v}" for k, v in status.items()) if status else "no status"
        print(f"| `{shard}` | {line} |")
        for name in OUTCOME_FILES:
            for m in read_lines(shard / "mutants.out" / name):
                merged[name].append(m)
                seen[m] += 1
    # The shards and the inventory must describe the same mutant set. An outcome
    # for a mutant the inventory does not plan, or more outcomes for a mutant than
    # the inventory plans, shows they do not: completeness is measured against the
    # inventory, so it proves nothing then, and the run cannot pass.
    unplanned = [m for m in seen if m not in planned]
    duplicated = [m for m in seen if m in planned and seen[m] > planned[m]]
    out = Path(args.out)
    results = out / "mutants.out"
    results.mkdir(parents=True, exist_ok=True)
    (results / "mutants.json").write_text(json.dumps([{"name": m} for m in inventory]))
    for name, items in merged.items():
        (results / name).write_text("".join(f"{m}\n" for m in items))
    (out / "unplanned.txt").write_text("".join(f"{m}\n" for m in unplanned + duplicated))
    expected = len(inventory)
    outcomes = sum(min(n, planned[m]) for m, n in seen.items() if m in planned)
    if unplanned or duplicated:
        state = "mismatch"
    else:
        state = "complete" if outcomes == expected else "partial" if outcomes else "not-started"
    line = (f"MUTANTS-STATUS status={state} expected={expected} outcomes={outcomes} "
            f"unplanned={len(unplanned)} duplicated={len(duplicated)}")
    (out / "status").write_text(line + "\n")
    print(f"\n{len(args.shards)} shard result(s) found.\n")
    for title, items in (("Not in the inventory", unplanned), ("More than one outcome", duplicated)):
        if items:
            print(f"{title} ({len(items)}):\n")
            print("".join(f"- `{m}`\n" for m in items[:MAX_SURVIVORS_SHOWN]))
    print(line)
    return 0


def emit(report: str, args: argparse.Namespace) -> None:
    if args.comment:
        Path(args.comment).write_text(report)
    if args.summary:
        with open(args.summary, "a") as fh:
            fh.write(report)
    print(report)


def cmd_report(args: argparse.Namespace) -> int:
    results = Path(args.results)

    # The driver's status says whether the run tested every planned mutant. A
    # stopped run must never pass, even when everything it tested was caught.
    note = None
    if args.status:
        run_status = read_status(Path(args.status))
        if run_status is None:
            print(f"ERROR: no MUTANTS-STATUS line in {args.status}: the mutation run did not "
                  f"finish — refusing to report a passing gate.", file=sys.stderr)
            return 2
        if run_status.get("status") != "complete":
            note = incomplete_note(run_status)

    # Guard against a silent pass when the run produced no results. We must tell
    # apart two cases that both leave the outcome lists empty:
    #   * the results dir is absent  -> no run happened (e.g. the diff had no
    #     mutatable changes); benign, report "nothing tested" and pass.
    #   * the results dir exists but the expected files are missing -> the run
    #     aborted, wrote elsewhere, or the tool renamed its output. Refusing to
    #     report a 100% pass here is the whole point.
    if not results.exists():
        if note:
            emit(f"## 🧬 Mutation testing — ❌ FAIL\n\n{note}\n", args)
            return 1
        report = "## 🧬 Mutation testing\n\nNo results at "
        report += f"`{results}` — nothing was mutated (e.g. no mutatable changes).\n"
        emit(report, args)
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
    if note and not (missed or caught or unviable or timeout):
        emit(f"## 🧬 Mutation testing — ❌ FAIL\n\n{note}\n", args)
        return 1

    _, line_supp = load_suppressions(Path(args.suppressions)) if args.suppressions else ([], [])
    # A line suppression matches either the bare mutation text (`mutant` written
    # without a locator) or the full `file:line:col: text` line. The latter lets
    # two mutants that share identical source text be suppressed independently.
    supp = {e["mutant"].strip() for e in line_supp if "mutant" in e}

    def partition(items: list[str]) -> tuple[list[str], list[str]]:
        sup, real = [], []
        for m in items:
            (sup if (m in supp or mutant_body(m) in supp) else real).append(m)
        return sup, real

    suppressed, real_survivors = partition(missed)
    # Timeouts are inconclusive — a mutant that hangs was never proven caught — so
    # an unsuppressed timeout fails the gate (the dev must make it terminate, kill
    # it, or record an explicit suppression). Suppressing reuses the same model.
    supp_timeouts, real_timeouts = partition(timeout)

    viable = len(caught) + len(missed)
    scored = viable - len(suppressed)  # equivalents/dead-code excluded from denominator
    killed = len(caught)
    score = (killed / scored * 100.0) if scored else 100.0
    gate_pass = not real_survivors and not real_timeouts and note is None

    # ---- Markdown report (PR comment + step summary) ----
    status = "✅ PASS" if gate_pass else "❌ FAIL"

    # No viable mutants and nothing inconclusive (typical diff run whose changed
    # lines contain nothing mutatable). Reporting "100% (0/0)" reads like a real
    # result and confuses readers — say plainly that nothing was tested.
    if viable == 0 and not real_timeouts and note is None:
        nothing = "**Nothing to test** — no mutants were generated"
        if unviable or timeout:
            nothing += f" ({len(unviable)} unviable, {len(timeout)} timed out)"
        else:
            nothing += " on the changed lines"
        emit(f"## 🧬 Mutation testing — ✅ PASS\n\n{nothing}.\n", args)
        return 0

    md = [
        f"## 🧬 Mutation testing — {status}",
        "",
        *([note, ""] if note else []),
        f"**Diff mutation score: {score:.1f}%** ({killed}/{scored} viable mutants killed)",
        "",
        f"- caught: {len(caught)}",
        f"- survived (real gaps): **{len(real_survivors)}**",
        f"- timed out (inconclusive): **{len(real_timeouts)}**",
        f"- suppressed (equivalent/dead-code): {len(suppressed) + len(supp_timeouts)}",
        f"- unviable: {len(unviable)} · timeout total: {len(timeout)}",
        "",
    ]

    def section(title: str, blurb: str, items: list[str], artifact: str) -> list[str]:
        out = [f"### {title}", "", blurb, ""]
        out += [f"- `{m}`" for m in items[:MAX_SURVIVORS_SHOWN]]
        if len(items) > MAX_SURVIVORS_SHOWN:
            out.append(
                f"- … and {len(items) - MAX_SURVIVORS_SHOWN} more "
                f"(see `{artifact}` in the run artifacts)."
            )
        return out

    if real_survivors:
        md += section(
            "Survivors needing attention",
            "Each mutation below changed the code but **no test failed**. Add a test "
            "that kills it, or — if it is provably equivalent/dead — add a justified "
            "entry to `mutants/suppressions.toml`.",
            real_survivors, "missed.txt",
        )
    if real_timeouts:
        md += section(
            "Timed-out mutants (inconclusive)",
            "Each mutation below never finished within the timeout, so it was **not "
            "proven caught**. Make it terminate (a faster test), kill it, or — if it "
            "is a genuine non-terminating/equivalent case — add a justified entry to "
            "`mutants/suppressions.toml`.",
            real_timeouts, "timeout.txt",
        )
    if real_survivors or real_timeouts:
        md += ["", "_Tip: run `/improve-mutation-score` to triage and fix these._"]
    elif note is None:
        md.append("No new test gaps introduced by this change. 🎉")
    emit("\n".join(md) + "\n", args)

    return 0 if gate_pass else 1


def cmd_orphans(args: argparse.Namespace) -> int:
    """Flag suppressions that match no live mutant (stale = a silent blind spot).

    `--universe` is a file of mutant lines (`file:line:col: text`) covering the
    whole crate, assembled from BOTH engines (cargo-mutants `--list` and
    `umutate.py plan`). A function suppression's regex `pattern` must match at
    least one mutant name; a line suppression's `mutant` must match one mutant
    (bare body or full locator line). Otherwise the suppression is an orphan and
    should be removed or updated."""
    func, line = load_suppressions(Path(args.suppressions))
    # Strip ANSI color codes defensively: `cargo mutants --list` colorizes its
    # output when CARGO_TERM_COLOR=always (as CI sets), which would otherwise make
    # every exact line-suppression "match nothing" and fail the check spuriously.
    ansi = re.compile(r"\x1b\[[0-9;]*m")
    universe = [ansi.sub("", m) for m in read_lines(Path(args.universe))]
    if not universe:
        print(f"ERROR: empty mutant universe at {args.universe}", file=sys.stderr)
        return 2
    bodies = {mutant_body(m) for m in universe}
    full = set(universe)

    orphans: list[str] = []
    for e in func:
        pat = e.get("pattern")
        if not pat:
            continue
        try:
            rx = re.compile(pat)
        except re.error as err:
            print(f"bad regex in suppression ({e.get('file', '?')}): {err}", file=sys.stderr)
            return 2
        if not any(rx.search(b) for b in bodies):
            orphans.append(f'[function] {e.get("file", "?")}: pattern "{pat}"')
    for e in line:
        m = e.get("mutant", "").strip()
        if m and m not in bodies and m not in full:
            orphans.append(f'[line] {e.get("file", "?")}: "{m[:90]}"')

    total = len(func) + len(line)
    if orphans:
        print(f"❌ {len(orphans)}/{total} suppressions are stale (no live mutant matches).")
        print("Remove or update these — a suppression that matches nothing is a "
              "silent blind spot:")
        for o in orphans:
            print(f"  - {o}")
        return 1
    print(f"✅ all {total} suppressions match a live mutant.")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)

    pe = sub.add_parser("exclude-re")
    pe.add_argument("--suppressions", required=True)
    pe.set_defaults(func=cmd_exclude_re)

    pp = sub.add_parser("progress", help="count planned and tested mutants")
    pp.add_argument("--results", required=True)
    pp.set_defaults(func=cmd_progress)

    pm = sub.add_parser("merge", help="combine sharded results against the full inventory")
    pm.add_argument("--inventory", required=True,
                    help="file of all mutant lines of the run (cargo mutants --list)")
    pm.add_argument("--out", required=True)
    pm.add_argument("shards", nargs="*", help="one directory per shard: mutants.out/ + status")
    pm.set_defaults(func=cmd_merge)

    pr = sub.add_parser("report")
    pr.add_argument("--results", required=True)
    pr.add_argument("--suppressions", default=None)
    pr.add_argument("--status", default=None,
                    help="the driver's status file; fail unless it says complete")
    pr.add_argument("--comment", default=None, help="write Markdown report here")
    pr.add_argument("--summary", default=None, help="append report here (GITHUB_STEP_SUMMARY)")
    pr.set_defaults(func=cmd_report)

    po = sub.add_parser("orphans", help="flag suppressions matching no live mutant")
    po.add_argument("--suppressions", required=True)
    po.add_argument("--universe", required=True,
                    help="file of all mutant lines (cargo-mutants --list + umutate.py plan)")
    po.set_defaults(func=cmd_orphans)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
