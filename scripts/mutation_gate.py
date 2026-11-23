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
        empty, changes no Rust source file or touches no mutant, and says so
        only in its output; this writes
        that line to <results>/no-mutants.txt. Does nothing when <results>
        exists, and fails when it does not and the output says no such thing.

  timeout-re  --results <mutants.out dir> [--suppressions <toml>] [--with-files]
        Print one `--re <regex>` pair per line for every unsuppressed mutant
        that timed out, each regex matching that mutant's name alone, and with
        --with-files one `-f <file>` pair per file they are in. Consumed by
        scripts/mutation_test.sh to re-run those mutants one at a time.

  missed-re   --results <mutants.out dir> [--recheck <mutants.out dir>]
              [--suppressions <toml>] [--with-files]
        Print one `--re <regex>` pair per line for every unsuppressed mutant
        the tests missed: in the run, or in the re-check of one that timed out.
        Consumed by scripts/mutation_test.sh to run those mutants against the
        Mega EEST suite.

  progress    --results <mutants.out dir>
        Print `expected=<n|unknown> outcomes=<n>`: how many mutants the run
        planned (unknown before cargo-mutants wrote its plan) and how many have
        an outcome. Used by scripts/mutation_test.sh for its status line.

  merge       --inventory <file> --out <dir> <shard dir>...
        Combine the results of a run split into shards (cargo mutants --shard)
        into <dir>/mutants.out and <dir>/status, counted against <inventory>
        (`cargo mutants --list` of the whole run). Each <shard dir> holds one
        shard's mutants.out/ and status; pass one per shard, including shards
        that may have uploaded nothing. The merged mutants.out keeps the
        contract below: `mutants.json` is the inventory, `outcomes.json` every
        shard's outcomes and baselines, with an `end_time` only when every
        shard finished and the run is complete; the outcome lists are written
        beside it. Every shard's own status is folded into the merged status
        and named in the report: a shard whose budget ran out makes the run
        partial, one that failed its baseline makes it baseline-failed, and one
        that errored, left no status or uploaded nothing makes it error. Every
        shard outcome is kept. An outcome the inventory does not plan, a second
        outcome for one mutant, or shard plans that do not add up to the
        inventory mean the shards and the inventory disagree on the mutant set:
        status mismatch. Only complete passes.

  merge-runs  --out <mutants.out dir> <mutants.out dir>...
        Combine the shards' follow-up runs (the re-checks of timed-out mutants,
        the Mega EEST runs of missed ones) into one record the report reads as
        --recheck or --mega-eest: their selections and outcomes together, each
        mutant's log copied beside them, an `end_time` only when every one
        finished, and the abnormal-exit note of any one. Directories that do
        not exist are skipped; with none, nothing is written.

  report      --results <mutants.out dir> [--recheck <mutants.out dir>]
              [--mega-eest <mutants.out dir>] [--status <file>] [--scope diff|full]
              [--suppressions <toml>] [--comment <path>] [--summary <path>]
        Read the run outcomes, take the re-check's outcome for a mutant that
        timed out and the Mega EEST run's outcome for a mutant the tests
        missed (a kill there only with the suite's `result=FAIL` summary line
        in that mutant's log; a caught outcome without it makes the results
        unscorable), apply line-scoped suppressions (each anchored to one mutant's
        file:line:col and text), compute the mutation score,
        write a Markdown report, and exit non-zero if any unsuppressed survivor
        or timeout remains (the "no new survivors" gate). Exits 2, with a
        report saying why, when the results cannot be scored. With --status
        (the driver's MUTANTS-STATUS line), a run that did not test every
        planned mutant also fails: what it did test is still reported, read
        without the completeness checks below. --scope says what the run
        covered, for the report's wording: `diff` (the default) for a PR's
        changed lines, `full` for every mutant of the crate (the nightly runs).

  orphans     --suppressions <toml> --universe <file>
        Flag suppressions that match no live mutant: a function pattern that
        matches no mutant name, a line entry whose anchor (file, line, column
        and text) is no mutant's, with the locations its text has moved to.

The results a run leaves are read fail-closed, on the contract cargo-mutants 27
keeps and scripts/umutate.py keeps with it. Before it tests anything the
producer writes `mutants.json`, a list of objects each with the `name` of a
mutant it selected. It then writes `outcomes.json`: `outcomes`, each with a
`scenario`, `"Baseline"` or `{"Mutant": {"name": ...}}`, and a `summary`; and
`end_time`, null until the run finishes. cargo-mutants rewrites `outcomes.json`
after every outcome, so a run that stops part way leaves one that looks whole;
its `total_mutants` counts the outcomes recorded so far, not the mutants
selected, and proves nothing about completion. A run is scored only if its
`end_time` is set, its baseline succeeded, and its outcomes name exactly the
mutants `mutants.json` selects. A run without `outcomes.json` tested nothing,
and passes only on the producer's own word that it had nothing to test: a
`mutants.json` holding `[]`, which cargo-mutants writes when its filters leave
no mutant, or a `no-mutants.txt` written by `note-empty`. Anything else fails:
no results directory, no outcomes, a run that did not finish, a baseline that
failed or is missing, no `mutants.json` beside the outcomes, a selected mutant
without an outcome or an outcome of a mutant not selected, an outcome of a kind
this script does not know, and results holding the `run-failed.txt` that
scripts/mutation_test.sh writes into them when the run that wrote them exited
abnormally, whatever else they hold.

A line suppression names one mutant: its `file`, `line` and `column`, and its
exact `mutant` text without the `file:line:col: ` prefix. The same text anywhere
else (a new `&&` in the function, or the reviewed one after the code above it
moved) stays gated; an entry without a well-formed anchor is refused (exit 2).

The PR gate runs diff-scoped (cargo mutants --in-diff), so every mutant it sees
lives on a line the PR changed; an unsuppressed survivor there is a test gap the
PR introduced.
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
from collections import Counter

# tomllib is new in Python 3.11: say so rather than fail on the import below.
if sys.version_info < (3, 11):
    sys.exit(f"scripts/mutation_gate.py needs Python 3.11 or newer, for tomllib; this is Python "
             f"{sys.version.split()[0]}.")

import tomllib  # noqa: E402
from dataclasses import dataclass, field  # noqa: E402
from pathlib import Path  # noqa: E402

# Cap how many survivors are rendered inline in the PR comment (GitHub caps a
# single comment at 65536 chars). The rest live in the run artifacts.
MAX_SURVIVORS_SHOWN = 20

# What cargo-mutants 27 prints, and writes no results for, when `--in-diff`
# leaves it nothing to mutate: a diff touching no mutant, an empty diff, and a
# diff changing no `.rs` file (a module guide under src/, say).
NOTHING_TO_MUTATE = (
    "No mutants to filter",
    "Diff file is empty",
    "Diff changes no Rust source files",
)

# The file `note-empty` records such a line in.
NO_MUTANTS_NOTE = "no-mutants.txt"

# The file scripts/mutation_test.sh writes into the results of a run it started that exited
# abnormally: an exit the files the run left cannot show.
RUN_FAILED_NOTE = "run-failed.txt"

# The outcome kinds a mutant can have, by the list they are reported in.
MUTANT_SUMMARIES = {
    "CaughtMutant": "caught",
    "MissedMutant": "missed",
    "Timeout": "timeout",
    "Unviable": "unviable",
}

ANSI = re.compile(r"\x1b\[[0-9;]*m")


# A mutant line as both engines write it: `file:line:col: text`.
LOCATOR = re.compile(r"^(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<text>.+)$")

Anchor = tuple[str, int, int, str]


class SuppressionError(Exception):
    """suppressions.toml has an entry the gate cannot apply safely."""


def mutant_anchor(line: str) -> Anchor | None:
    """(file, line, column, text) of a mutant line; None if it has no locator."""
    m = LOCATOR.match(ANSI.sub("", line))
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
    # Each mutant's log, relative to the results directory, as the outcome names it.
    logs: dict[str, str] = field(default_factory=dict)


def nothing_to_mutate(line: str) -> bool:
    """Whether a line of cargo-mutants' output says it had nothing to mutate."""
    return any(re.search(rf"\b(INFO|WARN)\s+{re.escape(m)}\s*$", line) for m in NOTHING_TO_MUTATE)


def read_selection(results: Path) -> list[str] | None:
    """The names of the mutants a run selected, from its `mutants.json`; None without one."""
    listed = results / "mutants.json"
    if not listed.exists():
        return None
    try:
        mutants = json.loads(listed.read_text())
        if not isinstance(mutants, list) or not all(
                isinstance(m, dict) and isinstance(m.get("name"), str) for m in mutants):
            raise TypeError("not a list of objects each with a string `name`")
    except (json.JSONDecodeError, TypeError) as err:
        raise ResultsError(f"{listed} is not a list of mutants: {err!r}") from err
    return [m["name"] for m in mutants]


def empty_list_evidence(results: Path) -> str | None:
    """The producer's own word that it had nothing to test, if it gave it."""
    selected = read_selection(results)
    if selected is not None:
        if not selected:
            return "the mutant list under the run's filters was empty"
        raise ResultsError(
            f"{results / 'mutants.json'} lists {len(selected)} mutants but {results} has no "
            f"outcomes.json: the run stopped before it recorded an outcome"
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
    failed = results / RUN_FAILED_NOTE
    if failed.exists():
        raise ResultsError(
            f"the run that wrote {results} exited abnormally ({failed.read_text().strip()}), "
            f"so what it left does not show that it finished"
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
        if not isinstance(outcomes, list) or not all(isinstance(o, dict) for o in outcomes):
            raise TypeError("`outcomes` is not a list of objects")
    except (json.JSONDecodeError, KeyError, TypeError) as err:
        raise ResultsError(f"{outcomes_path} is not a mutation run's outcomes: {err!r}") from err

    end_time = data.get("end_time")
    if not isinstance(end_time, str) or not end_time:
        raise ResultsError(
            f"{outcomes_path} records no end_time ({end_time!r}): the producer sets it only once "
            f"the run finishes, so this is the record of a run that stopped part way"
        )

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
        if isinstance(outcome.get("log_path"), str):
            run.logs[name] = outcome["log_path"]

    selected = read_selection(results)
    if selected is None:
        raise ResultsError(
            f"{results} has outcomes.json but no mutants.json: nothing says which mutants the "
            f"run selected, so nothing shows each has an outcome"
        )
    tested = Counter(o["scenario"]["Mutant"]["name"] for o in mutants)
    untested = Counter(selected) - tested
    if untested:
        raise ResultsError(
            f"{sum(untested.values())} of the {len(selected)} mutants mutants.json selects have "
            f"no outcome in {outcomes_path}, `{next(iter(untested))}` among them: the run did not "
            f"test them all"
        )
    unselected = tested - Counter(selected)
    if unselected:
        raise ResultsError(
            f"{outcomes_path} records {sum(unselected.values())} outcomes of mutants mutants.json "
            f"does not select, `{next(iter(unselected))}` among them: the two are not one run's"
        )
    if not selected:
        run.nothing_to_test = "the run tested no mutant"
    return run


def load_tested(results: Path) -> Run:
    """What a run that did not finish tested, without the completeness checks of load_run:
    its outcomes.json if it wrote one, else its outcome lists, else nothing."""
    run = Run()
    outcomes_path = results / "outcomes.json"
    if outcomes_path.exists():
        try:
            outcomes = json.loads(outcomes_path.read_text())["outcomes"]
        except (json.JSONDecodeError, KeyError, TypeError) as err:
            raise ResultsError(f"{outcomes_path} is not a mutation run's outcomes: {err!r}") from err
        for outcome in outcomes:
            if not isinstance(outcome, dict) or not isinstance(outcome.get("scenario"), dict):
                continue
            kind = MUTANT_SUMMARIES.get(outcome.get("summary"))
            name = (outcome["scenario"].get("Mutant") or {}).get("name")
            if kind and isinstance(name, str):
                getattr(run, kind).append(name)
        return run
    for kind in MUTANT_SUMMARIES.values():
        getattr(run, kind).extend(read_lines(results / f"{kind}.txt"))
    return run


# One file per cargo-mutants outcome; every tested mutant lands in exactly one.
OUTCOME_FILES = ("caught.txt", "missed.txt", "timeout.txt", "unviable.txt")


def progress(results: Path) -> tuple[int | None, int]:
    """Return (planned mutants or None if no plan was written, mutants with an outcome).

    The outcomes are counted in outcomes.json, which cargo-mutants rewrites after every
    outcome, or, without one, in the outcome lists."""
    plan = results / "mutants.json"
    expected = len(json.loads(plan.read_text())) if plan.exists() else None
    record = results / "outcomes.json"
    if record.exists():
        outcomes, _, _, _ = read_record(results)
        return expected, sum(1 for o in outcomes if outcome_name(o))
    return expected, sum(len(read_lines(results / name)) for name in OUTCOME_FILES)


def cmd_progress(args: argparse.Namespace) -> int:
    try:
        expected, outcomes = progress(Path(args.results))
    except (UnreadableRecord, json.JSONDecodeError, OSError) as err:
        # The driver then reports status=error: nothing says how far the run got.
        print(f"cannot count the run's outcomes: {err}", file=sys.stderr)
        return 2
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
    "followup-stopped": "its time budget ran out in the re-check of a timed-out mutant or the "
                        "Mega EEST run of a missed one",
    "baseline-failed": "the unmutated tree failed to build or to pass its tests",
    "error": "the run itself failed",
    "no-status": "it ended without a status line (cancelled, killed by the job timeout, "
                 "or crashed)",
    "no-artifact": "it uploaded no results",
    "unreadable": "its results cannot be read (a record cut off while it was written)",
    "invalid-results": "its results do not show that it finished",
    "foreign-artifact": "its run identity or shard index does not match this join",
}
BUDGET_STATES = ("partial", "not-started", "followup-stopped")


def incomplete_note(status: dict[str, str]) -> str:
    """Describe a run that did not test every planned mutant."""
    state, expected, outcomes = status.get("status"), status.get("expected"), status.get("outcomes")
    tail = " The results below cover only the tested mutants, and an incomplete run never passes."
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
        return (f"**Shards and inventory disagree** — the inventory lists {expected} mutants, "
                f"{outcomes} of them have an outcome, and {'; '.join(why)}. The shards did not "
                f"test the inventory's mutant set, so this run never passes. Every shard outcome "
                f"is still reported below; `unplanned.txt` in the run artifacts lists the "
                f"outcomes that do not fit the inventory.{listed}")
    budget_only = all(s in BUDGET_STATES for _, s in problems)
    if state == "partial" and budget_only:
        if outcomes == expected:
            return (f"**Incomplete run** — the time budget ran out after all {outcomes} "
                    f"planned outcomes were recorded but before the run finished.{tail}{listed}")
        return (f"**Incomplete run** — the time budget ran out after {outcomes} of {expected} "
                f"planned mutants; the rest were not tested.{tail}{listed}")
    if state == "not-started" and budget_only:
        return ("**Incomplete run** — the time budget ran out before any mutant was tested "
                f"({expected} planned). Nothing below is a result, and an incomplete run "
                f"never passes.{listed}")
    if state == "followup-stopped" and budget_only:
        return (f"**Incomplete run** — the {outcomes} planned mutants were tested, and the time "
                f"budget ran out in the re-check of a timed-out mutant or the Mega EEST run of a "
                f"missed one, so those are not decided.{tail}{listed}")
    if problems:
        return (f"**Incomplete run** — {outcomes} of {expected} planned mutants were tested; "
                f"these shards did not complete:{listed}\n\n{tail.strip()}")
    cause = SHARD_CAUSES.get(state or "", "see the run log")
    return (f"**Run did not complete** (status `{state}`: {cause}; {outcomes} of {expected} "
            f"mutants tested).{tail}")


class UnreadableRecord(Exception):
    """A results directory whose record cannot be read: a run cut off while it wrote it."""


def read_record(results: Path) -> tuple[list[dict], list[str], object, bool]:
    """A results directory's outcomes, its selection, its end_time, and whether its run
    exited abnormally; no outcomes for a directory without outcomes.json. Raises
    UnreadableRecord for a record that is not a run's (truncated, say)."""
    failed = (results / RUN_FAILED_NOTE).exists()
    path = results / "outcomes.json"
    try:
        selection = read_selection(results) or []
        if not path.exists():
            return [], selection, None, failed
        data = json.loads(path.read_text())
        if not isinstance(data, dict) or not isinstance(data.get("outcomes", []), list):
            raise TypeError("not an object with a list of outcomes")
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, TypeError, ResultsError) as err:
        raise UnreadableRecord(f"{results} cannot be read: {err}") from err
    outcomes = [o for o in data.get("outcomes", []) if isinstance(o, dict)]
    return outcomes, selection, data.get("end_time"), failed


def outcome_name(outcome: dict) -> str | None:
    scenario = outcome.get("scenario")
    if not isinstance(scenario, dict):
        return None
    name = (scenario.get("Mutant") or {}).get("name")
    return name if isinstance(name, str) else None


def write_record(out: Path, outcomes: list[dict], selection: list[str], end_time: object) -> None:
    """Write a record in cargo-mutants' contract, with the outcome lists beside it."""
    out.mkdir(parents=True, exist_ok=True)
    (out / "mutants.json").write_text(json.dumps([{"name": m} for m in selection]))
    (out / "outcomes.json").write_text(json.dumps({"outcomes": outcomes, "end_time": end_time}))
    lists: dict[str, list[str]] = {f"{kind}.txt": [] for kind in MUTANT_SUMMARIES.values()}
    for o in outcomes:
        kind, name = MUTANT_SUMMARIES.get(o.get("summary")), outcome_name(o)
        if kind and name:
            lists[f"{kind}.txt"].append(name)
    for file, items in lists.items():
        (out / file).write_text("".join(f"{m}\n" for m in items))


def copy_logs(source: Path, out: Path, prefix: str, outcomes: list[dict]) -> list[dict]:
    """Copy each outcome's log to `out/log/<prefix>-<name>` and point the outcome at the copy."""
    copied = []
    for o in outcomes:
        o = dict(o)
        log = o.get("log_path")
        if isinstance(log, str) and (source / log).is_file():
            target = Path("log") / f"{prefix}-{Path(log).name}"
            (out / target).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / log, out / target)
            o["log_path"] = str(target)
        copied.append(o)
    return copied


