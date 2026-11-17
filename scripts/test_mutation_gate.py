#!/usr/bin/env python3
"""Tests of scripts/mutation_gate.py and of the guards of scripts/mutation_test.sh.

Run from the repository root with Python 3.11 or newer (the gate reads TOML with
tomllib):

    python3 -m unittest discover -s scripts -p 'test_*.py' -v

Each case writes the files a mutation run leaves in a temporary directory, in the
shapes cargo-mutants 27.1.0 and scripts/umutate.py write them, and runs the gate's
subcommands on it.
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent

# The gate reads TOML with tomllib, new in Python 3.11. An older Python runs one test in place of
# the rest, which fails with the reason: a failure, not a skip, so a job on an old Python fails
# rather than passing on nothing tested.
TOO_OLD = sys.version_info < (3, 11)
REASON = (f"the mutation gate and its tests need Python 3.11 or newer, for tomllib; this is "
          f"Python {sys.version.split()[0]}")


class PythonVersion(unittest.TestCase):
    def test_python_is_new_enough(self) -> None:
        if TOO_OLD:
            self.fail(REASON)


def load_tests(loader: unittest.TestLoader, tests: unittest.TestSuite,
               pattern: str | None) -> unittest.TestSuite:
    """On an older Python, the version test alone: the others need the gate, which cannot load."""
    return loader.loadTestsFromTestCase(PythonVersion) if TOO_OLD else tests


def load_script(name: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    # Registered before it runs: its dataclasses look their module up.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


gate = None if TOO_OLD else load_script("mutation_gate")
umutate = None if TOO_OLD else load_script("umutate")

CAUGHT = "crates/mega-evm/src/a.rs:1:1: replace f -> u64 with 0"
MISSED = "crates/mega-evm/src/a.rs:2:5: replace + with - in g"
TIMEOUT = "crates/mega-evm/src/system/keyless/dispatch.rs:367:23: delete ! in prepare"
UNVIABLE = "crates/mega-evm/src/a.rs:3:9: replace h -> Foo with Default::default()"


# When a run finished, as cargo-mutants writes `end_time`.
FINISHED = "2026-09-27T07:17:03.989998Z"


def mutant(name: str, summary: str, log_path: str | None = None) -> dict:
    outcome = {"scenario": {"Mutant": {"name": name}}, "summary": summary}
    if log_path is not None:
        outcome["log_path"] = log_path
    return outcome


# The summary lines scripts/mega_eest.sh prints: the suite's verdict that cases failed, and a run
# that never reached them.
EEST_FAIL = ("MEGA-EEST rev=" + "5e" * 20 + " run=partial cases=930 plain=pass:927,fail:3,unsupported:0,"
             "error:0 negative=fail-as-recorded provenance=unchecked caseset=unchecked result=FAIL")
EEST_INCOMPLETE = "MEGA-EEST rev=" + "5e" * 20 + " run=partial stage=adapter-build result=INCOMPLETE"


def eest_log(*lines: str) -> str:
    """A mutant's log as cargo-mutants keeps it: nextest's output of the failing mega_eest test."""
    return "\n".join(["    Starting 1 test across 1 binary", "        FAIL [  31.0s] mega-evm::mega_eest",
                       "--- STDOUT:              mega-evm::mega_eest ---", *lines, ""])




def outcomes_json(*mutants: dict, baseline: str | None = "Success",
                  end_time: str | None = FINISHED) -> dict:
    """An `outcomes.json` as cargo-mutants 27.1.0 writes it, after every outcome and once more
    when it finishes: its counters are the outcomes recorded so far, and `end_time` is null
    until the run finishes."""
    outcomes = [] if baseline is None else [{"scenario": "Baseline", "summary": baseline}]
    outcomes += list(mutants)
    summaries = [m["summary"] for m in mutants]
    return {
        "outcomes": outcomes,
        "total_mutants": len(mutants),
        "missed": summaries.count("MissedMutant"),
        "caught": summaries.count("CaughtMutant"),
        "timeout": summaries.count("Timeout"),
        "unviable": summaries.count("Unviable"),
        "success": summaries.count("Success"),
        "start_time": "2026-09-27T07:16:56.385727Z",
        "end_time": end_time,
        "cargo_mutants_version": "27.1.0",
    }


def mutants_json(*names: str) -> str:
    """A `mutants.json`: every mutant a run selected, written before it tests any."""
    return json.dumps([{"name": n, "file": n.split(":", 1)[0]} for n in names])


def line_entry(file: str, line: int | None, column: int | None, text: str) -> str:
    """One `kind = "line"` suppression; None leaves that anchor field out."""
    out = f'[[suppress]]\nkind = "line"\nfile = "{file}"\n'
    out += f"line = {line}\n" if line is not None else ""
    out += f"column = {column}\n" if column is not None else ""
    return out + f'mutant = "{text}"\n'


def anchored_entry(mutant_line: str) -> str:
    """The suppression anchored to a mutant line `file:line:col: text`."""
    file, line, col, text = re.match(r"^([^:]+):(\d+):(\d+): (.+)$", mutant_line).groups()
    return line_entry(file, int(line), int(col), text)


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="mutation-gate-test-"))
        self.addCleanup(shutil.rmtree, self.tmp)

    def results(self, name: str = "mutants.out", outcomes: dict | None = None,
                files: dict[str, str] | None = None, selected: list[str] | None = None) -> Path:
        """A results directory. With `outcomes` and no `mutants.json` of its own, it selects the
        mutants the outcomes name, as a run that finished does."""
        path = self.tmp / name
        path.mkdir(parents=True)
        files = files or {}
        if outcomes is not None:
            (path / "outcomes.json").write_text(json.dumps(outcomes))
            if selected is None and "mutants.json" not in files:
                selected = [o["scenario"]["Mutant"]["name"] for o in outcomes["outcomes"]
                            if isinstance(o.get("scenario"), dict)]
        if selected is not None:
            (path / "mutants.json").write_text(mutants_json(*selected))
        for file, text in files.items():
            (path / file).parent.mkdir(parents=True, exist_ok=True)
            (path / file).write_text(text)
        return path

    def eest_results(self, name: str, *outcomes: tuple[str, str, str | None]) -> Path:
        """A Mega EEST run's results: each (mutant, summary, log text or None for no log)."""
        files, recorded = {}, []
        for i, (mutant_name, summary, log) in enumerate(outcomes):
            if log is None:
                recorded.append(mutant(mutant_name, summary))
            else:
                files[f"log/mutant-{i}.log"] = log
                recorded.append(mutant(mutant_name, summary, f"log/mutant-{i}.log"))
        return self.results(name, outcomes=outcomes_json(*recorded), files=files)

    def suppressions(self, *mutants: str) -> Path:
        """Line suppressions, each anchored to the mutant line given (`file:line:col: text`)."""
        path = self.tmp / "suppressions.toml"
        path.write_text("\n".join(anchored_entry(m) for m in mutants))
        return path

    def report(self, results: Path, recheck: Path | None = None,
               suppressions: Path | None = None,
               mega_eest: Path | None = None, status: Path | None = None,
               scope: str = "diff") -> tuple[int, str]:
        comment = self.tmp / "comment.md"
        comment.unlink(missing_ok=True)
        args = Namespace(
            results=str(results),
            recheck=str(recheck) if recheck else None,
            mega_eest=str(mega_eest) if mega_eest else None,
            status=str(status) if status else None,
            scope=scope,
            suppressions=str(suppressions) if suppressions else None,
            comment=str(comment),
            summary=None,
        )
        with contextlib.redirect_stdout(io.StringIO()):
            code = gate.cmd_report(args)
        text = comment.read_text() if comment.exists() else ""
        return code, text


