#!/usr/bin/env python3
"""Score and gate a mutation run for mega-evm.

Subcommands:

  exclude-re  --suppressions <toml>
        Print one `--exclude-re <regex>` pair per line for every function-scoped
        suppression. Consumed by scripts/mutation_test.sh so suppressed
        functions are never generated as mutants.

  note-empty  --log <cargo-mutants output> --results <mutants.out dir>
        Record that cargo-mutants itself said it had nothing to mutate. With
        `--in-diff`, cargo-mutants writes no results at all when the diff is
        empty or touches no mutant, and says so only in its output; this writes
        that line to <results>/no-mutants.txt. Does nothing when <results>
        exists, and fails when it does not and the output says no such thing.

  report      --results <mutants.out dir> [--suppressions <toml>]
              [--comment <path>] [--summary <path>]
        Read the run outcomes, apply line-scoped suppressions, compute the
        mutation score, write a Markdown report, and exit non-zero if any
        unsuppressed survivor or timeout remains (the "no new survivors" gate).
        Exits 2, with a report saying why, when the results cannot be scored.

  orphans     --suppressions <toml> --universe <file>
        Flag suppressions that match no live mutant.

The results a run leaves are read fail-closed. A run is scored from its
`outcomes.json`, cargo-mutants' own record (scripts/umutate.py writes the same
shape): `total_mutants`, and `outcomes`, each with a `scenario`, `"Baseline"` or
`{"Mutant": {"name": ...}}`, and a `summary`. It is scored only if its baseline
succeeded and every mutant has an outcome. A run without `outcomes.json` tested
nothing, and passes only on the producer's own word that it had nothing to
test: a `mutants.json` holding `[]`, which cargo-mutants writes when its
filters leave no mutant, or a `no-mutants.txt` written by `note-empty`.
Anything else fails: no results directory, no outcomes, a baseline that failed
or is missing, a mutant without an outcome, an outcome of a kind this script
does not know.

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
from dataclasses import dataclass, field
from pathlib import Path

# Cap how many survivors are rendered inline in the PR comment (GitHub caps a
# single comment at 65536 chars). The rest live in the run artifacts.
MAX_SURVIVORS_SHOWN = 20

# What cargo-mutants 27 prints, and writes no results for, when `--in-diff`
# leaves it nothing to mutate.
NOTHING_TO_MUTATE = ("No mutants to filter", "Diff file is empty")

# The file `note-empty` records such a line in.
NO_MUTANTS_NOTE = "no-mutants.txt"

# The outcome kinds a mutant can have, by the list they are reported in.
MUTANT_SUMMARIES = {
    "CaughtMutant": "caught",
    "MissedMutant": "missed",
    "Timeout": "timeout",
    "Unviable": "unviable",
}

ANSI = re.compile(r"\x1b\[[0-9;]*m")


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


class ResultsError(Exception):
    """Results that cannot be scored."""


@dataclass
class Run:
    """What a run decided about its mutants, by name."""

    caught: list[str] = field(default_factory=list)
    missed: list[str] = field(default_factory=list)
    timeout: list[str] = field(default_factory=list)
    unviable: list[str] = field(default_factory=list)
    # Set when the run tested nothing: the producer's own word for it.
    nothing_to_test: str | None = None


def nothing_to_mutate(line: str) -> bool:
    """Whether a line of cargo-mutants' output says it had nothing to mutate."""
    return any(re.search(rf"\b(INFO|WARN)\s+{re.escape(m)}\s*$", line) for m in NOTHING_TO_MUTATE)


def empty_list_evidence(results: Path) -> str | None:
    """The producer's own word that it had nothing to test, if it gave it."""
    listed = results / "mutants.json"
    if listed.exists():
        try:
            mutants = json.loads(listed.read_text())
        except json.JSONDecodeError as err:
            raise ResultsError(f"{listed} is not JSON: {err}") from err
        if mutants == []:
            return "the mutant list under the run's filters was empty"
        raise ResultsError(
            f"{listed} lists {len(mutants)} mutants but {results} has no outcomes.json: "
            f"the run stopped before it recorded an outcome"
        )
    note = results / NO_MUTANTS_NOTE
    if note.exists():
        text = note.read_text().strip()
        if nothing_to_mutate(text):
            return f"cargo-mutants said: `{text}`"
        raise ResultsError(f"{note} does not hold a line cargo-mutants prints: {text!r}")
    return None


