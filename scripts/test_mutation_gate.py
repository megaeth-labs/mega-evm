#!/usr/bin/env python3
"""Tests of scripts/mutation_gate.py and of the guards of scripts/mutation_test.sh.

Run from the repository root with Python 3.11 or newer (the gate reads TOML with
tomllib):

    python3 -m unittest discover -s scripts -p 'test_*.py' -v

Each case writes the files a mutation run leaves in a temporary directory, in the
shapes cargo-mutants 27 and scripts/umutate.py write them, and runs the gate's
subcommands on it.
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent


def load_gate():
    spec = importlib.util.spec_from_file_location("mutation_gate", SCRIPTS / "mutation_gate.py")
    module = importlib.util.module_from_spec(spec)
    # Registered before it runs: its dataclasses look their module up.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


gate = load_gate()

CAUGHT = "crates/mega-evm/src/a.rs:1:1: replace f -> u64 with 0"
MISSED = "crates/mega-evm/src/a.rs:2:5: replace + with - in g"
TIMEOUT = "crates/mega-evm/src/system/keyless/dispatch.rs:367:23: delete ! in prepare"
UNVIABLE = "crates/mega-evm/src/a.rs:3:9: replace h -> Foo with Default::default()"


def mutant(name: str, summary: str) -> dict:
    return {"scenario": {"Mutant": {"name": name}}, "summary": summary}


def outcomes_json(*mutants: dict, baseline: str | None = "Success", total: int | None = None) -> dict:
    outcomes = [] if baseline is None else [{"scenario": "Baseline", "summary": baseline}]
    outcomes += list(mutants)
    return {"total_mutants": len(mutants) if total is None else total, "outcomes": outcomes}


class GateCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="mutation-gate-test-"))
        self.addCleanup(shutil.rmtree, self.tmp)

    def results(self, name: str = "mutants.out", outcomes: dict | None = None,
                files: dict[str, str] | None = None) -> Path:
        path = self.tmp / name
        path.mkdir(parents=True)
        if outcomes is not None:
            (path / "outcomes.json").write_text(json.dumps(outcomes))
        for file, text in (files or {}).items():
            (path / file).write_text(text)
        return path

    def suppressions(self, *mutants: str) -> Path:
        path = self.tmp / "suppressions.toml"
        entries = [f'[[suppress]]\nkind = "line"\nfile = "x"\nmutant = "{m}"\n' for m in mutants]
        path.write_text("\n".join(entries))
        return path

    def report(self, results: Path, recheck: Path | None = None,
               suppressions: Path | None = None) -> tuple[int, str]:
        comment = self.tmp / "comment.md"
        args = Namespace(
            results=str(results),
            recheck=str(recheck) if recheck else None,
            suppressions=str(suppressions) if suppressions else None,
            comment=str(comment),
            summary=None,
        )
        with contextlib.redirect_stdout(io.StringIO()):
            code = gate.cmd_report(args)
        text = comment.read_text()
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

    def test_an_interrupted_run(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "CaughtMutant"), total=3))
        self.assert_unscorable(results, "1 of 3 mutants have an outcome")

    def test_an_outcome_of_an_unknown_kind(self) -> None:
        results = self.results(outcomes=outcomes_json(mutant(CAUGHT, "Exploded")))
        self.assert_unscorable(results, "none this gate knows")

    def test_outcomes_that_are_not_json(self) -> None:
        results = self.results(files={"outcomes.json": "{"})
        self.assert_unscorable(results, "is not a mutation run's outcomes")

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
        for suppressed in (MISSED, gate.mutant_body(MISSED)):
            code, text = self.report(results, suppressions=self.suppressions(suppressed))
            self.assertEqual(code, 0, text)

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


class DriverGuard(unittest.TestCase):
    """scripts/mutation_test.sh refuses to run on a shell that is not bash 5 or newer."""

    DRIVER = SCRIPTS / "mutation_test.sh"

    def assert_refused(self, shell: str) -> None:
        run = subprocess.run([shell, str(self.DRIVER), "full"], capture_output=True, text=True,
                             timeout=60)
        self.assertEqual(run.returncode, 1, run.stderr)
        self.assertIn("needs bash 5 or newer", run.stderr)
        self.assertEqual(run.stdout, "")

    def test_a_posix_shell_is_refused(self) -> None:
        # dash on Linux, bash 3.2 in POSIX mode on macOS.
        self.assert_refused("/bin/sh")

    def test_an_old_bash_is_refused(self) -> None:
        bash = shutil.which("bash", path="/bin")
        if bash is None:
            self.skipTest("no /bin/bash")
        version = subprocess.run([bash, "-c", "echo ${BASH_VERSINFO[0]}"], capture_output=True,
                                 text=True).stdout.strip()
        if int(version) >= 5:
            self.skipTest(f"/bin/bash is bash {version}")
        self.assert_refused(bash)


if __name__ == "__main__":
    unittest.main()