class ResultsThatCannotBeScored(GateCase):
    """Every way a run can leave results that say nothing is a failure, with its reason."""

    def assert_unscorable(self, results: Path, reason: str) -> None:
        code, text = self.report(results)
        self.assertEqual(code, 2, text)
        self.assertIn("FAIL", text)
        self.assertIn("cannot be scored", text)
        self.assertRegex(text, reason)

    def test_no_results_directory(self) -> None:
        self.assert_unscorable(self.tmp / "absent", "no results at")

    def test_a_directory_without_outcomes(self) -> None:
        # What a driver that crashed after cargo-mutants created its directory leaves,
        # and what the empty lists of an aborted run look like.
        results = self.results(files={"caught.txt": "", "missed.txt": "", "timeout.txt": ""})
        self.assert_unscorable(results, "no outcomes.json")

    def test_a_failed_baseline(self) -> None:
        results = self.results(outcomes=outcomes_json(baseline="Failure"))
        self.assert_unscorable(results, "baseline failed")

    def test_a_failed_baseline_with_an_exit_status(self) -> None:
        results = self.results(outcomes=outcomes_json(baseline={"Failure": 101}))
        self.assert_unscorable(results, "baseline failed")

    def test_no_baseline(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant"), baseline=None))
        self.assert_unscorable(results, "no baseline outcome")

    def test_a_run_stopped_after_its_baseline(self) -> None:
        # What cargo-mutants 27.1.0 leaves when it stops once its baseline passed and before it
        # records a mutant: the selection in mutants.json, and an outcomes.json holding the
        # baseline alone, counting no mutant, with no end_time.
        record = outcomes_json(end_time=None)
        self.assertEqual(record["total_mutants"], 0)
        results = self.results(outcomes=record, selected=[CAUGHT, MISSED])
        self.assert_unscorable(results, "records no end_time")

    def test_a_run_stopped_after_catching_part_of_its_selection(self) -> None:
        # The same record once some of the selection was caught: total_mutants counts the
        # outcomes recorded, so it equals them and says nothing of the mutants still to test.
        record = outcomes_json(mutant(CAUGHT, "CaughtMutant"), end_time=None)
        self.assertEqual(record["total_mutants"], 1)
        results = self.results(outcomes=record, selected=[CAUGHT, MISSED])
        self.assert_unscorable(results, "records no end_time")

    def test_a_record_without_an_end_time(self) -> None:
        record = outcomes_json(mutant(CAUGHT, "CaughtMutant"))
        del record["end_time"]
        self.assert_unscorable(self.results(outcomes=record), "records no end_time")

    def test_a_finished_record_missing_a_selected_mutant(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")),
                               selected=[CAUGHT, MISSED])
        self.assert_unscorable(results, "1 of the 2 mutants mutants.json selects have no outcome")

    def test_a_mutant_selected_twice_needs_two_outcomes(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")),
                               selected=[CAUGHT, CAUGHT])
        self.assert_unscorable(results, "1 of the 2 mutants mutants.json selects have no outcome")

    def test_an_outcome_of_a_mutant_not_selected(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "CaughtMutant")), selected=[CAUGHT])
        self.assert_unscorable(results, "outcomes of mutants mutants.json does not select")

    def test_outcomes_without_a_selection(self) -> None:
        results = self.results(files={"outcomes.json": json.dumps(
            outcomes_json(mutant(CAUGHT, "CaughtMutant")))})
        self.assert_unscorable(results, "no mutants.json")

    def test_a_selection_that_is_not_a_list_of_mutants(self) -> None:
        results = self.results(outcomes=outcomes_json(), files={"mutants.json": json.dumps([CAUGHT])})
        self.assert_unscorable(results, "is not a list of mutants")

    def test_an_outcome_of_an_unknown_kind(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "Exploded")))
        self.assert_unscorable(results, "none this gate knows")

    def test_outcomes_that_are_not_json(self) -> None:
        results = self.results(files={"outcomes.json": "{"})
        self.assert_unscorable(results, "is not a mutation run's outcomes")

    def test_outcomes_that_are_not_objects(self) -> None:
        results = self.results(files={"outcomes.json": json.dumps(
            {"total_mutants": 1, "outcomes": ["Baseline", CAUGHT]})})
        self.assert_unscorable(results, "not a list of objects")

    def test_a_listed_run_without_outcomes(self) -> None:
        results = self.results(files={"mutants.json": json.dumps([{"name": CAUGHT}])})
        self.assert_unscorable(results, "stopped before it recorded an outcome")

    def test_a_note_that_is_not_cargo_mutants_own(self) -> None:
        results = self.results(files={"no-mutants.txt": "nothing to see here\n"})
        self.assert_unscorable(results, "does not hold a line cargo-mutants prints")

    def test_a_broken_recheck_of_a_timeout(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(TIMEOUT, "Timeout")))
        recheck = self.results("recheck", outcomes=outcomes_json(baseline="Failure"))
        code, text = self.report(results, recheck)
        self.assertEqual(code, 2, text)
        self.assertIn("baseline failed", text)

    def test_a_recheck_that_exited_abnormally(self) -> None:
        # Its files record the timeout caught and the run finished; the driver's note of its exit
        # outweighs them.
        results = self.results(outcomes=outcomes_json(mutant(TIMEOUT, "Timeout")))
        recheck = self.results("recheck", outcomes=outcomes_json(mutant(TIMEOUT, "CaughtMutant")),
                               files={"run-failed.txt": "cargo-mutants exited 70\n"})
        code, text = self.report(results, recheck)
        self.assertEqual(code, 2, text)
        self.assertIn("exited abnormally (cargo-mutants exited 70)", text)


class NothingToTest(GateCase):
    """A run that tested nothing passes only on the producer's own word for it."""

    def assert_nothing_to_test(self, results: Path, evidence: str) -> None:
        code, text = self.report(results)
        self.assertEqual(code, 0, text)
        self.assertIn("PASS", text)
        self.assertIn("Nothing to test", text)
        self.assertIn(evidence, text)

    def test_cargo_mutants_listed_no_mutant(self) -> None:
        # What cargo-mutants writes when its filters leave nothing: mutants.json holding
        # [] and no outcomes; umutate.py writes the same when it generates nothing.
        results = self.results(files={"mutants.json": "[]\n", "caught.txt": "", "missed.txt": ""})
        self.assert_nothing_to_test(results, "mutant list under the run's filters was empty")

    def test_cargo_mutants_said_so_for_a_diff(self) -> None:
        results = self.results(files={"no-mutants.txt": "INFO No mutants to filter\n"})
        self.assert_nothing_to_test(results, "No mutants to filter")

    def test_cargo_mutants_said_so_for_an_empty_diff(self) -> None:
        results = self.results(files={"no-mutants.txt": "INFO Diff file is empty\n"})
        self.assert_nothing_to_test(results, "Diff file is empty")

    def test_cargo_mutants_said_so_for_a_diff_of_no_rust_source(self) -> None:
        results = self.results(files={"no-mutants.txt": "INFO Diff changes no Rust source files\n"})
        self.assert_nothing_to_test(results, "Diff changes no Rust source files")

    def test_every_mutant_unviable(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(UNVIABLE, "Unviable")))
        code, text = self.report(results)
        self.assertEqual(code, 0, text)
        self.assertIn("Nothing to test", text)
        self.assertIn("1 unviable", text)