def load_run(results: Path) -> Run:
    """Read a run's results fail-closed; see the module docstring."""
    if not results.is_dir():
        raise ResultsError(
            f"no results at {results}: the run did not happen, crashed before it wrote "
            f"anything, or wrote elsewhere"
        )
    outcomes_path = results / "outcomes.json"
    if not outcomes_path.exists():
        evidence = empty_list_evidence(results)
        if evidence is None:
            raise ResultsError(
                f"{results} has no outcomes.json and no word from the producer that it had "
                f"nothing to test: the run aborted or changed its output format"
            )
        return Run(nothing_to_test=evidence)
    try:
        data = json.loads(outcomes_path.read_text())
        outcomes = data["outcomes"]
        total = data["total_mutants"]
    except (json.JSONDecodeError, KeyError, TypeError) as err:
        raise ResultsError(f"{outcomes_path} is not a mutation run's outcomes: {err!r}") from err

    baselines = [o for o in outcomes if o.get("scenario") == "Baseline"]
    if not baselines:
        raise ResultsError(
            f"{outcomes_path} has no baseline outcome: nothing shows the unmutated tree "
            f"passes its tests, so a failing test proves nothing about a mutant"
        )
    failed = [o.get("summary") for o in baselines if o.get("summary") != "Success"]
    if failed:
        raise ResultsError(
            f"the baseline failed ({failed[0]}): the unmutated tree does not pass its tests, "
            f"so every mutant would read as caught. Fix the tests first"
        )

    run = Run()
    mutants = [o for o in outcomes if isinstance(o.get("scenario"), dict)]
    for outcome in mutants:
        try:
            name = outcome["scenario"]["Mutant"]["name"]
        except (KeyError, TypeError) as err:
            raise ResultsError(f"an outcome in {outcomes_path} names no mutant: {outcome!r}") from err
        kind = MUTANT_SUMMARIES.get(outcome.get("summary"))
        if kind is None:
            raise ResultsError(
                f"{name}: outcome {outcome.get('summary')!r} is none this gate knows "
                f"({', '.join(MUTANT_SUMMARIES)})"
            )
        getattr(run, kind).append(name)
    if len(mutants) != total:
        raise ResultsError(
            f"{len(mutants)} of {total} mutants have an outcome in {outcomes_path}: "
            f"the run was interrupted"
        )
    if total == 0:
        run.nothing_to_test = "the run tested no mutant"
    return run


def cmd_note_empty(args: argparse.Namespace) -> int:
    results = Path(args.results)
    if results.exists():
        return 0
    log = Path(args.log)
    lines = [ANSI.sub("", ln).rstrip() for ln in log.read_text().splitlines()] if log.exists() else []
    said = [ln.strip() for ln in lines if nothing_to_mutate(ln)]
    if not said:
        print(
            f"ERROR: cargo-mutants wrote no results at {results}, and its output ({log}) does not "
            f"say it had nothing to mutate.",
            file=sys.stderr,
        )
        return 1
    results.mkdir(parents=True)
    (results / NO_MUTANTS_NOTE).write_text(said[-1] + "\n")
    print(f"cargo-mutants had nothing to mutate: {said[-1]}", file=sys.stderr)
    return 0


def suppressed_names(path: str | None) -> set[str]:
    """The mutants line-scoped suppressions name.

    A line suppression matches either the bare mutation text (`mutant` written
    without a locator) or the full `file:line:col: text` line. The latter lets
    two mutants that share identical source text be suppressed independently.
    """
    _, line_supp = load_suppressions(Path(path)) if path else ([], [])
    return {e["mutant"].strip() for e in line_supp if "mutant" in e}


def is_suppressed(name: str, supp: set[str]) -> bool:
    return name in supp or mutant_body(name) in supp


def write_report(report: str, args: argparse.Namespace) -> None:
    if args.comment:
        Path(args.comment).write_text(report)
    if args.summary:
        with open(args.summary, "a") as fh:
            fh.write(report)
    print(report)


def cmd_report(args: argparse.Namespace) -> int:
    results = Path(args.results)
    try:
        run = load_run(results)
    except ResultsError as err:
        write_report(
            "## 🧬 Mutation testing — ❌ FAIL\n\n"
            f"**The results cannot be scored:** {err}.\n",
            args,
        )
        return 2

    if run.nothing_to_test is not None and not (run.timeout or run.caught or run.missed):
        note = f"**Nothing to test**: {run.nothing_to_test}"
        if run.unviable:
            note += f" ({len(run.unviable)} unviable)"
        write_report(f"## 🧬 Mutation testing — ✅ PASS\n\n{note}.\n", args)
        return 0

    caught, missed, timeout = run.caught, run.missed, run.timeout

    supp = suppressed_names(args.suppressions)

    def partition(items: list[str]) -> tuple[list[str], list[str]]:
        sup, real = [], []
        for m in items:
            (sup if is_suppressed(m, supp) else real).append(m)
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
    gate_pass = not real_survivors and not real_timeouts

    # ---- Markdown report (PR comment + step summary) ----
    status = "✅ PASS" if gate_pass else "❌ FAIL"

    # No viable mutants and nothing inconclusive (every mutant unviable, or every
    # timeout suppressed). Reporting "100% (0/0)" reads like a real result and
    # confuses readers — say plainly that nothing was tested.
    if viable == 0 and not real_timeouts:
        note = "**Nothing to test** — no viable mutant"
        note += f" ({len(run.unviable)} unviable, {len(timeout)} timed out)"
        write_report(f"## 🧬 Mutation testing — ✅ PASS\n\n{note}.\n", args)
        return 0

    md = [
        f"## 🧬 Mutation testing — {status}",
        "",
        f"**Diff mutation score: {score:.1f}%** ({killed}/{scored} viable mutants killed)",
        "",
        f"- caught: {len(caught)}",
        f"- survived (real gaps): **{len(real_survivors)}**",
        f"- timed out (inconclusive): **{len(real_timeouts)}**",
        f"- suppressed (equivalent/dead-code): {len(suppressed) + len(supp_timeouts)}",
        f"- unviable: {len(run.unviable)} · timeout total: {len(timeout)}",
    ]
    md.append("")

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
    else:
        md.append("No new test gaps introduced by this change. 🎉")
    write_report("\n".join(md) + "\n", args)

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

    pn = sub.add_parser("note-empty", help="record cargo-mutants saying it had nothing to mutate")
    pn.add_argument("--log", required=True, help="cargo-mutants' output")
    pn.add_argument("--results", required=True)
    pn.set_defaults(func=cmd_note_empty)

    pr = sub.add_parser("report")
    pr.add_argument("--results", required=True)
    pr.add_argument("--suppressions", default=None)
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
