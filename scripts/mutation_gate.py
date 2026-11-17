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
        shard's mutants.out/ and status; pass one per shard, including shards
        that may have uploaded nothing. Every shard's own status is folded into
        the merged status and named in the report: a shard whose budget ran out
        makes the run partial, one that failed its baseline makes it
        baseline-failed, and one that errored, left no status or uploaded
        nothing makes it error. Every shard outcome is kept. An outcome the
        inventory does not plan, a second outcome for one mutant, or shard plans
        that do not add up to the inventory mean the shards and the inventory
        disagree on the mutant set: status mismatch. Only complete passes.

  report      --results <mutants.out dir> [--suppressions <toml>]
              [--status <file>] [--scope diff|full]
              [--title <name>] [--informational]
              [--comment <path>] [--summary <path>]
        Read the run outcomes, apply line-scoped suppressions (each anchored to
        one mutant's file:line:col and text), compute the
        mutation score, write a Markdown report, and exit non-zero if any
        unsuppressed survivor remains (the "no new survivors" gate). With
        --status (the driver's MUTANTS-STATUS line), a run that did not test
        every planned mutant also fails: what it did test is still reported.
        --scope says what the run covered, for the report's wording: `diff`
        (the default) for a PR's changed lines, `full` for every mutant of a
        crate or subsystem (the nightly runs).
        --title names the report in its heading, for a run that is not the
        cargo-mutants gate (`Mutation testing`) or full run (`Mutation testing
        (full run)`, the default with --scope full); a titled full-scope report
        labels its score `Mutation score`, not `Full-run mutation score`. An
        empty title is refused (exit 2).
        --informational is for a run that must not fail its job: the heading
        says what the run found (survivors, timeouts, an incomplete run) instead
        of PASS/FAIL, the note on an incomplete run does not say it fails, and
        the exit code is 0. Results that cannot be read and suppressions that
        cannot be applied still exit 2.

The PR gate runs diff-scoped (cargo mutants --in-diff), so every mutant it sees
lives on a line the PR changed; an unsuppressed survivor there is a test gap the
PR introduced.
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


# A mutant line as both engines write it: `file:line:col: text`.
LOCATOR = re.compile(r"^(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<text>.+)$")

Anchor = tuple[str, int, int, str]


class SuppressionError(Exception):
    """suppressions.toml has an entry the gate cannot apply safely."""


def mutant_anchor(line: str) -> Anchor | None:
    """(file, line, column, text) of a mutant line; None if it has no locator."""
    m = LOCATOR.match(line)
    return (m["file"], int(m["line"]), int(m["col"]), m["text"]) if m else None


def entry_anchor(e: dict) -> Anchor:
    return (e["file"], e["line"], e["column"], e["mutant"].strip())


def check_line_entry(e: dict) -> str | None:
    """Why a line suppression cannot be applied, or None if it is well formed.

    A line entry names one mutant: its file, line and column, and its exact text.
    Matching on the text alone would also cover the same mutation anywhere else in
    the function, including code added after the review."""
    def positive(v: object) -> bool:
        return isinstance(v, int) and not isinstance(v, bool) and v > 0
    text = e.get("mutant")
    if not isinstance(text, str) or not text.strip():
        return "no `mutant` text"
    if LOCATOR.match(text.strip()):
        return "`mutant` carries a `file:line:col:` prefix; put the location in `line` and `column`"
    if not isinstance(e.get("file"), str) or not e["file"]:
        return "no `file`"
    if not (positive(e.get("line")) and positive(e.get("column"))):
        return "no `line` and `column` (positive integers) anchoring it to one mutant"
    return None


def load_suppressions(path: Path) -> tuple[list[dict], list[dict]]:
    """Return (function_scoped, line_scoped) suppression entries.

    Raises SuppressionError if a line entry is not anchored to one mutant."""
    if not path or not path.exists():
        return [], []
    data = tomllib.loads(path.read_text())
    entries = data.get("suppress", [])
    func = [e for e in entries if e.get("kind") == "function"]
    line = [e for e in entries if e.get("kind") == "line"]
    bad = [f'{e.get("file", "?")}: "{str(e.get("mutant", ""))[:90]}": {why}'
           for e in line if (why := check_line_entry(e))]
    if bad:
        raise SuppressionError(f"{len(bad)} line suppression(s) in {path} cannot be applied:\n"
                               + "\n".join(f"  - {b}" for b in bad))
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


# Why a shard (or a single run) did not test all of its mutants, by status.
# `no-status` and `no-artifact` are recorded by `merge` for a shard that left no
# status line or no directory at all.
SHARD_CAUSES = {
    "partial": "its time budget ran out",
    "not-started": "its time budget ran out before it tested any mutant",
    "baseline-failed": "the unmutated tree failed to build or to pass its tests",
    "error": "the run itself failed",
    "no-status": "it ended without a status line (cancelled, killed by the job timeout, "
                 "or crashed)",
    "no-artifact": "it uploaded no results",
}
BUDGET_STATES = ("partial", "not-started")


def incomplete_note(status: dict[str, str], informational: bool = False) -> str:
    """Describe a run that did not test every planned mutant.

    A gate's note says the run never passes; an informational report's does not,
    since it passes or fails nothing."""
    state, expected, outcomes = status.get("status"), status.get("expected"), status.get("outcomes")
    never = "" if informational else ", and an incomplete run never passes"
    tail = f" The results below cover only the tested mutants{never}."
    # A merged run names each shard that did not complete.
    merged = "shards" in status
    shard_states = [s for s in status.get("shards", "").split(",") if s]
    problems = [(i, s) for i, s in enumerate(shard_states) if s != "complete"]
    listed = "".join(f"\n- shard {i}: `{s}` — {SHARD_CAUSES.get(s, 'see its log')}"
                     for i, s in problems)
    if merged and not shard_states:
        return f"**Incomplete run** — no shard results were found ({expected} planned).{tail}"
    if state == "mismatch":
        why = []
        if status.get("unplanned", "0") != "0":
            why.append(f"{status['unplanned']} shard outcome(s) are for mutants the inventory "
                       f"does not list")
        if status.get("duplicated", "0") != "0":
            why.append(f"{status['duplicated']} mutant(s) have more than one outcome")
        planned = status.get("shards-planned", "unknown")
        if planned not in ("unknown", expected):
            why.append(f"the shards planned {planned} mutants")
        never_passes = "" if informational else ", so this run never passes"
        return (f"**Shards and inventory disagree** — the inventory lists {expected} mutants, "
                f"{outcomes} of them have an outcome, and {'; '.join(why)}. The shards did not "
                f"test the inventory's mutant set{never_passes}. Every shard outcome is still reported "
                f"below; `unplanned.txt` in the run artifacts lists the outcomes that do not "
                f"fit the inventory.{listed}")
    budget_only = all(s in BUDGET_STATES for _, s in problems)
    if state == "partial" and budget_only:
        return (f"**Incomplete run** — the time budget ran out after {outcomes} of {expected} "
                f"planned mutants; the rest were not tested.{tail}{listed}")
    if state == "not-started" and budget_only:
        return ("**Incomplete run** — the time budget ran out before any mutant was tested "
                f"({expected} planned). Nothing below is a result{never}.{listed}")
    if problems:
        return (f"**Incomplete run** — {outcomes} of {expected} planned mutants were tested; "
                f"these shards did not complete:{listed}\n\n{tail.strip()}")
    cause = SHARD_CAUSES.get(state or "", "see the run log")
    return (f"**Run did not complete** (status `{state}`: {cause}; {outcomes} of {expected} "
            f"mutants tested).{tail}")


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
    # Each shard's state, and its own plan (`expected`); the plans must add up to
    # the inventory.
    shard_states: list[str] = []
    shard_plans: list[int | None] = []
    print("| shard | status |")
    print("|---|---|")
    for shard in args.shards:
        shard = Path(shard)
        status = read_status(shard / "status")
        if not shard.is_dir():
            shard_states.append("no-artifact")
        else:
            shard_states.append((status or {}).get("status", "no-status"))
        line = " ".join(f"{k}={v}" for k, v in status.items()) if status else shard_states[-1]
        print(f"| `{shard}` | {line} |")
        plan = (status or {}).get("expected", "")
        shard_plans.append(int(plan) if plan.isdigit() else None)
        for name in OUTCOME_FILES:
            for m in read_lines(shard / "mutants.out" / name):
                merged[name].append(m)
                seen[m] += 1
    # The shards and the inventory must describe the same mutant set. An outcome
    # for a mutant the inventory does not plan, more outcomes for a mutant than the
    # inventory plans, or shard plans that do not add up to the inventory show they
    # do not: completeness is measured against the inventory, so it proves nothing
    # then, and the run cannot pass.
    unplanned = [m for m in seen if m not in planned]
    duplicated = [m for m in seen if m in planned and seen[m] > planned[m]]
    plans_known = bool(shard_plans) and None not in shard_plans
    shards_planned = sum(p for p in shard_plans if p is not None) if plans_known else None
    out = Path(args.out)
    results = out / "mutants.out"
    results.mkdir(parents=True, exist_ok=True)
    (results / "mutants.json").write_text(json.dumps([{"name": m} for m in inventory]))
    for name, items in merged.items():
        (results / name).write_text("".join(f"{m}\n" for m in items))
    (out / "unplanned.txt").write_text("".join(f"{m}\n" for m in unplanned + duplicated))
    expected = len(inventory)
    outcomes = sum(min(n, planned[m]) for m, n in seen.items() if m in planned)
    # The most specific cause wins: a mismatch, then a failed baseline, then any
    # other shard failure. A shortfall that only budget-stopped shards explain is
    # partial or not-started, as for a single run.
    if unplanned or duplicated or (plans_known and shards_planned != expected):
        state = "mismatch"
    elif "baseline-failed" in shard_states:
        state = "baseline-failed"
    elif any(s not in ("complete", *BUDGET_STATES) for s in shard_states):
        state = "error"
    else:
        state = "complete" if outcomes == expected else "partial" if outcomes else "not-started"
    line = (f"MUTANTS-STATUS status={state} expected={expected} outcomes={outcomes} "
            f"unplanned={len(unplanned)} duplicated={len(duplicated)} "
            f"shards-planned={'unknown' if shards_planned is None else shards_planned} "
            f"shards={','.join(shard_states)}")
    (out / "status").write_text(line + "\n")
    found = sum(s != "no-artifact" for s in shard_states)
    print(f"\n{len(args.shards)} shard(s), {found} with results.\n")
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


def verdict(args: argparse.Namespace, survivors: int = 0, timeouts: int = 0,
            incomplete: bool = False) -> str:
    """The verdict in the report's heading, from what the run found.

    A gate passes when it found nothing: no unsuppressed survivor or timeout, and
    a complete run. An informational run neither passes nor fails, so it says
    what it found."""
    found_any = bool(survivors or timeouts or incomplete)
    if not args.informational:
        return "❌ FAIL" if found_any else "✅ PASS"
    if not found_any:
        return "✅ no survivors"
    found = [f"{survivors} survivor{'s' if survivors != 1 else ''}"] if survivors else []
    found += [f"{timeouts} timed out"] if timeouts else []
    found += ["incomplete run"] if incomplete else []
    return f"⚠️ {', '.join(found)} (informational; does not fail the run)"


def cmd_report(args: argparse.Namespace) -> int:
    results = Path(args.results)
    full = args.scope == "full"
    name = args.title or ("Mutation testing (full run)" if full else "Mutation testing")
    title = f"## 🧬 {name}"
    # An informational run reports what it found and never fails; a gate fails.
    fail_code = 0 if args.informational else 1

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
            note = incomplete_note(run_status, args.informational)

    # Guard against a silent pass when the run produced no results. We must tell
    # apart two cases that both leave the outcome lists empty:
    #   * the results dir is absent  -> no run happened (e.g. the diff had no
    #     mutatable changes); benign, report "nothing tested" and pass.
    #   * the results dir exists but the expected files are missing -> the run
    #     aborted, wrote elsewhere, or the tool renamed its output. Refusing to
    #     report a 100% pass here is the whole point.
    if not results.exists():
        if note:
            emit(f"{title} — {verdict(args, incomplete=True)}\n\n{note}\n", args)
            return fail_code
        report = f"{title}\n\nNo results at "
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
        emit(f"{title} — {verdict(args, incomplete=True)}\n\n{note}\n", args)
        return fail_code

    _, line_supp = load_suppressions(Path(args.suppressions)) if args.suppressions else ([], [])
    # A line suppression matches one mutant: the same file, line, column and text.
    # The same text anywhere else (another `&&` in the function, or the reviewed
    # one after the code above it moved) stays gated.
    supp = {entry_anchor(e) for e in line_supp}

    def partition(items: list[str]) -> tuple[list[str], list[str]]:
        sup, real = [], []
        for m in items:
            (sup if mutant_anchor(m) in supp else real).append(m)
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
    status = verdict(args, len(real_survivors), len(real_timeouts), note is not None)

    # No viable mutants and nothing inconclusive (typical diff run whose changed
    # lines contain nothing mutatable). Reporting "100% (0/0)" reads like a real
    # result and confuses readers — say plainly that nothing was tested.
    if viable == 0 and not real_timeouts and note is None:
        nothing = "**Nothing to test** — no mutants were generated"
        if unviable or timeout:
            nothing += f" ({len(unviable)} unviable, {len(timeout)} timed out)"
        elif not full:
            nothing += " on the changed lines"
        emit(f"{title} — {verdict(args)}\n\n{nothing}.\n", args)
        return 0

    # `Full-run` names the cargo-mutants full run, the untitled full-scope report.
    label = ("Diff mutation score" if not full
             else "Mutation score" if args.title else "Full-run mutation score")
    md = [
        f"{title} — {status}",
        "",
        *([note, ""] if note else []),
        f"**{label}: {score:.1f}%** ({killed}/{scored} viable mutants killed)",
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
        md.append("Every viable mutant was caught or suppressed. 🎉" if full
                  else "No new test gaps introduced by this change. 🎉")
    emit("\n".join(md) + "\n", args)

    return 0 if gate_pass else fail_code


def cmd_orphans(args: argparse.Namespace) -> int:
    """Flag suppressions that match no live mutant (stale = a silent blind spot).

    `--universe` is a file of mutant lines (`file:line:col: text`) covering the
    whole crate, assembled from BOTH engines (cargo-mutants `--list` and
    `umutate.py plan`). A function suppression's regex `pattern` must match at
    least one mutant name; a line suppression must match the mutant at its
    anchor (file, line, column and text). Otherwise the suppression is stale and
    should be removed or re-anchored; for a line entry whose text now sits
    elsewhere in its file, the new locations are listed."""
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
    anchors = {a for a in map(mutant_anchor, universe) if a}

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
        anchor = entry_anchor(e)
        if anchor in anchors:
            continue
        file, ln, col, text = anchor
        moved = sorted((a[1], a[2]) for a in anchors if a[0] == file and a[3] == text)
        hint = f" — the text is now at {', '.join(f'{l}:{c}' for l, c in moved)}" if moved else ""
        orphans.append(f'[line] {file}:{ln}:{col}: "{text[:90]}"{hint}')

    total = len(func) + len(line)
    if orphans:
        print(f"❌ {len(orphans)}/{total} suppressions are stale (no live mutant matches).")
        print("Remove or re-anchor these — a suppression that matches nothing is a "
              "silent blind spot, and a moved one no longer suppresses its mutant:")
        for o in orphans:
            print(f"  - {o}")
        return 1
    print(f"✅ all {total} suppressions match a live mutant.")
    return 0


def report_title(value: str) -> str:
    """`--title`: an empty name would fall back to a cargo-mutants heading."""
    if not value.strip():
        raise argparse.ArgumentTypeError("must not be empty")
    return value


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
    pr.add_argument("--scope", choices=("diff", "full"), default="diff",
                    help="what the run covered: a PR's changed lines, or every mutant")
    pr.add_argument("--title", default=None, type=report_title,
                    help="the report's name in its heading, not empty (default: `Mutation "
                         "testing`, with `(full run)` for --scope full)")
    pr.add_argument("--informational", action="store_true",
                    help="say what the run found instead of PASS/FAIL, and exit 0 "
                         "(2 still means the results could not be read)")
    pr.add_argument("--comment", default=None, help="write Markdown report here")
    pr.add_argument("--summary", default=None, help="append report here (GITHUB_STEP_SUMMARY)")
    pr.set_defaults(func=cmd_report)

    po = sub.add_parser("orphans", help="flag suppressions matching no live mutant")
    po.add_argument("--suppressions", required=True)
    po.add_argument("--universe", required=True,
                    help="file of all mutant lines (cargo-mutants --list + umutate.py plan)")
    po.set_defaults(func=cmd_orphans)

    args = p.parse_args()
    try:
        return args.func(args)
    except SuppressionError as err:
        print(f"ERROR: {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