class Scoring(GateCase):
    """What survives or times out fails the gate unless suppressed or re-checked."""

    def test_all_caught_passes(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable")))
        code, text = self.report(results)
        self.assertEqual(code, 0, text)
        self.assertIn("100.0%", text)

    def test_a_survivor_fails(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant")))
        code, text = self.report(results)
        self.assertEqual(code, 1, text)
        self.assertIn(MISSED, text)

    def test_a_suppressed_survivor_passes(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant")))
        code, text = self.report(results, suppressions=self.suppressions(MISSED))
        self.assertEqual(code, 0, text)
        self.assertIn("suppressed (equivalent/dead-code): 1", text)

    def test_an_anchored_suppression_leaves_the_same_text_elsewhere_gated(self) -> None:
        # A new `+` added to a function with a suppressed `+` mutant: same text, another
        # column. Only the reviewed mutant is suppressed.
        added = f"crates/mega-evm/src/a.rs:7:30: {gate.mutant_body(MISSED)}"
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant"), mutant(added, "MissedMutant")))
        code, text = self.report(results, suppressions=self.suppressions(MISSED))
        self.assertEqual(code, 1, text)
        self.assertIn("survived (real gaps): **1**", text)
        self.assertIn(f"`{added}`", text)
        self.assertNotIn(f"`{MISSED}`", text)

    def test_a_moved_mutant_is_gated_again(self) -> None:
        moved = f"crates/mega-evm/src/a.rs:3:5: {gate.mutant_body(MISSED)}"
        results = self.results(outcomes=outcomes_json(mutant(moved, "MissedMutant")))
        code, text = self.report(results, suppressions=self.suppressions(MISSED))
        self.assertEqual(code, 1, text)
        self.assertIn(f"`{moved}`", text)

    def test_an_anchor_needs_the_same_text_and_file(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        for entry in (line_entry("crates/mega-evm/src/a.rs", 2, 5, "replace + with * in g"),
                      line_entry("crates/mega-evm/src/b.rs", 2, 5, gate.mutant_body(MISSED))):
            path = self.tmp / "suppressions.toml"
            path.write_text(entry)
            code, text = self.report(results, suppressions=path)
            self.assertEqual(code, 1, (entry, text))

    def test_unanchored_line_suppressions_are_refused(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        a_rs, body = "crates/mega-evm/src/a.rs", gate.mutant_body(MISSED)
        for entry, why in ((line_entry(a_rs, None, None, body), "`line` and `column`"),
                           (line_entry(a_rs, 2, None, body), "`line` and `column`"),
                           (line_entry(a_rs, 0, 5, body), "`line` and `column`"),
                           (line_entry(a_rs, 2, 5, MISSED), "`file:line:col:` prefix")):
            path = self.tmp / "suppressions.toml"
            path.write_text(entry)
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                code, _ = self.report(results, suppressions=path)
            self.assertEqual(code, 2, entry)
            self.assertIn(why, err.getvalue())

    def test_a_timeout_without_a_recheck_fails(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(TIMEOUT, "Timeout")))
        code, text = self.report(results, self.tmp / "no-recheck")
        self.assertEqual(code, 1, text)
        self.assertIn("none of the 1 that timed out", text)

    def test_a_suppressed_timeout_passes(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(TIMEOUT, "Timeout")))
        code, text = self.report(results, suppressions=self.suppressions(TIMEOUT))
        self.assertEqual(code, 0, text)

    def recheck_of_the_timeout(self, summary: str) -> tuple[int, str]:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(TIMEOUT, "Timeout")))
        recheck = self.results("recheck", outcomes=outcomes_json(mutant(TIMEOUT, summary)))
        return self.report(results, recheck)

    def test_a_timeout_caught_alone_passes(self) -> None:
        code, text = self.recheck_of_the_timeout("CaughtMutant")
        self.assertEqual(code, 0, text)
        self.assertIn("1 caught, 0 survived, 0 timed out again", text)
        self.assertIn("(2/2 viable mutants killed)", text)

    def test_a_timeout_that_survives_alone_is_a_survivor(self) -> None:
        code, text = self.recheck_of_the_timeout("MissedMutant")
        self.assertEqual(code, 1, text)
        self.assertIn("Survivors needing attention", text)
        self.assertNotIn("Timed-out mutants", text)

    def test_a_timeout_that_times_out_alone_stays_inconclusive(self) -> None:
        code, text = self.recheck_of_the_timeout("Timeout")
        self.assertEqual(code, 1, text)
        self.assertIn("1 timed out again", text)
        self.assertIn("Timed-out mutants", text)

    def test_a_timeout_the_recheck_did_not_run_stays_inconclusive(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(TIMEOUT, "Timeout")))
        recheck = self.results("recheck", outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")))
        code, text = self.report(results, recheck)
        self.assertEqual(code, 1, text)
        self.assertIn("1 not re-run", text)

    def test_a_recheck_only_speaks_for_the_timeouts(self) -> None:
        # A mutant the first run decided is not re-decided by a re-check that happens to
        # include it.
        results = self.results(outcomes=outcomes_json(
            mutant(MISSED, "MissedMutant"), mutant(TIMEOUT, "Timeout")))
        recheck = self.results("recheck", outcomes=outcomes_json(
            mutant(MISSED, "CaughtMutant"), mutant(TIMEOUT, "CaughtMutant")))
        code, text = self.report(results, recheck)
        self.assertEqual(code, 1, text)
        self.assertIn(MISSED, text)


class MegaEestScoring(GateCase):
    """A mutant the tests missed is decided by its Mega EEST run, and only such a mutant."""

    def against_mega_eest(self, summary: str, log: str | None = None, *,
                          suppressions: Path | None = None) -> tuple[int, str]:
        # Each call its own pair of results, as a test may score several.
        self.calls = getattr(self, "calls", 0) + 1
        results = self.results(f"run-{self.calls}/mutants.out", outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant")))
        if log is None and summary == "CaughtMutant":
            log = eest_log(EEST_FAIL, f"Mega EEST cases failed on this checkout: {EEST_FAIL}")
        eest = self.eest_results(f"run-{self.calls}/mega-eest", (MISSED, summary, log))
        return self.report(results, suppressions=suppressions, mega_eest=eest)

    def test_a_missed_mutant_mega_eest_catches_passes(self) -> None:
        code, text = self.against_mega_eest("CaughtMutant")
        self.assertEqual(code, 0, text)
        self.assertIn("run against Mega EEST after the tests missed them: 1 — 1 caught, 0 survived", text)
        self.assertIn("(2/2 viable mutants killed)", text)

    def test_a_kill_needs_the_suites_verdict_in_the_mutants_log(self) -> None:
        # Coloured as CI's CARGO_TERM_COLOR=always leaves it, the verdict still counts.
        code, text = self.against_mega_eest("CaughtMutant", eest_log("\x1b[1m" + EEST_FAIL + "\x1b[0m"))
        self.assertEqual(code, 0, text)
        for log, reason in (
                (eest_log("error: could not compile `mega-evm`", EEST_INCOMPLETE,
                          f"the Mega EEST run reached no verdict (exit status: 2): {EEST_INCOMPLETE}"),
                 "result=INCOMPLETE, not FAIL"),
                (eest_log("thread panicked: cannot run scripts/mega_eest.sh"), "no Mega EEST summary line"),
                (eest_log(EEST_FAIL.replace("fail:3", "fail:0")), "shows no failing case"),
                (eest_log(EEST_FAIL, EEST_INCOMPLETE), "result=FAIL/INCOMPLETE"),
        ):
            code, text = self.against_mega_eest("CaughtMutant", log)
            self.assertEqual(code, 2, text)
            self.assertIn("cannot be scored", text)
            self.assertIn("recorded as caught without the suite's verdict", text)
            self.assertIn(reason, text)

    def test_a_kill_that_moved_the_control_is_credited(self) -> None:
        # The mutant failed gating cases and moved where the wrong expectation first differs.
        moved = EEST_FAIL.replace("negative=fail-as-recorded", "negative=fail-elsewhere")
        code, text = self.against_mega_eest("CaughtMutant", eest_log(moved))
        self.assertEqual(code, 0, text)
        self.assertIn("1 caught, 0 survived", text)

    def test_a_kill_without_a_log_cannot_be_scored(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        for eest in (self.eest_results("no-log-path", (MISSED, "CaughtMutant", None)),
                     self.results("log-missing", outcomes=outcomes_json(
                         mutant(MISSED, "CaughtMutant", "log/gone.log")))):
            code, text = self.report(results, mega_eest=eest)
            self.assertEqual(code, 2, text)
            self.assertRegex(text, "names no log|is missing")

    def test_a_missed_mutant_mega_eest_misses_is_a_survivor(self) -> None:
        code, text = self.against_mega_eest("MissedMutant")
        self.assertEqual(code, 1, text)
        self.assertIn("Survivors needing attention", text)
        self.assertIn(MISSED, text)

    def test_a_missed_mutant_mega_eest_times_out_on_is_inconclusive(self) -> None:
        code, text = self.against_mega_eest("Timeout")
        self.assertEqual(code, 1, text)
        self.assertIn("Timed-out mutants", text)
        self.assertNotIn("Survivors needing attention", text)

    def test_a_missed_mutant_without_mega_eest_results_is_a_survivor(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        code, text = self.report(results, mega_eest=self.tmp / "absent")
        self.assertEqual(code, 1, text)
        self.assertIn("none of the 1 the tests missed (no Mega EEST results)", text)

    def test_a_missed_mutant_mega_eest_did_not_run_is_a_survivor(self) -> None:
        other = "crates/mega-evm/src/b.rs:9:9: replace * with + in k"
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        eest = self.results("mega-eest", outcomes=outcomes_json(mutant(other, "CaughtMutant")))
        code, text = self.report(results, mega_eest=eest)
        self.assertEqual(code, 1, text)
        self.assertIn("0 caught, 0 survived, 0 timed out, 1 not run", text)

    def test_mega_eest_results_only_speak_for_the_missed(self) -> None:
        # A timeout is the re-check's to decide, whatever a Mega EEST run says of it.
        results = self.results(outcomes=outcomes_json(
            mutant(MISSED, "MissedMutant"), mutant(TIMEOUT, "Timeout")))
        verdict = eest_log(EEST_FAIL)
        eest = self.eest_results("mega-eest", (MISSED, "CaughtMutant", verdict),
                                 (TIMEOUT, "CaughtMutant", verdict))
        code, text = self.report(results, self.tmp / "no-recheck", mega_eest=eest)
        self.assertEqual(code, 1, text)
        self.assertIn("Timed-out mutants", text)
        self.assertIn(TIMEOUT, text)

    def test_a_timeout_that_survives_alone_is_decided_by_mega_eest(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(TIMEOUT, "Timeout")))
        recheck = self.results("recheck", outcomes=outcomes_json(mutant(TIMEOUT, "MissedMutant")))
        eest = self.eest_results("mega-eest", (TIMEOUT, "CaughtMutant", eest_log(EEST_FAIL)))
        code, text = self.report(results, recheck, mega_eest=eest)
        self.assertEqual(code, 0, text)

    def test_mega_eest_results_that_cannot_be_scored_fail(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        for name, eest in [
            ("failed-baseline", outcomes_json(mutant(MISSED, "CaughtMutant"), baseline="Failure")),
            ("unfinished", outcomes_json(mutant(MISSED, "CaughtMutant"), end_time=None)),
        ]:
            code, text = self.report(results, mega_eest=self.results(name, outcomes=eest))
            self.assertEqual(code, 2, text)
            self.assertIn("Mega EEST run of the mutants the tests missed cannot be scored", text)
        failed = self.results("run-failed", outcomes=outcomes_json(mutant(MISSED, "CaughtMutant")),
                              files={"run-failed.txt": "cargo-mutants exited 70\n"})
        code, text = self.report(results, mega_eest=failed)
        self.assertEqual(code, 2, text)
        self.assertIn("exited abnormally (cargo-mutants exited 70)", text)

    def test_a_suppressed_survivor_needs_no_mega_eest_run(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        code, text = self.report(results, suppressions=self.suppressions(MISSED),
                                 mega_eest=self.tmp / "absent")
        self.assertEqual(code, 0, text)


class MissedRe(GateCase):
    """`missed-re` selects exactly the unsuppressed mutants the tests missed."""

    def regexes(self, results: Path, recheck: Path | None = None,
                suppressions: Path | None = None) -> list[str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = gate.cmd_missed_re(Namespace(
                results=str(results), recheck=str(recheck) if recheck else None,
                suppressions=str(suppressions) if suppressions else None))
        self.assertEqual(code, 0)
        lines = out.getvalue().splitlines()
        self.assertEqual(lines[0::2], ["--re"] * (len(lines) // 2))
        return lines[1::2]

    def test_one_anchored_regex_per_missed_mutant(self) -> None:
        other = "crates/mega-evm/src/a.rs:2:5: replace + with - in g2"
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant"),
            mutant(other, "MissedMutant"), mutant(TIMEOUT, "Timeout")))
        regexes = self.regexes(results)
        names = [CAUGHT, MISSED, other, TIMEOUT]
        self.assertEqual([[n for n in names if re.search(r, n)] for r in regexes], [[MISSED], [other]])

    def test_a_timeout_the_recheck_missed_is_selected(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(TIMEOUT, "Timeout")))
        for summary, selected in [("MissedMutant", [TIMEOUT]), ("CaughtMutant", []), ("Timeout", [])]:
            recheck = self.results(f"recheck-{summary}", outcomes=outcomes_json(mutant(TIMEOUT, summary)))
            regexes = self.regexes(results, recheck)
            self.assertEqual([n for n in [TIMEOUT] if any(re.search(r, n) for r in regexes)], selected)

    def test_suppressed_survivors_are_not_selected(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant")))
        self.assertEqual(self.regexes(results, suppressions=self.suppressions(MISSED)), [])

    def test_results_that_cannot_be_scored_select_nothing(self) -> None:
        self.assertEqual(self.regexes(self.tmp / "absent"), [])
        unfinished = self.results(outcomes=outcomes_json(mutant(MISSED, "MissedMutant"), end_time=None))
        self.assertEqual(self.regexes(unfinished), [])


class UmutateRecord(GateCase):
    """scripts/umutate.py writes the record the gate scores, on the contract cargo-mutants keeps."""

    def test_a_finished_run_is_scored(self) -> None:
        out = self.results("umutate")
        umutate.write_selection(out, [CAUGHT, MISSED])
        umutate.write_outcomes(out, umutate.now(), [CAUGHT], [MISSED], [])
        code, text = self.report(out)
        self.assertEqual(code, 1, text)
        self.assertIn("(1/2 viable mutants killed)", text)
        self.assertIn(MISSED, text)

    def test_a_run_that_stops_part_way_is_refused(self) -> None:
        # The selection is on disk before any mutant is tested, the outcomes only once all are.
        out = self.results("umutate")
        umutate.write_selection(out, [CAUGHT, MISSED])
        code, text = self.report(out)
        self.assertEqual(code, 2, text)
        self.assertIn("stopped before it recorded an outcome", text)

    def test_a_run_that_generates_nothing_has_nothing_to_test(self) -> None:
        out = self.results("umutate")
        umutate.write_selection(out, [])
        code, text = self.report(out)
        self.assertEqual(code, 0, text)
        self.assertIn("mutant list under the run's filters was empty", text)


class NoteEmpty(GateCase):
    """`note-empty` records cargo-mutants saying it had nothing to mutate, and nothing else."""

    def note(self, log_text: str | None, results: Path) -> int:
        log = self.tmp / "run.log"
        if log_text is not None:
            log.write_text(log_text)
        with contextlib.redirect_stderr(io.StringIO()):
            return gate.cmd_note_empty(Namespace(log=str(log), results=str(results)))

    def test_it_records_what_cargo_mutants_printed(self) -> None:
        results = self.tmp / "mutants.out"
        # As CI prints it, with CARGO_TERM_COLOR=always.
        log = "Found 0 mutants\n\x1b[32m INFO\x1b[0m No mutants to filter\n"
        self.assertEqual(self.note(log, results), 0)
        self.assertEqual((results / "no-mutants.txt").read_text(), "INFO No mutants to filter\n")
        code, text = self.report(results)
        self.assertEqual(code, 0, text)

    def test_it_records_a_diff_of_no_rust_source(self) -> None:
        # What cargo-mutants 27.1.0 prints, and all it does, for a diff that changes only
        # Markdown files: it writes no results and exits 0.
        results = self.tmp / "mutants.out"
        log = "\x1b[32m INFO\x1b[0m Diff changes no Rust source files\n"
        self.assertEqual(self.note(log, results), 0)
        self.assertEqual((results / "no-mutants.txt").read_text(),
                         "INFO Diff changes no Rust source files\n")
        code, text = self.report(results)
        self.assertEqual(code, 0, text)

    def test_it_refuses_output_that_says_no_such_thing(self) -> None:
        results = self.tmp / "mutants.out"
        self.assertEqual(self.note("error: could not compile\n", results), 1)
        self.assertFalse(results.exists())
        self.assertEqual(self.note(None, results), 1)
        self.assertFalse(results.exists())

    def test_it_leaves_results_cargo_mutants_wrote_alone(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")))
        self.assertEqual(self.note(" INFO Diff file is empty\n", results), 0)
        self.assertFalse((results / "no-mutants.txt").exists())


class TimeoutRe(GateCase):
    """`timeout-re` selects exactly the unsuppressed mutants that timed out."""

    def regexes(self, results: Path, suppressions: Path | None = None) -> list[str]:
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = gate.cmd_timeout_re(Namespace(
                results=str(results), suppressions=str(suppressions) if suppressions else None))
        self.assertEqual(code, 0)
        lines = out.getvalue().splitlines()
        self.assertEqual(lines[0::2], ["--re"] * (len(lines) // 2))
        return lines[1::2]

    def test_one_anchored_regex_per_timeout(self) -> None:
        other = "crates/mega-evm/src/system/keyless/dispatch.rs:367:23: delete ! in prepare2"
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(TIMEOUT, "Timeout"), mutant(other, "Timeout")))
        regexes = self.regexes(results)
        self.assertEqual(len(regexes), 2)
        names = [CAUGHT, TIMEOUT, other]
        for regex, timed_out in zip(regexes, [TIMEOUT, other]):
            self.assertEqual([n for n in names if re.search(regex, n)], [timed_out])

    def test_suppressed_timeouts_are_not_rerun(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(TIMEOUT, "Timeout")))
        self.assertEqual(self.regexes(results, self.suppressions(TIMEOUT)), [])

    def test_results_without_outcomes_select_nothing(self) -> None:
        self.assertEqual(self.regexes(self.results(files={"mutants.json": "[]"})), [])
        self.assertEqual(self.regexes(self.tmp / "absent"), [])


class DiffScope(unittest.TestCase):
    """The diff the driver hands cargo-mutants holds every Rust source of the crate and nothing else."""

    ROOT = SCRIPTS.parent
    SRC = "crates/mega-evm/src"

    def ls_files(self, *pathspecs: str) -> list[str]:
        run = subprocess.run(["git", "ls-files", "--", *pathspecs], cwd=self.ROOT,
                             capture_output=True, text=True, timeout=60)
        if run.returncode != 0:
            self.skipTest(f"not a git checkout: {run.stderr.strip()}")
        return run.stdout.splitlines()

    def test_the_pathspec_selects_every_rust_source_and_nothing_else(self) -> None:
        driver = (SCRIPTS / "mutation_test.sh").read_text()
        match = re.search(r"^SRC_PATHSPEC='([^']+)'$", driver, re.MULTILINE)
        self.assertIsNotNone(match, "the driver names its pathspec")
        tracked = self.ls_files(self.SRC)
        rust = [path for path in tracked if path.endswith(".rs")]
        self.assertIn(f"{self.SRC}/lib.rs", rust)
        self.assertTrue(any(not path.endswith(".rs") for path in tracked),
                        "src/ holds files that are not Rust sources, which the scope leaves out")
        self.assertEqual(self.ls_files(match.group(1)), rust)


def bash5() -> str | None:
    """A bash 5 or newer, if this machine has one."""
    for shell in (shutil.which("bash"), "/opt/homebrew/bin/bash", "/usr/local/bin/bash",
                  "/usr/bin/bash", "/bin/bash"):
        if shell and Path(shell).exists() and (DriverGuard.bash_major(shell) or 0) >= 5:
            return shell
    return None


# A stand-in for `cargo mutants`: it writes the results the test prepared for the run it is asked
# for, by its output directory — the first run, the re-check or the Mega EEST run — records the
# arguments it was given in <run>.argv, and exits as the test says.
FAKE_CARGO = """#!/bin/sh
if [ "$1" = nextest ]; then echo "$*" > "$FAKE_RUNS/warm.argv"; exit "${FAKE_WARM_EXIT:-0}"; fi
[ "$1" = mutants ] || exit 99
shift
printf '%s\\0' "$@" > "$FAKE_RUNS/last.argv0"
if [ "$1" = --version ]; then echo "cargo-mutants 27.1.0"; exit 0; fi
argv="$*"; out=""; run=main
while [ $# -gt 0 ]; do
    case "$1" in
        --output) out="$2"; shift ;;
    esac
    shift
done
case "$(basename "$out")" in
    recheck) run=recheck ;;
    mega-eest) run=mega-eest ;;
esac
echo "$argv" > "$FAKE_RUNS/$run.argv"
mv "$FAKE_RUNS/last.argv0" "$FAKE_RUNS/$run.argv0"
echo "dir=${MEGA_EEST_DIR-unset} modes=${MEGA_EEST_MODES-unset} work=${MEGA_EEST_WORK-unset}" \\
    "provenance=${MEGA_EEST_SKIP_PROVENANCE-checked} base=${MEGA_EEST_BASE_REV-unset}" > "$FAKE_RUNS/$run.env"
if [ -d "$FAKE_RUNS/$run" ]; then
    mkdir -p "$out/mutants.out"
    cp -R "$FAKE_RUNS/$run/." "$out/mutants.out/"
    if [ -n "${FAKE_TRUNCATE:-}" ] && [ "$run" = main ]; then
        printf '{"outcomes": [' > "$out/mutants.out/outcomes.json"
    fi
fi
exit "$(cat "$FAKE_RUNS/$run.exit")"
"""


class DriverRecheck(GateCase):
    """scripts/mutation_test.sh re-runs a timed-out mutant alone and carries a failed re-check's
    exit into its results, which the gate then refuses."""

    def drive(self, recheck_exit: int, recheck: dict | None) -> tuple[int, str, Path]:
        shell = bash5()
        if shell is None:
            self.skipTest("no bash 5 or newer on this machine")
        runs = self.tmp / "runs"
        self.results("runs/main", outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(TIMEOUT, "Timeout")))
        (runs / "main.exit").write_text("3\n")
        if recheck is not None:
            self.results("runs/recheck", outcomes=recheck)
        (runs / "recheck.exit").write_text(f"{recheck_exit}\n")
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "cargo").write_text(FAKE_CARGO)
        (bin_dir / "cargo").chmod(0o755)
        (bin_dir / "python3").symlink_to(sys.executable)
        out = self.tmp / "out" / "mutants"
        env = {"PATH": f"{bin_dir}:/usr/bin:/bin", "FAKE_RUNS": str(runs), "OUT_DIR": str(out),
               "JOBS": "1", "SUPPRESS": str(self.tmp / "no-suppressions.toml")}
        run = subprocess.run([shell, str(DriverGuard.DRIVER), "file", "crates/mega-evm/src/a.rs"],
                             capture_output=True, text=True, timeout=120, env=env)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("Re-checking 1 timed-out mutant(s) one at a time", run.stderr)
        self.assertIn("-f crates/mega-evm/src/a.rs", (runs / "recheck.argv").read_text())
        code, text = self.report(out / "mutants.out", out / "recheck" / "mutants.out")
        return code, text, out / "recheck" / "mutants.out"

    def test_a_recheck_that_finishes_decides_the_timeout(self) -> None:
        code, text, recheck = self.drive(0, outcomes_json(mutant(TIMEOUT, "CaughtMutant")))
        self.assertFalse((recheck / "run-failed.txt").exists())
        self.assertEqual(code, 0, text)
        self.assertIn("1 caught, 0 survived, 0 timed out again", text)

    def test_a_recheck_that_exits_abnormally_is_refused(self) -> None:
        # Its files are those of a finished run that caught the timeout: only its exit says
        # otherwise.
        code, text, recheck = self.drive(70, outcomes_json(mutant(TIMEOUT, "CaughtMutant")))
        self.assertEqual((recheck / "run-failed.txt").read_text(), "cargo-mutants exited 70\n")
        self.assertEqual(code, 2, text)
        self.assertIn("exited abnormally (cargo-mutants exited 70)", text)

    def test_a_recheck_that_writes_nothing_is_refused(self) -> None:
        code, text, recheck = self.drive(1, None)
        self.assertEqual(sorted(p.name for p in recheck.iterdir()), ["run-failed.txt"])
        self.assertEqual(code, 2, text)
        self.assertIn("exited abnormally (cargo-mutants exited 1)", text)


class DriverMegaEest(GateCase):
    """scripts/mutation_test.sh runs a mutant the tests missed against Mega EEST, with mega-evm's
    `mega_eest` test alone, after the suite passed on the unmutated tree, and carries a failed
    warm-up or run into the results, which the gate then refuses."""

    def drive(self, *, warmup_exit: int = 0, eest_exit: int = 0,
              eest: list[tuple[str, str, str | None]] | None = None,
              env: dict[str, str] | None = None) -> tuple[int, str, Path, Path]:
        shell = bash5()
        if shell is None:
            self.skipTest("no bash 5 or newer on this machine")
        runs = self.tmp / "runs"
        self.results("runs/main", outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant")))
        (runs / "main.exit").write_text("2\n")
        if eest is not None:
            self.eest_results("runs/mega-eest", *eest)
        (runs / "mega-eest.exit").write_text(f"{eest_exit}\n")
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        for name, text in (("cargo", FAKE_CARGO), ("timeout", FAKE_TIMEOUT)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        (bin_dir / "python3").symlink_to(sys.executable)
        # The warm-up's stand-in records the modes it was asked for and exits as the test says.
        stub = self.tmp / "mega_eest.sh"
        stub.write_text(f'#!/bin/sh\necho "$MEGA_EEST_MODES" > "{runs}/warmup.modes"\nexit {warmup_exit}\n')
        stub.chmod(0o755)
        out = self.tmp / "out" / "mutants"
        run = subprocess.run(
            [shell, str(DriverGuard.DRIVER), "file", "crates/mega-evm/src/a.rs"],
            capture_output=True, text=True, timeout=120,
            env={"PATH": f"{bin_dir}:/usr/bin:/bin", "FAKE_RUNS": str(runs), "OUT_DIR": str(out),
                 "JOBS": "1", "SUPPRESS": str(self.tmp / "no-suppressions.toml"),
                 "MEGA_EEST_SCRIPT": str(stub), **(env or {})})
        self.assertEqual(run.returncode, 0, run.stderr)
        results = out / "mega-eest" / "mutants.out"
        code, text = self.report(out / "mutants.out", out / "recheck" / "mutants.out",
                                 mega_eest=results)
        return code, text, runs, results

    def test_a_missed_mutant_runs_against_mega_eest_alone(self) -> None:
        work = self.tmp / "eest-work"
        code, text, runs, _ = self.drive(eest=[(MISSED, "CaughtMutant", eest_log(EEST_FAIL))],
                                         env={"MEGA_EEST_WORK": str(work), "MEGA_EEST_BASE_REV": "b" * 40})
        self.assertEqual(code, 0, text)
        self.assertEqual((runs / "warmup.modes").read_text(), "plain\n")
        # Every build directory reads the warm-up's checkout and works in its own directory; the
        # warm-up checked the pin, so the runs do not, and no case set is compared.
        self.assertEqual((runs / "mega-eest.env").read_text(),
                         f"dir={work}/src modes=plain work=unset provenance=1 base=unset\n")
        # One mutant at a time, each with the machine's parallelism for its cases.
        self.assertIn("--jobs 1 ", (runs / "mega-eest.argv").read_text())
        argv = (runs / "mega-eest.argv").read_text().strip()
        self.assertIn("--cargo-arg=--test --cargo-arg=mega_eest", argv)
        # The first run's scope, which keeps cargo-mutants' unfiltered struct-field mutants to it.
        self.assertIn("-f crates/mega-evm/src/a.rs", argv)
        self.assertIn(f"--re ^{re.escape(MISSED)}$", argv)
        self.assertNotIn(re.escape(CAUGHT), argv)
        self.assertTrue(argv.endswith("-- --run-ignored only"), argv)

    def test_a_nested_build_failure_is_not_a_kill(self) -> None:
        # The baseline passed; the mutant's own run of the suite failed to build the adapter, which
        # fails the mega_eest test as a failing case would, and cargo-mutants records it caught.
        log = eest_log("error: could not compile `mega-evm` (lib)", EEST_INCOMPLETE,
                       f"the Mega EEST run reached no verdict (exit status: 2): {EEST_INCOMPLETE}")
        code, text, _, _ = self.drive(eest=[(MISSED, "CaughtMutant", log)])
        self.assertEqual(code, 2, text)
        self.assertIn("recorded as caught without the suite's verdict", text)
        self.assertIn(MISSED, text)

    def test_a_warmup_that_fails_is_refused(self) -> None:
        code, text, runs, results = self.drive(
            warmup_exit=1, eest=[(MISSED, "CaughtMutant", eest_log(EEST_FAIL))])
        self.assertFalse((runs / "mega-eest.argv").exists())
        self.assertEqual((results / "run-failed.txt").read_text(),
                         "scripts/mega_eest.sh exited 1 on the unmutated tree\n")
        self.assertEqual(code, 2, text)

    def test_a_mega_eest_run_that_exits_abnormally_is_refused(self) -> None:
        # Its files are those of a finished run that caught the mutant: only its exit says otherwise.
        code, text, _, results = self.drive(
            eest_exit=70, eest=[(MISSED, "CaughtMutant", eest_log(EEST_FAIL))])
        self.assertEqual((results / "run-failed.txt").read_text(), "cargo-mutants exited 70\n")
        self.assertEqual(code, 2, text)
        self.assertIn("exited abnormally (cargo-mutants exited 70)", text)

    def test_a_pass_the_budget_stops_is_recorded_and_refused(self) -> None:
        code, text, runs, results = self.drive(
            eest=[(MISSED, "CaughtMutant", eest_log(EEST_FAIL))],
            env={"MUTANTS_BUDGET": "3600", "FAKE_STOP": "/mega-eest"})
        self.assertEqual((results / "run-failed.txt").read_text(), "stopped by MUTANTS_BUDGET (3600s)\n")
        self.assertEqual(code, 2, text)
        out = results.parent.parent
        self.assertEqual((out / "status").read_text().strip(),
                         "MUTANTS-STATUS status=followup-stopped expected=2 outcomes=2")
        code, text = self.report(out / "mutants.out", out / "recheck" / "mutants.out",
                                 mega_eest=results, status=out / "status")
        self.assertEqual(code, 1, text)
        self.assertIn("Mega EEST run of a missed one", text)

    def test_the_pass_can_be_left_out(self) -> None:
        code, text, runs, _ = self.drive(eest=[(MISSED, "CaughtMutant", eest_log(EEST_FAIL))],
                                         env={"MUTANTS_MEGA_EEST": "0"})
        self.assertFalse((runs / "mega-eest.argv").exists())
        self.assertFalse((runs / "warmup.modes").exists())
        self.assertEqual(code, 1, text)
        self.assertIn(MISSED, text)


# A stand-in for GNU timeout: it runs the command, or answers 124 (stopped) for one whose
# arguments contain $FAKE_STOP.
FAKE_TIMEOUT = """#!/bin/sh
shift 3
case "$*" in *"${FAKE_STOP:-no such argument}"*) exit 124 ;; esac
exec "$@"
"""


class DriverRun(GateCase):
    """scripts/mutation_test.sh builds the tests first, bounds its phases by MUTANTS_BUDGET and
    ends with a MUTANTS-STATUS line, as main's driver does, and keeps this branch's re-check."""

    def drive(self, *args: str, main: dict | None = None, recheck: dict | None = None,
              env: dict[str, str] | None = None) -> tuple[subprocess.CompletedProcess, Path, Path]:
        shell = bash5()
        if shell is None:
            self.skipTest("no bash 5 or newer on this machine")
        runs = self.tmp / "runs"
        runs.mkdir(exist_ok=True)
        if main is not None:
            self.results("runs/main", outcomes=main)
        (runs / "main.exit").write_text("0\n")
        if recheck is not None:
            self.results("runs/recheck", outcomes=recheck)
        (runs / "recheck.exit").write_text("0\n")
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir(exist_ok=True)
        for name, text in (("cargo", FAKE_CARGO), ("timeout", FAKE_TIMEOUT)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        if not (bin_dir / "python3").exists():
            (bin_dir / "python3").symlink_to(sys.executable)
        out = self.tmp / "out" / "mutants"
        run = subprocess.run(
            [shell, str(DriverGuard.DRIVER), *args], capture_output=True, text=True, timeout=120,
            env={"PATH": f"{bin_dir}:/usr/bin:/bin", "FAKE_RUNS": str(runs), "OUT_DIR": str(out),
                 "JOBS": "1", "SUPPRESS": str(self.tmp / "no-suppressions.toml"),
                 "MUTANTS_MEGA_EEST": "0", **(env or {})})
        return run, runs, out

    def test_a_finished_run_builds_first_and_reports_complete(self) -> None:
        run, runs, out = self.drive("file", "crates/mega-evm/src/a.rs", main=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable")))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual((runs / "warm.argv").read_text().strip(),
                         "nextest run --package mega-evm --all-features --no-run")
        self.assertEqual((out / "status").read_text().strip(),
                         "MUTANTS-STATUS status=complete expected=2 outcomes=2")
        self.assertTrue(run.stdout.strip().endswith("MUTANTS-STATUS status=complete expected=2 outcomes=2"))

    def test_a_record_that_cannot_be_counted_is_an_error(self) -> None:
        runs_main = outcomes_json(mutant(CAUGHT, "CaughtMutant"))
        run, runs, out = self.drive("file", "crates/mega-evm/src/a.rs", main=runs_main,
                                    env={"FAKE_TRUNCATE": "1"})
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertEqual((out / "status").read_text().strip(),
                         "MUTANTS-STATUS status=error expected=unknown outcomes=0")

    def test_a_warm_build_that_fails_is_a_failed_baseline(self) -> None:
        run, runs, out = self.drive("file", "crates/mega-evm/src/a.rs", main=outcomes_json(
            mutant(CAUGHT, "CaughtMutant")), env={"FAKE_WARM_EXIT": "101"})
        self.assertEqual(run.returncode, 101, run.stderr)
        self.assertFalse((runs / "main.argv").exists())
        self.assertIn("status=baseline-failed", (out / "status").read_text())

    def test_a_spent_budget_stops_before_the_build(self) -> None:
        run, runs, out = self.drive("file", "crates/mega-evm/src/a.rs", main=outcomes_json(
            mutant(CAUGHT, "CaughtMutant")), env={"MUTANTS_BUDGET": "0"})
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertFalse((runs / "warm.argv").exists())
        self.assertEqual((out / "status").read_text().strip(),
                         "MUTANTS-STATUS status=not-started expected=unknown outcomes=0")
        self.assertIn("ran out", run.stdout)

    def test_a_re_check_the_budget_stops_is_recorded_and_refused(self) -> None:
        run, _, out = self.drive(
            "file", "crates/mega-evm/src/a.rs",
            main=outcomes_json(mutant(CAUGHT, "CaughtMutant"), mutant(TIMEOUT, "Timeout")),
            recheck=outcomes_json(mutant(TIMEOUT, "CaughtMutant")),
            env={"MUTANTS_BUDGET": "3600", "FAKE_STOP": "/recheck"})
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual((out / "status").read_text().strip(),
                         "MUTANTS-STATUS status=followup-stopped expected=2 outcomes=2")
        self.assertEqual((out / "recheck" / "mutants.out" / "run-failed.txt").read_text(),
                         "stopped by MUTANTS_BUDGET (3600s)\n")
        code, text = self.report(out / "mutants.out", out / "recheck" / "mutants.out",
                                 status=out / "status")
        self.assertEqual(code, 1, text)
        self.assertIn("re-check of a timed-out mutant", text)

    def test_list_names_the_mutant_set_without_running(self) -> None:
        supp = self.tmp / "no-suppressions.toml"
        supp.write_text('[[suppress]]\nkind = "function"\nfile = "a"\npattern = "in f$"\n')
        run, runs, out = self.drive("list")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual((runs / "main.argv").read_text().strip(),
                         "--list --colors never --package mega-evm --exclude-re in f$")
        run, runs, _ = self.drive("list", "--all")
        self.assertEqual((runs / "main.argv").read_text().strip(),
                         "--list --colors never --package mega-evm")
        self.assertFalse((runs / "warm.argv").exists())


class ShardDriver(GateCase):
    """Runs the driver's sharded full run with the fake cargo, recording each run's arguments."""

    def drive_shard(self, *shard: str, timed_out: str = TIMEOUT, missed: str = MISSED,
                    env: dict[str, str] | None = None) -> tuple[subprocess.CompletedProcess, Path]:
        shell = bash5()
        if shell is None:
            self.skipTest("no bash 5 or newer on this machine")
        runs = self.tmp / "runs"
        runs.mkdir(exist_ok=True)
        # The first run: one mutant timed out, one missed; the re-check misses the timed-out one,
        # so the Mega EEST pass is asked for both; it catches both, with the suite's verdict.
        self.results("runs/main", outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(timed_out, "Timeout"), mutant(missed, "MissedMutant")))
        self.results("runs/recheck", outcomes=outcomes_json(mutant(timed_out, "MissedMutant")))
        self.eest_results("runs/mega-eest", (missed, "CaughtMutant", eest_log(EEST_FAIL)),
                          (timed_out, "CaughtMutant", eest_log(EEST_FAIL)))
        for name in ("main", "recheck", "mega-eest"):
            (runs / f"{name}.exit").write_text("0\n")
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir(exist_ok=True)
        for name, text in (("cargo", FAKE_CARGO), ("timeout", FAKE_TIMEOUT)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        if not (bin_dir / "python3").exists():
            (bin_dir / "python3").symlink_to(sys.executable)
        stub = self.tmp / "mega_eest.sh"
        stub.write_text("#!/bin/sh\nexit 0\n")
        stub.chmod(0o755)
        out = self.tmp / "out" / "mutants"
        run = subprocess.run(
            [shell, str(DriverGuard.DRIVER), "full", *shard], capture_output=True, text=True, timeout=120,
            env={"PATH": f"{bin_dir}:/usr/bin:/bin", "FAKE_RUNS": str(runs), "OUT_DIR": str(out),
                 "JOBS": "1", "SUPPRESS": str(self.tmp / "no-suppressions.toml"),
                 "MEGA_EEST_SCRIPT": str(stub), **(env or {})})
        self.assertEqual(run.returncode, 0, run.stderr)
        return run, runs

    @staticmethod
    def argv(runs: Path, name: str) -> list[str]:
        return (runs / f"{name}.argv0").read_text().split("\0")[:-1]


class DriverShardedFollowUps(ShardDriver):
    """In a sharded full run the follow-up runs keep their mutants: they are not cut by the shard."""

    def test_the_follow_ups_of_a_shard_are_not_sharded_again(self) -> None:
        for shard in (["--shard", "1/4"], ["--shard=1/4"]):
            with self.subTest(shard=shard):
                _, runs = self.drive_shard(*shard)
                main = self.argv(runs, "main")
                self.assertIn(shard[-1].removeprefix("--shard="), " ".join(main))
                recheck, eest = self.argv(runs, "recheck"), self.argv(runs, "mega-eest")
                for argv in (recheck, eest):
                    self.assertFalse([a for a in argv if a.startswith("--shard")], argv)
                    self.assertNotIn("1/4", argv)
                # Each follow-up names its mutants, and their files keep the struct-field extras
                # cargo-mutants adds to those files.
                self.assertIn(f"^{re.escape(TIMEOUT)}$", recheck)
                self.assertEqual(recheck[recheck.index("-f") + 1], TIMEOUT.split(":", 1)[0])
                self.assertIn(f"^{re.escape(MISSED)}$", eest)
                self.assertIn(f"^{re.escape(TIMEOUT)}$", eest)
                self.assertEqual(sorted(eest[i + 1] for i, a in enumerate(eest) if a == "-f"),
                                 sorted({MISSED.split(":", 1)[0], TIMEOUT.split(":", 1)[0]}))
                shutil.rmtree(self.tmp / "runs")
                shutil.rmtree(self.tmp / "out")

    def test_a_restricted_run_keeps_its_own_restriction(self) -> None:
        _, runs = self.drive_shard("-f", "crates/mega-evm/src/a.rs", "--shard", "2/4")
        for name in ("recheck", "mega-eest"):
            argv = self.argv(runs, name)
            self.assertEqual([argv[i + 1] for i, a in enumerate(argv) if a == "-f"], ["crates/mega-evm/src/a.rs"])
            self.assertFalse([a for a in argv if a.startswith("--shard")], argv)


@unittest.skipUnless(os.environ.get("MUTANTS_REAL_LIST") == "1",
                     "set MUTANTS_REAL_LIST=1 to list the crate's mutants with the installed cargo-mutants")
class FollowUpSelectionAgainstCargoMutants(ShardDriver):
    """The follow-up arguments the driver builds, replayed through the installed cargo-mutants'
    real `--list` on this tree: for every shard of the full run, each mutant a follow-up is asked
    to decide is in the list it selects."""

    ROOT = SCRIPTS.parent

    def real_list(self, args: list[str]) -> list[str]:
        run = subprocess.run(["cargo", "mutants", "--list", "--colors", "never", *args], cwd=self.ROOT,
                             capture_output=True, text=True, timeout=600)
        self.assertEqual(run.returncode, 0, run.stderr)
        return [line for line in run.stdout.splitlines() if line.strip()]

    @staticmethod
    def selection(argv: list[str]) -> list[str]:
        """The arguments of a follow-up run that select mutants, as `--list` takes them."""
        out, i = [], 0
        while i < len(argv):
            a = argv[i]
            if a == "--":
                break
            if a in ("--output", "--jobs"):
                i += 2
                continue
            if a in ("-vV", "--no-shuffle") or a.startswith("--cargo-arg"):
                i += 1
                continue
            out.append(a)
            i += 1
        return out

    def test_every_requested_mutant_is_selected_in_every_shard(self) -> None:
        shell = bash5()
        if shell is None:
            self.skipTest("no bash 5 or newer on this machine")
        for k in range(4):
            with self.subTest(shard=k):
                listed = subprocess.run([shell, str(DriverGuard.DRIVER), "list", "--shard", f"{k}/4"],
                                        cwd=self.ROOT, capture_output=True, text=True, timeout=600)
                self.assertEqual(listed.returncode, 0, listed.stderr)
                names = [line for line in listed.stdout.splitlines() if line.strip()]
                timed_out, missed = names[0], names[-1]
                _, runs = self.drive_shard("--shard", f"{k}/4", timed_out=timed_out, missed=missed)
                recheck = self.real_list(self.selection(self.argv(runs, "recheck")))
                eest = self.real_list(self.selection(self.argv(runs, "mega-eest")))
                self.assertIn(timed_out, recheck)
                self.assertIn(missed, eest)
                self.assertIn(timed_out, eest)
                # The shard, added back, is what would cut them.
                cut = self.real_list([*self.selection(self.argv(runs, "mega-eest")), "--shard", f"{k}/4"])
                print(f"shard {k}/4: re-check lists {len(recheck)}, Mega EEST pass lists {len(eest)}, "
                      f"both requested in each; with the shard again: "
                      f"{sum(n in cut for n in (timed_out, missed))} of 2 kept", file=sys.stderr)
                shutil.rmtree(self.tmp / "runs")
                shutil.rmtree(self.tmp / "out")


class DriverGuard(unittest.TestCase):
    """scripts/mutation_test.sh refuses to run on a shell that is not bash 5 or newer."""

    DRIVER = SCRIPTS / "mutation_test.sh"

    @staticmethod
    def bash_major(shell: str) -> int | None:
        """The bash major version `shell` is, or None when it is not bash."""
        out = subprocess.run([shell, "-c", 'echo "${BASH_VERSION:-}"'], capture_output=True,
                             text=True).stdout.strip()
        return int(out.split(".")[0]) if out else None

    def assert_refused(self, shell: str) -> None:
        major = self.bash_major(shell)
        if major is not None and major >= 5:
            self.skipTest(f"{shell} is bash {major}")
        # A subcommand the driver does not have and a scratch output directory: were the guard to
        # let the shell through, the driver would print its usage and touch nothing else.
        with tempfile.TemporaryDirectory() as out:
            run = subprocess.run([shell, str(self.DRIVER), "guard-check"], capture_output=True,
                                 text=True, timeout=60, env={"PATH": "/usr/bin:/bin",
                                                             "OUT_DIR": f"{out}/mutants"})
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("needs bash 5 or newer", run.stderr)
        self.assertEqual(run.stdout, "")

    def test_a_posix_shell_is_refused(self) -> None:
        # dash on Debian and Ubuntu, bash 3.2 in POSIX mode on macOS.
        self.assert_refused("/bin/sh")

    def test_an_old_bash_is_refused(self) -> None:
        if not Path("/bin/bash").exists():
            self.skipTest("no /bin/bash")
        self.assert_refused("/bin/bash")


class OrphanSuppressions(GateCase):
    """A suppression must name a live mutant: a line entry at its anchor."""

    def orphans(self, entries: str, universe: list[str]) -> tuple[int, str]:
        path = self.tmp / "universe.txt"
        path.write_text("".join(f"{line}\n" for line in universe))
        supp = self.tmp / "suppressions.toml"
        supp.write_text(entries)
        args = Namespace(suppressions=str(supp), universe=str(path))
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = gate.cmd_orphans(args)
            except gate.SuppressionError:
                code = 2
        return code, buf.getvalue()

    BODY = "replace + with - in g"
    A_RS = "crates/mega-evm/src/a.rs"
    UNIVERSE = [CAUGHT, f"crates/mega-evm/src/a.rs:3:1: replace + with - in g", UNVIABLE]

    def test_an_entry_at_its_anchor_is_live(self) -> None:
        code, text = self.orphans(line_entry(self.A_RS, 3, 1, self.BODY), self.UNIVERSE)
        self.assertEqual(code, 0, text)

    def test_a_moved_entry_is_reported_with_where_its_text_went(self) -> None:
        code, text = self.orphans(line_entry(self.A_RS, 2, 1, self.BODY), self.UNIVERSE)
        self.assertEqual(code, 1, text)
        self.assertIn(f"{self.A_RS}:2:1", text)
        self.assertIn("the text is now at 3:1", text)

    def test_an_anchor_whose_text_changed_is_reported(self) -> None:
        code, text = self.orphans(line_entry(self.A_RS, 3, 1, "replace + with * in g"), self.UNIVERSE)
        self.assertEqual(code, 1, text)
        self.assertNotIn("now at", text)

    def test_an_unanchored_entry_is_refused(self) -> None:
        code, _ = self.orphans(line_entry(self.A_RS, None, None, self.BODY), self.UNIVERSE)
        self.assertEqual(code, 2)

    def test_a_function_pattern_must_match_a_mutant(self) -> None:
        live = '[[suppress]]\nkind = "function"\nfile = "a"\npattern = "in g$"\n'
        dead = '[[suppress]]\nkind = "function"\nfile = "a"\npattern = "in nowhere$"\n'
        self.assertEqual(self.orphans(live, self.UNIVERSE)[0], 0)
        code, text = self.orphans(dead, self.UNIVERSE)
        self.assertEqual(code, 1, text)
        self.assertIn("[function]", text)


class ExcludeRe(GateCase):
    """Function suppressions become one `--exclude-re` pair each."""

    def test_one_pair_per_function_suppression(self) -> None:
        path = self.tmp / "suppressions.toml"
        path.write_text('[[suppress]]\nkind = "function"\npattern = "foo"\n\n'
                        + anchored_entry(MISSED))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.cmd_exclude_re(Namespace(suppressions=str(path)))
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue().splitlines(), ["--exclude-re", "foo"])


class Progress(GateCase):
    """`progress` counts what a run planned and what has an outcome so far."""

    def progress(self, results: Path) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            gate.cmd_progress(Namespace(results=str(results)))
        return out.getvalue().strip()

    def test_before_any_output(self) -> None:
        self.assertEqual(self.progress(self.tmp / "absent"), "expected=unknown outcomes=0")

    def test_counts_the_outcomes_the_record_holds(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant"), end_time=None),
            selected=[CAUGHT, MISSED, UNVIABLE])
        self.assertEqual(self.progress(results), "expected=3 outcomes=2")

    def test_a_record_cut_off_cannot_be_counted(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")))
        (results / "outcomes.json").write_text('{"outcomes": [{"scen')
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            code = gate.cmd_progress(Namespace(results=str(results)))
        self.assertEqual(code, 2)
        self.assertIn("cannot count", err.getvalue())

    def test_counts_the_plan_and_the_outcome_lists(self) -> None:
        results = self.results(selected=[CAUGHT, MISSED, UNVIABLE, TIMEOUT, "x"], files={
            "caught.txt": f"{CAUGHT}\n", "missed.txt": f"{MISSED}\n", "unviable.txt": f"{UNVIABLE}\n"})
        self.assertEqual(self.progress(results), "expected=5 outcomes=3")


def status_file(path: Path, line: str) -> Path:
    path.write_text(f"MUTANTS-STATUS {line}\n")
    return path


class Status(GateCase):
    """With the driver's status, a run that did not test every planned mutant never passes."""

    def test_a_complete_run_passes(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable")))
        code, text = self.report(results, status=status_file(
            self.tmp / "status", "status=complete expected=2 outcomes=2"))
        self.assertEqual(code, 0, text)
        self.assertNotIn("Incomplete", text)

    def test_a_complete_run_still_gates_survivors_and_completeness(self) -> None:
        status = status_file(self.tmp / "status", "status=complete expected=2 outcomes=2")
        survivor = self.results("survivor", outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "MissedMutant")))
        self.assertEqual(self.report(survivor, status=status)[0], 1)
        # The status says complete, the record says the run stopped: the record is refused.
        stopped = self.results("stopped", outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), end_time=None))
        self.assertEqual(self.report(stopped, status=status)[0], 2)

    def test_a_partial_run_fails_even_when_all_it_tested_was_caught(self) -> None:
        # The budget stopped cargo-mutants: no end_time, a mutant of the plan untested.
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"), end_time=None),
            selected=[CAUGHT, UNVIABLE, MISSED])
        code, text = self.report(results, status=status_file(
            self.tmp / "status", "status=partial expected=3 outcomes=2"))
        self.assertEqual(code, 1, text)
        self.assertIn("Incomplete run", text)
        self.assertIn("2 of 3", text)
        self.assertIn("caught: 1", text)  # what it tested is still reported

    def test_a_run_not_started_fails(self) -> None:
        code, text = self.report(self.tmp / "absent", status=status_file(
            self.tmp / "status", "status=not-started expected=unknown outcomes=0"))
        self.assertEqual(code, 1, text)
        self.assertIn("before any mutant was tested", text)
        self.assertNotIn("PASS", text)

    def test_a_follow_up_the_budget_stopped_fails(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")))
        code, text = self.report(results, status=status_file(
            self.tmp / "status", "status=followup-stopped expected=1 outcomes=1"))
        self.assertEqual(code, 1, text)
        self.assertIn("re-check of a timed-out mutant or the Mega EEST run", text)

    def test_a_missing_status_line_is_refused(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant")))
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code, _ = self.report(results, status=self.tmp / "no-status")
        self.assertEqual(code, 2)
        self.assertIn("MUTANTS-STATUS", err.getvalue())

    def test_an_empty_diff_with_the_producers_word_passes(self) -> None:
        results = self.results(files={gate.NO_MUTANTS_NOTE: "INFO Diff file is empty\n"})
        code, text = self.report(results, status=status_file(
            self.tmp / "status", "status=complete expected=0 outcomes=0"))
        self.assertEqual(code, 0, text)

    def test_a_full_run_is_titled_as_one(self) -> None:
        results = self.results(outcomes=outcomes_json(
            mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable")))
        code, text = self.report(results, scope="full")
        self.assertEqual(code, 0, text)
        self.assertIn("## 🧬 Mutation testing (full run) — ✅ PASS", text)
        self.assertIn("**Full-run mutation score: 100.0%** (1/1", text)
        self.assertNotIn("this change", text)
        self.assertIn("**Diff mutation score: 100.0%**", self.report(results)[1])


class Merge(GateCase):
    """Sharded results merge into one record, counted against the inventory."""

    def shard(self, name: str, status: str | None, *outcomes: dict, finished: bool = True) -> Path:
        """One shard's artifact: its mutants.out (the run's record) and status line."""
        shard = self.tmp / name
        if outcomes:
            self.results(f"{name}/mutants.out", outcomes=outcomes_json(
                *outcomes, end_time=FINISHED if finished else None))
        else:
            shard.mkdir(parents=True)
        if status is not None:
            status_file(shard / "status", status)
        return shard

    def merge(self, *shards: Path) -> str:
        inventory = self.tmp / "inventory.txt"
        # `cargo mutants --list` output, with the colour codes CI can leave in it.
        inventory.write_text(f"\x1b[33m{CAUGHT}\x1b[0m\n{MISSED}\n{UNVIABLE}\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate.cmd_merge(Namespace(inventory=str(inventory), out=str(self.tmp / "merged"),
                                            shards=[str(s) for s in shards]))
        self.assertEqual(code, 0)
        return out.getvalue()

    def merged(self) -> tuple[int, str]:
        merged = self.tmp / "merged"
        return self.report(merged / "mutants.out", status=merged / "status", scope="full")

    def status(self) -> str:
        return (self.tmp / "merged" / "status").read_text().strip()

    def test_every_shard_complete_is_a_complete_run_scored_strictly(self) -> None:
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"))
        b = self.shard("b", "status=complete expected=1 outcomes=1", mutant(MISSED, "CaughtMutant"))
        self.merge(a, b)
        self.assertEqual(self.status(), "MUTANTS-STATUS status=complete expected=3 outcomes=3 "
                                        "unplanned=0 duplicated=0 shards-planned=3 shards=complete,complete")
        merged = self.tmp / "merged" / "mutants.out"
        self.assertEqual((merged / "caught.txt").read_text().splitlines(), [CAUGHT, MISSED])
        # The merged record keeps the contract: the strict reading accepts it.
        self.assertEqual(sorted(gate.load_run(merged).caught), sorted([CAUGHT, MISSED]))
        code, text = self.merged()
        self.assertEqual(code, 0, text)

    def test_a_shard_without_an_artifact_is_named(self) -> None:
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"))
        self.merge(a, self.tmp / "never-uploaded")
        self.assertIn("status=error expected=3 outcomes=2 ", self.status())
        self.assertIn("shards=complete,no-artifact", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("shard 1: `no-artifact` — it uploaded no results", text)
        self.assertNotIn("time budget", text)

    def test_a_baseline_failed_shard_is_named_not_blamed_on_the_budget(self) -> None:
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"))
        b = self.shard("b", "status=baseline-failed expected=unknown outcomes=0")
        self.merge(a, b)
        self.assertIn("status=baseline-failed expected=3 outcomes=2 ", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("shard 1: `baseline-failed` — the unmutated tree failed", text)

    def test_shard_failures_outrank_a_budget_stop(self) -> None:
        a = self.shard("a", "status=error expected=2 outcomes=1", mutant(CAUGHT, "CaughtMutant"))
        b = self.shard("b", "status=partial expected=1 outcomes=0")
        c = self.shard("c", None)
        self.merge(a, b, c)
        self.assertIn("status=error ", self.status())
        self.assertIn("shards=error,partial,no-status", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("shard 0: `error` — the run itself failed", text)
        self.assertIn("shard 1: `partial` — its time budget ran out", text)
        self.assertIn("shard 2: `no-status` — it ended without a status line", text)

    def test_a_budget_stopped_shard_blames_the_budget(self) -> None:
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"))
        b = self.shard("b", "status=not-started expected=1 outcomes=0")
        self.merge(a, b)
        self.assertIn("status=partial expected=3 outcomes=2 ", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("the time budget ran out after 2 of 3", text)

    def test_no_shards_is_not_started(self) -> None:
        self.merge()
        self.assertIn("status=not-started expected=3 outcomes=0", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("no shard results were found", text)

    def test_an_unplanned_survivor_is_kept_and_fails(self) -> None:
        unplanned = "crates/mega-evm/src/z.rs:9:9: replace z -> bool with true"
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"))
        b = self.shard("b", "status=complete expected=2 outcomes=2",
                       mutant(MISSED, "CaughtMutant"), mutant(unplanned, "MissedMutant"))
        self.assertIn(unplanned, self.merge(a, b))
        self.assertIn("status=mismatch expected=3 outcomes=3 unplanned=1 duplicated=0", self.status())
        merged = self.tmp / "merged"
        self.assertEqual((merged / "mutants.out" / "missed.txt").read_text(), f"{unplanned}\n")
        self.assertEqual((merged / "unplanned.txt").read_text(), f"{unplanned}\n")
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("1 shard outcome(s) are for mutants the inventory does not list", text)
        self.assertIn(f"`{unplanned}`", text)

    def test_a_mutant_tested_twice_is_a_mismatch(self) -> None:
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(MISSED, "CaughtMutant"))
        b = self.shard("b", "status=complete expected=2 outcomes=2",
                       mutant(MISSED, "MissedMutant"), mutant(UNVIABLE, "Unviable"))
        self.merge(a, b)
        self.assertIn("status=mismatch expected=3 outcomes=3 unplanned=0 duplicated=1", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("1 mutant(s) have more than one outcome", text)

    def test_shard_plans_that_miss_inventory_mutants_are_a_mismatch(self) -> None:
        a = self.shard("a", "status=complete expected=1 outcomes=1", mutant(CAUGHT, "CaughtMutant"))
        b = self.shard("b", "status=complete expected=1 outcomes=1", mutant(MISSED, "MissedMutant"))
        self.merge(a, b)
        self.assertIn("status=mismatch expected=3 outcomes=2 unplanned=0 duplicated=0 shards-planned=2",
                      self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("the shards planned 2 mutants", text)

    def test_a_shard_whose_record_is_cut_off_is_named_and_the_run_is_an_error(self) -> None:
        a = self.shard("a", "status=complete expected=2 outcomes=2",
                       mutant(CAUGHT, "CaughtMutant"), mutant(UNVIABLE, "Unviable"))
        b = self.shard("b", "status=complete expected=1 outcomes=1", mutant(MISSED, "CaughtMutant"))
        record = b / "mutants.out" / "outcomes.json"
        record.write_text(record.read_text()[:40])  # killed while cargo-mutants rewrote it
        printed = self.merge(a, b)
        self.assertIn("unreadable", printed)
        self.assertIn("status=error ", self.status())
        self.assertIn("shards=complete,unreadable", self.status())
        code, text = self.merged()
        self.assertEqual(code, 1, text)
        self.assertIn("shard 1: `unreadable` — its results cannot be read", text)
        self.assertIn("caught: 1", text)  # the readable shard is still reported

    def test_an_empty_inventory_is_refused(self) -> None:
        (self.tmp / "inventory.txt").write_text("")
        with contextlib.redirect_stderr(io.StringIO()):
            code = gate.cmd_merge(Namespace(inventory=str(self.tmp / "inventory.txt"),
                                            out=str(self.tmp / "merged"), shards=[]))
        self.assertEqual(code, 2)


class MergeRuns(GateCase):
    """The shards' follow-up runs merge into one record the report reads."""

    def merge_runs(self, *runs: Path) -> Path:
        out = self.tmp / "merged-followups"
        with contextlib.redirect_stderr(io.StringIO()):
            gate.cmd_merge_runs(Namespace(out=str(out), runs=[str(r) for r in runs]))
        return out

    def test_finished_runs_merge_with_their_logs(self) -> None:
        def logged(name: str, summary: str) -> dict:
            return {**mutant(name, summary), "log_path": "log/m.log"}
        a = self.results("a", outcomes=outcomes_json(logged(MISSED, "CaughtMutant")),
                         files={"log/m.log": "the verdict\n"})
        b = self.results("b", outcomes=outcomes_json(logged(TIMEOUT, "MissedMutant")),
                         files={"log/m.log": "another\n"})
        out = self.merge_runs(a, b, self.tmp / "absent")
        run = gate.load_run(out)
        self.assertEqual((run.caught, run.missed), ([MISSED], [TIMEOUT]))
        logs = {gate.outcome_name(o): o["log_path"]
                for o in json.loads((out / "outcomes.json").read_text())["outcomes"] if gate.outcome_name(o)}
        self.assertEqual((out / logs[MISSED]).read_text(), "the verdict\n")
        self.assertEqual((out / logs[TIMEOUT]).read_text(), "another\n")

    def test_an_unfinished_or_failed_run_makes_the_merge_unscorable(self) -> None:
        a = self.results("a", outcomes=outcomes_json(mutant(MISSED, "CaughtMutant")))
        b = self.results("b", outcomes=outcomes_json(mutant(TIMEOUT, "CaughtMutant"), end_time=None))
        with self.assertRaises(gate.ResultsError):
            gate.load_run(self.merge_runs(a, b))
        c = self.results("c", outcomes=outcomes_json(mutant(TIMEOUT, "CaughtMutant")),
                         files={gate.RUN_FAILED_NOTE: "cargo-mutants exited 70\n"})
        with self.assertRaisesRegex(gate.ResultsError, "exited abnormally"):
            gate.load_run(self.merge_runs(a, c))

    def test_a_run_whose_record_is_cut_off_makes_the_merge_unscorable(self) -> None:
        a = self.results("a", outcomes=outcomes_json(mutant(MISSED, "CaughtMutant")))
        b = self.results("b", outcomes=outcomes_json(mutant(TIMEOUT, "CaughtMutant")))
        (b / "outcomes.json").write_text('{"outcomes": [')
        with self.assertRaisesRegex(gate.ResultsError, "exited abnormally.*cannot be read"):
            gate.load_run(self.merge_runs(a, b))

    def test_no_runs_writes_nothing(self) -> None:
        self.assertFalse(self.merge_runs(self.tmp / "absent").exists())


if __name__ == "__main__":
    unittest.main()