def cmd_merge(args: argparse.Namespace) -> int:
    diff = getattr(args, "scope", "full") == "diff"
    if diff and (not args.head or not args.base or len(args.shards) != 4):
        print("ERROR: a diff join requires the tested SHA, base SHA and four shard paths",
              file=sys.stderr)
        return 2
    inventory_path = Path(args.inventory)
    if not inventory_path.is_file():
        print(f"ERROR: missing mutant inventory at {inventory_path}", file=sys.stderr)
        return 2
    inventory = [ANSI.sub("", m) for m in read_lines(inventory_path)]
    if not inventory and not diff:
        print(f"ERROR: empty mutant inventory at {args.inventory}", file=sys.stderr)
        return 2
    planned = Counter(inventory)
    out = Path(args.out)
    results = out / "mutants.out"
    # Every shard outcome goes into the merged record, planned or not: a survivor
    # must never be dropped on the way to the report.
    outcomes: list[dict] = []
    seen: Counter[str] = Counter()
    # Each shard's state, and its own plan (`expected`); the plans must add up to
    # the inventory.
    shard_states: list[str] = []
    shard_plans: list[int | None] = []
    finished = True
    print("| shard | status |")
    print("|---|---|")
    for i, shard in enumerate(map(Path, args.shards)):
        status = read_status(shard / "status")
        if not shard.is_dir():
            shard_states.append("no-artifact")
        else:
            shard_states.append((status or {}).get("status", "no-status"))
        line = " ".join(f"{k}={v}" for k, v in status.items()) if status else shard_states[-1]
        print(f"| `{shard}` | {line} |")
        if diff:
            if status is None:
                shard_plans.append(None)
                continue
            identity = {"head": args.head, "base": args.base, "shard-index": str(i),
                        "shard-count": "4", "run-id": args.run_id, "run-attempt": args.run_attempt}
            mismatches = [f"{key} expected {value!r}, got {status.get(key)!r}"
                          for key, value in identity.items() if status.get(key) != value]
            if mismatches:
                shard_states[-1] = "foreign-artifact"
                shard_plans.append(None)
                print(f"| `{shard}` | {'; '.join(mismatches)} |")
                continue
        plan = (status or {}).get("expected", "")
        shard_plans.append(int(plan) if plan.isdigit() else None)
        shard_results = shard / "mutants.out"
        try:
            shard_outcomes, selection, end_time, _ = read_record(shard_results)
        except UnreadableRecord as err:
            # Named, whatever its status line says; the other shards are still reported.
            shard_states[-1] = "unreadable"
            print(f"| `{shard}` | unreadable: {err} |")
            finished = False
            continue
        if diff and shard_states[-1] == "complete":
            try:
                load_run(shard_results)
                if len(selection) != shard_plans[-1]:
                    raise ResultsError("the status and results disagree on the shard's mutant count")
            except ResultsError as err:
                shard_states[-1] = "invalid-results"
                print(f"| `{shard}` | invalid results: {err} |")
                continue
        if shard_outcomes and not end_time:
            finished = False
        for o in copy_logs(shard_results, results, f"shard{i}", shard_outcomes):
            outcomes.append(o)
            name = outcome_name(o)
            if name is not None:
                seen[name] += 1
    # The shards and the inventory must describe the same mutant set. An outcome
    # for a mutant the inventory does not plan, more outcomes for a mutant than the
    # inventory plans, or shard plans that do not add up to the inventory show they
    # do not: completeness is measured against the inventory, so it proves nothing
    # then, and the run cannot pass.
    unplanned = [m for m in seen if m not in planned]
    duplicated = [m for m in seen if m in planned and seen[m] > planned[m]]
    plans_known = bool(shard_plans) and None not in shard_plans
    shards_planned = sum(p for p in shard_plans if p is not None) if plans_known else None
    expected = len(inventory)
    tested = sum(min(n, planned[m]) for m, n in seen.items() if m in planned)
    # The most specific cause wins: a mismatch, then a failed baseline, then any
    # other shard failure. A shortfall that only budget-stopped shards explain is
    # partial or not-started, as for a single run.
    if unplanned or duplicated or (plans_known and shards_planned != expected):
        state = "mismatch"
    elif "baseline-failed" in shard_states:
        state = "baseline-failed"
    elif any(s not in ("complete", *BUDGET_STATES) for s in shard_states):
        state = "error"
    elif tested == expected and "followup-stopped" in shard_states:
        state = "followup-stopped"
    elif diff and (not shard_states or "partial" in shard_states or "not-started" in shard_states):
        state = "partial" if tested else "not-started"
    else:
        state = "complete" if tested == expected else "partial" if tested else "not-started"
    # The merged record says it finished only when every shard's did and the run is complete;
    # the report reads anything else as what was tested, under the status's note.
    if state == "complete" and not inventory:
        results.mkdir(parents=True, exist_ok=True)
        (results / "outcomes.json").unlink(missing_ok=True)
        (results / "mutants.json").write_text("[]\n")
    else:
        write_record(results, outcomes, inventory,
                     "merged" if state == "complete" and finished else None)
    (out / "unplanned.txt").write_text("".join(f"{m}\n" for m in unplanned + duplicated))
    line = (f"MUTANTS-STATUS status={state} expected={expected} outcomes={tested} "
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


def cmd_merge_runs(args: argparse.Namespace) -> int:
    out = Path(args.out)
    present = [Path(d) for d in args.runs if Path(d).is_dir()]
    if not present:
        print(f"no runs to merge into {out}", file=sys.stderr)
        return 0
    outcomes: list[dict] = []
    selection: list[str] = []
    finished = True
    failed: list[str] = []
    diff = getattr(args, "scope", "full") == "diff"
    for i, run in enumerate(present):
        try:
            if diff:
                load_run(run)
            run_outcomes, run_selection, end_time, run_failed = read_record(run)
            if diff:
                # A follow-up can select field deletions from other shards despite
                # its regex filters. Only the shard that owns a mutant may decide it.
                own = set(read_selection(run.parent.parent / "mutants.out") or [])
                run_selection = [name for name in run_selection if name in own]
                run_outcomes = [o for o in run_outcomes
                                if o.get("scenario") == "Baseline" or outcome_name(o) in own]
        except (UnreadableRecord, ResultsError) as err:
            failed.append(f"{run}: {err}" if diff else str(err))
            finished = False
            continue
        if run_failed:
            failed.append(f"{run}: {(run / RUN_FAILED_NOTE).read_text().strip()}")
        if not end_time:
            finished = False
        selection += run_selection
        outcomes += copy_logs(run, out, f"run{i}", run_outcomes)
    write_record(out, outcomes, selection, "merged" if finished else None)
    if failed:
        (out / RUN_FAILED_NOTE).write_text("; ".join(failed) + "\n")
    print(f"merged {len(present)} run(s) into {out}: {len(selection)} mutant(s) selected, "
          f"{'all finished' if finished else 'not all finished'}", file=sys.stderr)
    return 0


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


def suppressed_anchors(path: str | None) -> set[Anchor]:
    """The mutants the line suppressions name, by anchor; none when no file is given."""
    if not path:
        return set()
    _, line = load_suppressions(Path(path))
    return {entry_anchor(e) for e in line}


def is_suppressed(name: str, anchors: set[Anchor]) -> bool:
    return mutant_anchor(name) in anchors


def cmd_timeout_re(args: argparse.Namespace) -> int:
    try:
        run = load_run(Path(args.results))
    except ResultsError as err:
        # Nothing to re-check; the gate reports why the results cannot be scored.
        print(f"no timed-out mutants to re-check: {err}", file=sys.stderr)
        return 0
    supp = suppressed_anchors(args.suppressions)
    print_selection([name for name in run.timeout if not is_suppressed(name, supp)],
                    getattr(args, "with_files", False))
    return 0


def print_selection(names: list[str], with_files: bool) -> None:
    """One `--re <regex>` pair per line for each name, each regex matching that name alone; with
    `with_files`, one `-f <file>` pair for each file the names are in. The files keep the struct-field
    mutants cargo-mutants adds without matching them against `--re` to those files, for a run the
    first run's own restriction does not scope."""
    for name in names:
        print("--re")
        print(f"^{re.escape(name)}$")
    if with_files:
        for file in sorted({name.split(":", 1)[0] for name in names}):
            print("-f")
            print(file)


# The summary line scripts/mega_eest.sh prints, as the mega_eest test prints it again into a
# mutant's log; its `result` says whether the suite ran to a verdict.
MEGA_EEST_SUMMARY = re.compile(r"MEGA-EEST rev=[0-9a-f]{40} [^\n]*")
MEGA_EEST_MODE_COUNTS = re.compile(
    r"\b(?:plain|instrumented)=pass:\d+,fail:(\d+),unsupported:\d+,error:(\d+)\b")


def mega_eest_kill_unproven(results: Path, log_path: str | None) -> str | None:
    """Why a mutant the Mega EEST run reports caught is not shown to be caught; None when it is.

    The run's test fails on any failure of the suite's script, a build or a fetch that never
    reached a case included, and cargo-mutants records each as caught alike. Only the script's
    summary line tells them apart: a kill is a log whose summary lines all say `result=FAIL`, a
    complete run of the suite's cases with a case that failed or erred, or plain and instrumented
    observations that differ.
    """
    if not log_path:
        return "its outcome names no log"
    path = results / log_path
    if not path.is_file():
        return f"its log {log_path} is missing"
    lines = MEGA_EEST_SUMMARY.findall(ANSI.sub("", path.read_text(errors="replace")))
    if not lines:
        return "its log holds no Mega EEST summary line, so the suite never reached its cases"
    found = {m.group(1) if (m := re.search(r" result=(\w+)", line)) else "none" for line in lines}
    if found != {"FAIL"}:
        return (f"its log's Mega EEST summary says result={'/'.join(sorted(found))}, not FAIL: "
                f"the suite reached no verdict on it")
    counts = MEGA_EEST_MODE_COUNTS.findall(lines[-1])
    if not any(int(failed) + int(erred) for failed, erred in counts) and "-differ" not in lines[-1]:
        return "its log's Mega EEST summary shows no failing case"
    return None


def cmd_missed_re(args: argparse.Namespace) -> int:
    try:
        run = load_run(Path(args.results))
        recheck = None
        if run.timeout and args.recheck and Path(args.recheck).exists():
            recheck = load_run(Path(args.recheck))
    except ResultsError as err:
        # Nothing to run; the gate reports why the results cannot be scored.
        print(f"no missed mutants to run against Mega EEST: {err}", file=sys.stderr)
        return 0
    missed = list(run.missed)
    if recheck is not None:
        missed += [name for name in run.timeout if name in set(recheck.missed)]
    supp = suppressed_anchors(args.suppressions)
    print_selection([name for name in missed if not is_suppressed(name, supp)],
                    getattr(args, "with_files", False))
    return 0


def write_report(report: str, args: argparse.Namespace) -> None:
    if args.comment:
        Path(args.comment).write_text(report)
    if args.summary:
        with open(args.summary, "a") as fh:
            fh.write(report)
    print(report)


def cmd_report(args: argparse.Namespace) -> int:
    results = Path(args.results)
    full = getattr(args, "scope", "diff") == "full"
    title = "## 🧬 Mutation testing (full run)" if full else "## 🧬 Mutation testing"

    # The driver's status says whether the run tested every planned mutant. A
    # stopped run must never pass, even when everything it tested was caught; what
    # it did test is still reported, read without the completeness checks.
    note = None
    merged_diff = False
    status_path = getattr(args, "status", None)
    if status_path:
        run_status = read_status(Path(status_path))
        if run_status is None:
            print(f"ERROR: no MUTANTS-STATUS line in {status_path}: the mutation run did not "
                  f"finish — refusing to report a passing gate.", file=sys.stderr)
            return 2
        if run_status.get("status") != "complete":
            note = incomplete_note(run_status)
        merged_diff = not full and "shards" in run_status

    if merged_diff:
        failures = []
        for followup in (args.recheck, getattr(args, "mega_eest", None)):
            if followup and (failure := Path(followup) / RUN_FAILED_NOTE).is_file():
                failures.append(failure.read_text().strip())
        if failures:
            details = "\n".join(f"- {failure}" for failure in failures)
            incomplete = f"{note}\n\n" if note else ""
            write_report(f"{title} — ❌ FAIL\n\n{incomplete}"
                         f"**Shard follow-up results cannot be scored.**\n\n{details}\n", args)
            return 2

    try:
        anchors = suppressed_anchors(args.suppressions)
    except SuppressionError as err:
        print(f"ERROR: {err}", file=sys.stderr)
        return 2

    try:
        run = load_tested(results) if note else load_run(results)
        recheck = None
        if not note and run.timeout and args.recheck and Path(args.recheck).exists():
            recheck = load_run(Path(args.recheck))
    except ResultsError as err:
        write_report(
            f"{title} — ❌ FAIL\n\n"
            f"**The results cannot be scored:** {err}.\n",
            args,
        )
        return 2

    if run.nothing_to_test is not None and note is None:
        nothing = f"**Nothing to test**: {run.nothing_to_test}"
        write_report(f"{title} — ✅ PASS\n\n{nothing}.\n", args)
        return 0

    caught, missed, timeout = list(run.caught), list(run.missed), []
    rechecked: dict[str, list[str]] = {"caught": [], "missed": [], "timeout": [], "absent": []}
    if recheck is not None:
        verdicts = {n: k for k in ("caught", "missed", "timeout", "unviable") for n in getattr(recheck, k)}
        for name in run.timeout:
            verdict = verdicts.get(name)
            if verdict == "caught":
                caught.append(name)
                rechecked["caught"].append(name)
            elif verdict == "missed":
                missed.append(name)
                rechecked["missed"].append(name)
            else:
                timeout.append(name)
                rechecked["timeout" if verdict else "absent"].append(name)
    else:
        timeout = list(run.timeout)

    # A mutant the tests missed was run once more against the Mega EEST suite; one it catches is
    # caught, one it times out on is inconclusive, and the rest stay survivors. Its results speak
    # for those mutants alone, as a re-check's speak for the timeouts.
    mega_eest_dir = None if note else getattr(args, "mega_eest", None)
    against_eest: dict[str, list[str]] = {"caught": [], "missed": [], "timeout": [], "absent": []}
    eest = None
    if missed and mega_eest_dir and Path(mega_eest_dir).exists():
        try:
            eest = load_run(Path(mega_eest_dir))
        except ResultsError as err:
            write_report(
                f"{title} — ❌ FAIL\n\n"
                f"**The Mega EEST run of the mutants the tests missed cannot be scored:** {err}.\n",
                args,
            )
            return 2
    if eest is not None:
        verdicts = {n: k for k in ("caught", "missed", "timeout", "unviable") for n in getattr(eest, k)}
        # A kill counts only on the suite's own verdict in the mutant's log: a run that failed
        # before it reached the cases says nothing of the mutant, and fails the gate unscored.
        unproven = [(name, why) for name in missed if verdicts.get(name) == "caught"
                    if (why := mega_eest_kill_unproven(Path(mega_eest_dir), eest.logs.get(name)))]
        if unproven:
            listed = "\n".join(f"- `{name}`: {why}" for name, why in unproven[:MAX_SURVIVORS_SHOWN])
            write_report(
                f"{title} — ❌ FAIL\n\n"
                f"**The Mega EEST run of the mutants the tests missed cannot be scored:** "
                f"{len(unproven)} mutant(s) recorded as caught without the suite's verdict that "
                f"cases failed.\n\n{listed}\n",
                args,
            )
            return 2
        remaining = []
        for name in missed:
            verdict = verdicts.get(name)
            if verdict == "caught":
                caught.append(name)
                against_eest["caught"].append(name)
            elif verdict == "timeout":
                timeout.append(name)
                against_eest["timeout"].append(name)
            else:
                remaining.append(name)
                against_eest["missed" if verdict else "absent"].append(name)
        missed = remaining

    def partition(items: list[str]) -> tuple[list[str], list[str]]:
        sup, real = [], []
        for m in items:
            (sup if is_suppressed(m, anchors) else real).append(m)
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

    # No viable mutants and nothing inconclusive (every mutant unviable, or every
    # timeout suppressed). Reporting "100% (0/0)" reads like a real result and
    # confuses readers — say plainly that nothing was tested.
    if viable == 0 and not real_timeouts and note is None:
        nothing = "**Nothing to test** — no viable mutant"
        nothing += f" ({len(run.unviable)} unviable, {len(timeout)} timed out)"
        write_report(f"{title} — ✅ PASS\n\n{nothing}.\n", args)
        return 0

    label = "Full-run mutation score" if full else "Diff mutation score"
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
        f"- unviable: {len(run.unviable)} · timeout total: {len(timeout)}",
    ]
    if run.timeout:
        if recheck is None:
            md.append(f"- re-checked: none of the {len(run.timeout)} that timed out (no re-check results)")
        else:
            md.append(
                f"- re-checked alone after timing out: {len(run.timeout)} — "
                f"{len(rechecked['caught'])} caught, {len(rechecked['missed'])} survived, "
                f"{len(rechecked['timeout'])} timed out again, {len(rechecked['absent'])} not re-run"
            )
    ran_against_eest = sum(len(v) for v in against_eest.values())
    if eest is not None:
        md.append(
            f"- run against Mega EEST after the tests missed them: {ran_against_eest} — "
            f"{len(against_eest['caught'])} caught, {len(against_eest['missed'])} survived, "
            f"{len(against_eest['timeout'])} timed out, {len(against_eest['absent'])} not run"
        )
    elif mega_eest_dir and missed:
        md.append(f"- run against Mega EEST: none of the {len(missed)} the tests missed (no Mega EEST results)")
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
    elif note is None:
        md.append("Every viable mutant was caught or suppressed. 🎉" if full
                  else "No new test gaps introduced by this change. 🎉")
    write_report("\n".join(md) + "\n", args)

    return 0 if gate_pass else 1


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
    universe = [ANSI.sub("", m) for m in read_lines(Path(args.universe))]
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

    pt = sub.add_parser("timeout-re", help="--re arguments selecting the mutants that timed out")
    pt.add_argument("--results", required=True)
    pt.add_argument("--suppressions", default=None)
    pt.add_argument("--with-files", action="store_true",
                    help="also print one `-f <file>` pair per file of the selected mutants")
    pt.set_defaults(func=cmd_timeout_re)

    pm = sub.add_parser("missed-re", help="--re arguments selecting the mutants the tests missed")
    pm.add_argument("--results", required=True)
    pm.add_argument("--recheck", default=None)
    pm.add_argument("--suppressions", default=None)
    pm.add_argument("--with-files", action="store_true",
                    help="also print one `-f <file>` pair per file of the selected mutants")
    pm.set_defaults(func=cmd_missed_re)

    pp = sub.add_parser("progress", help="count planned and tested mutants")
    pp.add_argument("--results", required=True)
    pp.set_defaults(func=cmd_progress)

    pg = sub.add_parser("merge", help="combine sharded results against the full inventory")
    pg.add_argument("--inventory", required=True,
                    help="file of all mutant lines of the run (cargo mutants --list)")
    pg.add_argument("--out", required=True)
    pg.add_argument("--scope", choices=("diff", "full"), default="full",
                    help="allow an empty diff inventory and verify each diff shard's results")
    pg.add_argument("--head", help="tested SHA expected by the diff join")
    pg.add_argument("--base", help="base SHA expected by the diff join")
    pg.add_argument("--run-id", help="GitHub run ID expected by the diff join")
    pg.add_argument("--run-attempt", help="GitHub run attempt expected by the diff join")
    pg.add_argument("shards", nargs="*", help="one directory per shard: mutants.out/ + status")
    pg.set_defaults(func=cmd_merge)

    pf = sub.add_parser("merge-runs", help="combine the shards' re-check or Mega EEST runs")
    pf.add_argument("--out", required=True)
    pf.add_argument("--scope", choices=("diff", "full"), default="full",
                    help="for diff shards, keep only follow-up outcomes owned by each primary run")
    pf.add_argument("runs", nargs="*", help="one mutants.out directory per shard")
    pf.set_defaults(func=cmd_merge_runs)

    pr = sub.add_parser("report")
    pr.add_argument("--results", required=True)
    pr.add_argument("--recheck", default=None,
                    help="results of re-running the timed-out mutants one at a time")
    pr.add_argument("--mega-eest", default=None,
                    help="results of running the mutants the tests missed against Mega EEST")
    pr.add_argument("--suppressions", default=None)
    pr.add_argument("--status", default=None,
                    help="the driver's status file; fail unless it says complete")
    pr.add_argument("--scope", choices=("diff", "full"), default="diff",
                    help="what the run covered: a PR's changed lines, or every mutant")
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
