#!/usr/bin/env python3
"""Tests for scripts/mutation_gate.py, run through its command line.

    python3 scripts/test_mutation_gate.py
"""
from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).resolve().with_name("mutation_gate.py")

CAUGHT = "crates/mega-evm/src/a.rs:1:1: replace f -> bool with true"
MISSED = "crates/mega-evm/src/a.rs:2:1: replace g -> bool with false"
UNVIABLE = "crates/mega-evm/src/a.rs:3:1: replace h -> Foo with Default::default()"


def gate(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(GATE), *args], capture_output=True, text=True)


class GateTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.results = self.tmp / "mutants.out"
        self.status = self.tmp / "status"
        self.comment = self.tmp / "comment.md"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write_run(self, planned: int, **lists: list[str]) -> None:
        """Lay out a cargo-mutants output directory with `planned` mutants in its plan."""
        self.results.mkdir()
        (self.results / "mutants.json").write_text(json.dumps([{"name": i} for i in range(planned)]))
        for name in ("caught", "missed", "timeout", "unviable"):
            lines = lists.get(name, [])
            (self.results / f"{name}.txt").write_text("".join(f"{m}\n" for m in lines))

    def write_status(self, status: str, expected: str, outcomes: str) -> None:
        self.status.write_text(
            f"MUTANTS-STATUS status={status} expected={expected} outcomes={outcomes}\n")

    def report(self, *extra: str) -> subprocess.CompletedProcess:
        return gate("report", "--results", str(self.results), "--comment", str(self.comment), *extra)

    # ---- progress ----

    def test_progress_before_any_output(self) -> None:
        out = gate("progress", "--results", str(self.results))
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.strip(), "expected=unknown outcomes=0")

    def test_progress_counts_plan_and_outcomes(self) -> None:
        self.write_run(5, caught=[CAUGHT], missed=[MISSED], unviable=[UNVIABLE])
        out = gate("progress", "--results", str(self.results))
        self.assertEqual(out.stdout.strip(), "expected=5 outcomes=3")

    # ---- report without --status (unchanged behaviour) ----

    def test_absent_results_pass_without_status(self) -> None:
        out = self.report()
        self.assertEqual(out.returncode, 0, out.stdout)
        self.assertIn("nothing was mutated", out.stdout)

    def test_survivor_fails(self) -> None:
        self.write_run(2, caught=[CAUGHT], missed=[MISSED])
        self.assertEqual(self.report().returncode, 1)

    def test_suppressed_survivor_passes(self) -> None:
        self.write_run(2, caught=[CAUGHT], missed=[MISSED])
        supp = self.tmp / "suppressions.toml"
        supp.write_text(f'[[suppress]]\nkind = "line"\nmutant = "{MISSED}"\n')
        self.assertEqual(self.report("--suppressions", str(supp)).returncode, 0)

    # ---- report with --status ----

    def test_complete_run_passes(self) -> None:
        self.write_run(2, caught=[CAUGHT], unviable=[UNVIABLE])
        self.write_status("complete", "2", "2")
        out = self.report("--status", str(self.status))
        self.assertEqual(out.returncode, 0, out.stdout)
        self.assertNotIn("Incomplete", out.stdout)

    def test_complete_run_still_gates_survivors(self) -> None:
        self.write_run(2, caught=[CAUGHT], missed=[MISSED])
        self.write_status("complete", "2", "2")
        self.assertEqual(self.report("--status", str(self.status)).returncode, 1)

    def test_partial_run_fails_even_when_all_tested_were_caught(self) -> None:
        self.write_run(3, caught=[CAUGHT], unviable=[UNVIABLE])
        self.write_status("partial", "3", "2")
        out = self.report("--status", str(self.status))
        self.assertEqual(out.returncode, 1, out.stdout)
        self.assertIn("Incomplete run", out.stdout)
        self.assertIn("2 of 3", out.stdout)
        self.assertIn("caught: 1", out.stdout)  # what it tested is still reported
        self.assertIn("Incomplete run", self.comment.read_text())

    def test_not_started_without_results_fails(self) -> None:
        self.write_status("not-started", "unknown", "0")
        out = self.report("--status", str(self.status))
        self.assertEqual(out.returncode, 1, out.stdout)
        self.assertIn("before any mutant was tested", out.stdout)

    def test_not_started_with_plan_fails(self) -> None:
        self.write_run(4)
        self.write_status("not-started", "4", "0")
        out = self.report("--status", str(self.status))
        self.assertEqual(out.returncode, 1, out.stdout)
        self.assertIn("4 planned", out.stdout)
        self.assertNotIn("PASS", out.stdout)

    def test_empty_diff_status_passes(self) -> None:
        self.write_status("complete", "0", "0")
        self.assertEqual(self.report("--status", str(self.status)).returncode, 0)

    def test_missing_status_file_refuses(self) -> None:
        self.write_run(1, caught=[CAUGHT])
        out = self.report("--status", str(self.status))
        self.assertEqual(out.returncode, 2)
        self.assertIn("MUTANTS-STATUS", out.stderr)

    # ---- merge (sharded full runs) ----

    def write_shard(self, name: str, status: str | None, **lists: list[str]) -> Path:
        """Lay out one shard's artifact; `status=None` leaves no status file."""
        shard = self.tmp / name
        (shard / "mutants.out").mkdir(parents=True)
        for kind in ("caught", "missed", "timeout", "unviable"):
            lines = lists.get(kind, [])
            (shard / "mutants.out" / f"{kind}.txt").write_text("".join(f"{m}\n" for m in lines))
        if status is not None:
            (shard / "status").write_text(f"MUTANTS-STATUS {status}\n")
        return shard

    def merge(self, *shards: Path) -> subprocess.CompletedProcess:
        inventory = self.tmp / "inventory.txt"
        # `cargo mutants --list` output, with the colour codes CI can leave in it.
        inventory.write_text(f"\x1b[33m{CAUGHT}\x1b[0m\n{MISSED}\n{UNVIABLE}\n")
        return gate("merge", "--inventory", str(inventory), "--out", str(self.tmp / "merged"),
                    *map(str, shards))

    def merged_report(self) -> subprocess.CompletedProcess:
        merged = self.tmp / "merged"
        return gate("report", "--results", str(merged / "mutants.out"),
                    "--status", str(merged / "status"))

    def test_merge_of_all_shards_is_complete(self) -> None:
        a = self.write_shard("a", "status=complete expected=2 outcomes=2",
                             caught=[CAUGHT], unviable=[UNVIABLE])
        b = self.write_shard("b", "status=complete expected=1 outcomes=1", caught=[MISSED])
        out = self.merge(a, b)
        self.assertEqual(out.returncode, 0, out.stderr)
        merged = self.tmp / "merged"
        self.assertEqual((merged / "status").read_text().strip(),
                         "MUTANTS-STATUS status=complete expected=3 outcomes=3 "
                         "unplanned=0 duplicated=0 shards-planned=3 shards=complete,complete")
        self.assertEqual((merged / "mutants.out" / "caught.txt").read_text().splitlines(),
                         [CAUGHT, MISSED])
        self.assertEqual(self.merged_report().returncode, 0)

    def test_shard_without_artifact_is_named(self) -> None:
        a = self.write_shard("a", "status=complete expected=2 outcomes=2",
                             caught=[CAUGHT], unviable=[UNVIABLE])
        self.merge(a, self.tmp / "never-uploaded")
        status = (self.tmp / "merged" / "status").read_text().strip()
        self.assertIn("status=error expected=3 outcomes=2 ", status)
        self.assertIn("shards=complete,no-artifact", status)
        out = self.merged_report()
        self.assertEqual(out.returncode, 1, out.stdout)
        self.assertIn("2 of 3", out.stdout)
        self.assertIn("shard 1: `no-artifact` — it uploaded no results", out.stdout)
        self.assertNotIn("time budget", out.stdout)

    def test_baseline_failed_shard_is_named_not_blamed_on_the_budget(self) -> None:
        a = self.write_shard("a", "status=complete expected=2 outcomes=2",
                             caught=[CAUGHT], unviable=[UNVIABLE])
        b = self.write_shard("b", "status=baseline-failed expected=unknown outcomes=0")
        self.merge(a, b)
        status = (self.tmp / "merged" / "status").read_text()
        self.assertIn("status=baseline-failed expected=3 outcomes=2 ", status)
        out = self.merged_report()
        self.assertEqual(out.returncode, 1, out.stdout)
        self.assertIn("shard 1: `baseline-failed` — the unmutated tree failed", out.stdout)
        self.assertNotIn("time budget", out.stdout)

    def test_shard_failures_outrank_a_budget_stop(self) -> None:
        a = self.write_shard("a", "status=error expected=2 outcomes=1", caught=[CAUGHT])
        b = self.write_shard("b", "status=partial expected=1 outcomes=0")
        c = self.write_shard("c", None)
        self.merge(a, b, c)
        status = (self.tmp / "merged" / "status").read_text()
        self.assertIn("status=error ", status)
        self.assertIn("shards=error,partial,no-status", status)
        out = self.merged_report()
        self.assertIn("shard 0: `error` — the run itself failed", out.stdout)
        self.assertIn("shard 1: `partial` — its time budget ran out", out.stdout)
        self.assertIn("shard 2: `no-status` — it ended without a status line", out.stdout)

    def test_budget_stopped_shard_still_blames_the_budget(self) -> None:
        a = self.write_shard("a", "status=complete expected=2 outcomes=2",
                             caught=[CAUGHT], unviable=[UNVIABLE])
        b = self.write_shard("b", "status=not-started expected=1 outcomes=0")
        self.merge(a, b)
        self.assertIn("status=partial expected=3 outcomes=2 ",
                      (self.tmp / "merged" / "status").read_text())
        out = self.merged_report()
        self.assertEqual(out.returncode, 1, out.stdout)
        self.assertIn("the time budget ran out after 2 of 3", out.stdout)
        self.assertIn("shard 1: `not-started`", out.stdout)

    def test_merge_of_no_shards_is_not_started(self) -> None:
        self.merge()
        self.assertIn("status=not-started expected=3 outcomes=0",
                      (self.tmp / "merged" / "status").read_text())
        out = self.merged_report()
        self.assertEqual(out.returncode, 1)
        self.assertIn("no shard results were found", out.stdout)

    def test_merge_reports_an_unplanned_survivor(self) -> None:
        unplanned = "crates/mega-evm/src/z.rs:9:9: replace z -> bool with true"
        a = self.write_shard("a", "status=complete expected=2 outcomes=2",
                             caught=[CAUGHT], unviable=[UNVIABLE])
        b = self.write_shard("b", "status=complete expected=2 outcomes=2",
                             caught=[MISSED], missed=[unplanned])
        out = self.merge(a, b)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn(unplanned, out.stdout)
        merged = self.tmp / "merged"
        self.assertIn("status=mismatch expected=3 outcomes=3 unplanned=1 duplicated=0",
                      (merged / "status").read_text())
        # The unplanned survivor is kept, listed, and fails the report.
        self.assertEqual((merged / "mutants.out" / "missed.txt").read_text(), f"{unplanned}\n")
        self.assertEqual((merged / "unplanned.txt").read_text(), f"{unplanned}\n")
        report = self.merged_report()
        self.assertEqual(report.returncode, 1, report.stdout)
        self.assertIn("1 shard outcome(s) are for mutants the inventory does not list",
                      report.stdout)
        self.assertIn(f"`{unplanned}`", report.stdout)

    def test_merge_keeps_both_outcomes_of_a_mutant_tested_twice(self) -> None:
        a = self.write_shard("a", "status=complete expected=2 outcomes=2",
                             caught=[CAUGHT, MISSED])
        b = self.write_shard("b", "status=complete expected=2 outcomes=2",
                             missed=[MISSED], unviable=[UNVIABLE])
        self.merge(a, b)
        merged = self.tmp / "merged"
        self.assertIn("status=mismatch expected=3 outcomes=3 unplanned=0 duplicated=1",
                      (merged / "status").read_text())
        self.assertEqual((merged / "mutants.out" / "missed.txt").read_text(), f"{MISSED}\n")
        report = self.merged_report()
        self.assertEqual(report.returncode, 1, report.stdout)
        self.assertIn("1 mutant(s) have more than one outcome", report.stdout)

    def test_merge_flags_shard_plans_that_miss_inventory_mutants(self) -> None:
        # Both shards finished their own plans, but those plans left out a mutant
        # the inventory lists: the run is not partial for lack of time.
        a = self.write_shard("a", "status=complete expected=1 outcomes=1", caught=[CAUGHT])
        b = self.write_shard("b", "status=complete expected=1 outcomes=1", missed=[MISSED])
        self.merge(a, b)
        self.assertIn("status=mismatch expected=3 outcomes=2 unplanned=0 duplicated=0 "
                      "shards-planned=2", (self.tmp / "merged" / "status").read_text())
        report = self.merged_report()
        self.assertEqual(report.returncode, 1, report.stdout)
        self.assertIn("the shards planned 2 mutants", report.stdout)
        self.assertNotIn("time budget", report.stdout)

    def test_merge_refuses_an_empty_inventory(self) -> None:
        (self.tmp / "inventory.txt").write_text("")
        out = gate("merge", "--inventory", str(self.tmp / "inventory.txt"),
                   "--out", str(self.tmp / "merged"))
        self.assertEqual(out.returncode, 2)

    # ---- exclude-re ----

    def test_exclude_re_emits_one_pair_per_function_suppression(self) -> None:
        supp = self.tmp / "suppressions.toml"
        supp.write_text('[[suppress]]\nkind = "function"\npattern = "foo"\n'
                        '[[suppress]]\nkind = "line"\nmutant = "x"\n')
        out = gate("exclude-re", "--suppressions", str(supp))
        self.assertEqual(out.stdout.splitlines(), ["--exclude-re", "foo"])


if __name__ == "__main__":
    unittest.main()
