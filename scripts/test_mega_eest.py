#!/usr/bin/env python3
"""Tests of scripts/mega_eest.py, the structured-file half of the Mega EEST gate.

Run from the repository root with Python 3.11 or newer (the script reads TOML with tomllib):

    python3 -m unittest discover -s scripts -p 'test_*.py' -v

Each case writes, in a temporary directory, the files the gate reads — a mega-evm root manifest
and lock, the suite's adapter manifest, the run reports `mega-fixture consume` writes and the
slice's index — in the shapes they have, and runs a subcommand on them.
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from argparse import Namespace
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
ROOT = SCRIPTS.parent


def load_script(name: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


eest = load_script("mega_eest")

FORK = "git+https://github.com/megaeth-labs/revm?rev=aaaa#aaaa"
OTHER_FORK = "git+https://github.com/megaeth-labs/revm?rev=bbbb#bbbb"
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"

ROOT_MANIFEST = """
[workspace]
members = ["crates/mega-evm"]

[workspace.dependencies]
revm = { version = "=40.0.3", default-features = false }
alloy-rlp = { version = "0.3.16", default-features = false }
op-revm = { git = "https://github.com/ethereum-optimism/optimism", rev = "new", default-features = false }
mega-evm = { path = "crates/mega-evm", version = "2.0.0" }

[profile.dev.package]
blst.opt-level = 3

[patch.crates-io]
revm = { git = "https://github.com/megaeth-labs/revm", rev = "new-fork" }

[patch."https://github.com/ethereum-optimism/optimism"]
op-revm = { git = "https://github.com/megaeth-labs/op-revm", rev = "new-op" }
"""

ADAPTER_MANIFEST = """
[workspace]

[package]
name = "mega-eest-adapter-revm"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "adapter-revm"
path = "src/main.rs"

[dependencies]
mega-fixture-format = { path = "../../crates/mega-fixture-format" }
mega-evm = { git = "https://github.com/megaeth-labs/mega-evm", rev = "old", features = ["test-utils"] }
revm = { version = "=40.0.3" }
alloy-rlp = { version = "0.3" }
op-revm = { git = "https://github.com/ethereum-optimism/optimism", rev = "old" }
serde = "1"

[patch.crates-io]
revm = { git = "https://github.com/megaeth-labs/revm", rev = "old-fork" }
"""


def lock(*packages: dict) -> str:
    """A Cargo.lock holding `packages`, each `{name, version, source?, dependencies?}`."""
    out = ["version = 4", ""]
    for p in packages:
        out.append("[[package]]")
        out.append(f'name = "{p["name"]}"')
        out.append(f'version = "{p["version"]}"')
        if "source" in p:
            out.append(f'source = "{p["source"]}"')
        if p.get("dependencies"):
            out.append("dependencies = [" + ", ".join(json.dumps(d) for d in p["dependencies"]) + "]")
        out.append("")
    return "\n".join(out)


class Case(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="mega-eest-test-"))
        self.addCleanup(shutil.rmtree, self.tmp)

    def run_quietly(self, func, args: Namespace) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = func(args)
            except eest.GateError as e:
                print(f"mega-eest: error: {e}", file=sys.stderr)
                code = 1
        return code, out.getvalue(), err.getvalue()


class RenderToml(unittest.TestCase):
    """The writer's text reads back as the document it was given, or it refuses."""

    def test_a_manifest_reads_back_as_written(self) -> None:
        doc = {
            "cargo-features": ["profile-rustflags"],
            "workspace": {},
            "package": {"name": "a", "version": "0.1.0", "metadata": {"k": "v"}},
            "bin": [{"name": "adapter-revm", "path": "src/main.rs"}],
            "dependencies": {"x": {"path": "/p q/\"r\"", "features": ["f"], "default-features": False}},
            "patch": {"https://github.com/o/r": {"op-revm": {"git": "u", "rev": "r"}}},
            "profile": {"dev": {"debug": "full", "package": {"blst": {"opt-level": 3}}}},
        }
        self.assertEqual(tomllib.loads(eest.render_toml(doc)), doc)

    def test_a_value_it_cannot_write_is_refused(self) -> None:
        with self.assertRaises(eest.GateError):
            eest.render_toml({"package": {"when": object()}})


class Adapter(Case):
    """The generated workspace builds the adapter against this checkout."""

    def write_inputs(self) -> tuple[Path, Path]:
        tree, suite = self.tmp / "tree", self.tmp / "suite"
        (tree / "crates" / "mega-evm").mkdir(parents=True)
        (tree / "Cargo.toml").write_text(ROOT_MANIFEST)
        (tree / "crates" / "mega-evm" / "Cargo.toml").write_text('[package]\nname = "mega-evm"\n')
        (tree / "Cargo.lock").write_text(lock({"name": "mega-evm", "version": "2.0.0"}))
        adapter = suite / "adapters" / "revm"
        (adapter / "src").mkdir(parents=True)
        (adapter / "Cargo.toml").write_text(ADAPTER_MANIFEST)
        (adapter / "Cargo.lock").write_text("# the adapter's own lock\n")
        (adapter / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "other"\n')
        (adapter / "build.rs").write_text("fn main() {}\n")
        (adapter / "src" / "main.rs").write_text("fn main() {}\n")
        return tree, suite

    def generate(self, tree: Path, suite: Path, out: Path, flags: str = "") -> dict:
        code, _, err = self.run_quietly(eest.cmd_adapter, Namespace(
            tree=str(tree), suite=str(suite), out=str(out), engine_rustflags=flags))
        self.assertEqual(code, 0, err)
        return tomllib.loads((out / "Cargo.toml").read_text())

    def test_the_engine_and_its_crates_follow_this_checkout(self) -> None:
        tree, suite = self.write_inputs()
        out = self.tmp / "out"
        manifest = self.generate(tree, suite, out)
        deps = manifest["dependencies"]
        # This checkout's package, by path, with the adapter's features.
        self.assertEqual(deps["mega-evm"], {"path": str((tree / "crates/mega-evm").resolve()),
                                            "features": ["test-utils"]})
        # The sources this checkout declares, without its default-features choice.
        self.assertEqual(deps["op-revm"], {"git": "https://github.com/ethereum-optimism/optimism",
                                           "rev": "new"})
        self.assertEqual(deps["alloy-rlp"], {"version": "0.3.16"})
        # The suite's own path dependency, made absolute; a crate this checkout does not declare,
        # as the adapter declares it.
        self.assertEqual(deps["mega-fixture-format"],
                         {"path": str((suite / "crates/mega-fixture-format").resolve())})
        self.assertEqual(deps["serde"], {"version": "1"})
        # This checkout's patches and profiles, replacing the adapter's.
        self.assertEqual(manifest["patch"]["crates-io"]["revm"]["rev"], "new-fork")
        self.assertEqual(manifest["patch"]["https://github.com/ethereum-optimism/optimism"]["op-revm"]["rev"],
                         "new-op")
        self.assertEqual(manifest["profile"]["dev"]["package"]["blst"], {"opt-level": 3})
        self.assertNotIn("cargo-features", manifest)
        # The adapter's sources are copied; its lock and toolchain are not, and the lock is this
        # checkout's.
        self.assertEqual(sorted(p.relative_to(out).as_posix() for p in out.rglob("*") if p.is_file()),
                         [".mega-eest-seed", "Cargo.lock", "Cargo.toml", "build.rs", "src/main.rs"])
        self.assertEqual((out / "Cargo.lock").read_text(), (tree / "Cargo.lock").read_text())

    def test_engine_rustflags_reach_the_engine_and_the_adapter_alone(self) -> None:
        tree, suite = self.write_inputs()
        manifest = self.generate(tree, suite, self.tmp / "out", "-Cinstrument-coverage --cfg=coverage")
        self.assertEqual(manifest["cargo-features"], ["profile-rustflags"])
        packages = manifest["profile"]["dev"]["package"]
        flags = ["-Cinstrument-coverage", "--cfg=coverage"]
        self.assertEqual(packages["mega-evm"], {"rustflags": flags})
        self.assertEqual(packages["mega-eest-adapter-revm"], {"rustflags": flags})
        self.assertEqual(packages["blst"], {"opt-level": 3})
        self.assertEqual(sorted(packages), ["blst", "mega-eest-adapter-revm", "mega-evm"])

    def test_a_resolved_lock_is_kept_and_a_changed_lock_reseeds(self) -> None:
        tree, suite = self.write_inputs()
        out = self.tmp / "out"
        self.generate(tree, suite, out)
        (out / "Cargo.lock").write_text("# what cargo resolved\n")
        self.generate(tree, suite, out)
        self.assertEqual((out / "Cargo.lock").read_text(), "# what cargo resolved\n")
        (tree / "Cargo.lock").write_text(lock({"name": "mega-evm", "version": "2.0.1"}))
        self.generate(tree, suite, out)
        self.assertEqual((out / "Cargo.lock").read_text(), (tree / "Cargo.lock").read_text())

    def test_an_adapter_that_does_not_depend_on_this_checkout_is_refused(self) -> None:
        tree, suite = self.write_inputs()
        manifest = suite / "adapters" / "revm" / "Cargo.toml"
        manifest.write_text(manifest.read_text().replace("\nmega-evm = ", "\nother-evm = "))
        code, _, err = self.run_quietly(eest.cmd_adapter, Namespace(
            tree=str(tree), suite=str(suite), out=str(self.tmp / "out"), engine_rustflags=""))
        self.assertEqual(code, 1)
        self.assertIn("depends on no package of this checkout", err)


class CheckLock(Case):
    """Every package mega-evm links is one this checkout locks, and there is one revm."""

    ENGINE = {"name": "mega-evm", "version": "2.0.0", "dependencies": ["revm", "alloy-rlp"]}
    REVM = {"name": "revm", "version": "40.0.3", "source": FORK}
    RLP = {"name": "alloy-rlp", "version": "0.3.16", "source": CRATES_IO}

    def check(self, adapter_lock: str) -> tuple[int, str]:
        tree, adapter = self.tmp / "tree", self.tmp / "adapter"
        tree.mkdir()
        adapter.mkdir()
        (tree / "Cargo.lock").write_text(lock(self.ENGINE, self.REVM, self.RLP))
        (adapter / "Cargo.lock").write_text(adapter_lock)
        code, _, err = self.run_quietly(eest.cmd_check_lock, Namespace(tree=str(tree), adapter=str(adapter)))
        return code, err

    def test_the_engine_as_this_checkout_locks_it_passes(self) -> None:
        extra = {"name": "sha2", "version": "0.10.9", "source": CRATES_IO}
        code, err = self.check(lock(self.ENGINE, self.REVM, self.RLP, extra))
        self.assertEqual(code, 0, err)
        self.assertIn("mega-evm links 3 packages", err)
        self.assertIn("for the adapter alone: sha2 0.10.9", err)

    def test_a_package_at_another_version_fails(self) -> None:
        newer = {"name": "alloy-rlp", "version": "0.3.17", "source": CRATES_IO}
        code, err = self.check(lock(self.ENGINE, self.REVM, newer))
        self.assertEqual(code, 1, err)
        self.assertIn("alloy-rlp 0.3.17", err)

    def test_a_second_revm_fails(self) -> None:
        engine = {**self.ENGINE, "dependencies": [f"revm 40.0.3 ({FORK})", "alloy-rlp"]}
        other = {"name": "revm", "version": "40.0.3", "source": OTHER_FORK}
        code, err = self.check(lock(engine, self.REVM, other, self.RLP))
        self.assertEqual(code, 1, err)
        self.assertIn("2 revm packages, not one", err)


class Check(Case):
    """The run reports decide the result, and its exit code says whether it is a verdict."""

    CASES = [("satin/a", "sha256:1"), ("satin/b", "sha256:2")]
    NEGATIVE = [{"caseId": "satin/negative/wrong", "file": "negative/wrong.json",
                 "status": "fail", "path": "/expected/x"}]
    INDEX_NEGATIVE = [{"caseId": "satin/negative/wrong", "file": "negative/wrong.json",
                       "wrongAt": "/expected/x"}]
    ORACLE_CONTROL = {"caseId": "satin/negative/oracle", "file": "negative/oracle.json",
                      "error": "unexpected oracle request for slot 0x3"}

    def write(self, reports: dict[str, list[dict] | None], negative: list[dict] | None = None,
              exits: dict[str, int] | None = None, *, modes: str | None = None,
              index_negative: list[dict] | None = None, provenance: str = "unchecked",
              repo: str = "unchecked", caseset: str = "unchecked") -> tuple[int, str, str]:
        # A fresh pair for every call: a test may write several runs.
        run = Path(tempfile.mkdtemp(dir=self.tmp))
        suite, work = run / "suite", run / "work"
        (suite / "slice").mkdir(parents=True)
        work.mkdir()
        (suite / "slice" / "index.json").write_text(json.dumps({
            "cases": [{"caseId": c, "fileDigest": d} for c, d in self.CASES],
            "negative": self.INDEX_NEGATIVE if index_negative is None else index_negative,
        }))
        for mode, cases in reports.items():
            if cases is not None:
                (work / f"{mode}.json").write_text(json.dumps({"cases": cases}))
            default = 1 if cases and any(c["status"] != "pass" for c in cases) else 0
            (work / f"{mode}.exit").write_text(f"{(exits or {}).get(mode, default)}\n")
            if negative is not None:
                controls = [{**c, "file": str(suite / "slice" / c["file"])} if "file" in c else c
                            for c in negative]
                (work / f"negative-{mode}.json").write_text(json.dumps({"cases": controls}))
                control_exit = 1 if any(c["status"] != "pass" for c in controls) else 0
                (work / f"negative-{mode}.exit").write_text(f"{control_exit}\n")
        return self.run_quietly(eest.cmd_check, Namespace(
            suite=str(suite), work=str(work), modes=" ".join(reports) if modes is None else modes,
            rev="f" * 40, provenance=provenance, repo=repo, caseset=caseset))

    @staticmethod
    def case(case_id: str, digest: str, status: str = "pass", observations: str = "sha256:o") -> dict:
        report = {"caseId": case_id, "fileDigest": digest, "status": status,
                  "observationsDigest": observations, "file": f"{case_id}.json"}
        if status == "fail":
            report.update(path="/expected/gasLedgers/state", expected="0x1", actual="0x2", rules=["R-1"])
        if status == "error":
            report.update(message="the adapter panicked")
        return report

    def all_passed(self) -> list[dict]:
        return [self.case(c, d) for c, d in self.CASES]

    def one_failing(self, status: str = "fail") -> list[dict]:
        return [self.case("satin/a", "sha256:1"), self.case("satin/b", "sha256:2", status)]

    def test_every_case_passing_in_both_modes_is_a_full_pass(self) -> None:
        code, out, err = self.write({"plain": self.all_passed(), "instrumented": self.all_passed()}, self.NEGATIVE)
        self.assertEqual(code, 0, err)
        self.assertEqual(out.strip(), f"MEGA-EEST rev={'f' * 40} run=full cases=2 "
                         "plain=pass:2,fail:0,unsupported:0,error:0 instrumented=pass:2,fail:0,unsupported:0,error:0 "
                         "parity=identical negative=fail-as-recorded provenance=unchecked repo=unchecked caseset=unchecked result=pass")

    def test_one_mode_is_a_partial_run(self) -> None:
        code, out, err = self.write({"plain": self.all_passed()}, self.NEGATIVE)
        self.assertEqual(code, 0, err)
        self.assertIn(" run=partial ", out)
        self.assertTrue(out.strip().endswith("result=pass"))

    def test_a_failing_or_erring_case_is_a_verdict(self) -> None:
        for status in ("fail", "error"):
            code, out, err = self.write({"plain": self.one_failing(status)}, self.NEGATIVE)
            self.assertEqual(code, 1, (status, err))
            self.assertTrue(out.strip().endswith("result=FAIL"), out)
        code, out, err = self.write({"plain": self.one_failing()}, self.NEGATIVE)
        self.assertIn("plain=pass:1,fail:1", out)
        self.assertIn('fail satin/b\n  first difference at /expected/gasLedgers/state: expected "0x1" actual "0x2"', err)
        self.assertIn("rules: R-1", err)

    def test_observations_that_differ_between_modes_are_a_verdict(self) -> None:
        other = [self.case("satin/a", "sha256:1"), self.case("satin/b", "sha256:2", observations="sha256:other")]
        code, out, err = self.write({"plain": self.all_passed(), "instrumented": other},
                                    self.control("fail", "/expected/y"))
        self.assertEqual(code, 1, err)
        self.assertIn("parity=1-differ", out)
        self.assertIn("negative=fail-elsewhere", out)
        self.assertIn("result=FAIL", out)

    def test_a_passing_case_without_a_digest_is_incomplete_before_parity(self) -> None:
        for modes, control, verdict in (("plain instrumented", self.control("fail", "/expected/y"), "not-caught"),
                                        ("instrumented", self.NEGATIVE, "fail-as-recorded")):
            with self.subTest(modes=modes):
                reports = {mode: self.all_passed() for mode in modes.split()}
                del reports["instrumented"][1]["observationsDigest"]
                run = self.write(reports, control)
                self.assert_no_verdict(*run, "instrumented: passed case satin/b has no observation digest")
                self.assertNotIn(" parity=", run[1])
                self.assertIn(f"negative={verdict}", run[1])

    def assert_no_verdict(self, code: int, out: str, err: str, reason: str) -> None:
        self.assertEqual(code, 2, (out, err))
        self.assertTrue(out.strip().endswith("result=INCOMPLETE"), out)
        self.assertIn(reason, err)

    def test_no_mode_is_no_verdict(self) -> None:
        for modes in ("", " ", " \t\n"):
            code, out, err = self.write({}, self.NEGATIVE, modes=modes)
            self.assert_no_verdict(code, out, err, "no mode was run")

    def test_cases_that_are_not_the_index_are_no_verdict(self) -> None:
        for cases in ([self.case("satin/a", "sha256:1")],
                      [self.case("satin/a", "sha256:1"), self.case("satin/b", "sha256:other")],
                      [],
                      # A failing case does not make an incomplete run a verdict.
                      [self.case("satin/a", "sha256:1", "fail")]):
            code, out, err = self.write({"plain": cases}, self.NEGATIVE)
            self.assert_no_verdict(code, out, err, "the cases consumed are not the slice's index")

    def test_a_mode_without_a_report_is_no_verdict(self) -> None:
        code, out, err = self.write({"plain": self.one_failing(), "instrumented": None}, self.NEGATIVE,
                                    exits={"instrumented": 2})
        self.assert_no_verdict(code, out, err, "wrote no report (exit 2)")
        self.assertIn("instrumented=no-report", out)

    def test_a_consume_exit_its_report_does_not_explain_is_no_verdict(self) -> None:
        for cases, exit_code in ((self.all_passed(), 1), (self.one_failing(), 0), (self.one_failing(), 101)):
            code, out, err = self.write({"plain": cases}, self.NEGATIVE, exits={"plain": exit_code})
            self.assert_no_verdict(code, out, err, f"exited {exit_code}")

    def test_a_status_the_consumer_does_not_define_is_no_verdict(self) -> None:
        # A consumer of a later pin with a fifth status: the four counts no longer add up.
        cases = [self.case("satin/a", "sha256:1"), self.case("satin/b", "sha256:2", "skipped")]
        for exit_code in (1, 0):
            code, out, err = self.write({"plain": cases}, self.NEGATIVE, exits={"plain": exit_code})
            self.assert_no_verdict(code, out, err, "case statuses the consumer does not define: skipped (1)")
            self.assertIn("plain=pass:1,fail:0,unsupported:0,error:0,skipped:1", out)

    def test_an_unsupported_case_is_no_verdict(self) -> None:
        code, out, err = self.write({"plain": self.one_failing("unsupported")}, self.NEGATIVE)
        self.assert_no_verdict(code, out, err, "the adapter does not support")

    @staticmethod
    def control(status: str, path: str | None = None) -> list[dict]:
        case = {"caseId": "satin/negative/wrong", "file": "negative/wrong.json", "status": status}
        if path is not None:
            case["path"] = path
        return [case]

    def test_failing_cases_with_the_control_failing_elsewhere_are_a_verdict(self) -> None:
        # A change of behaviour can move where the control first differs; the failing cases
        # already show the pipeline reports failures.
        for control in (self.control("fail", "/expected/gasLedgers/gasUsed"), self.control("error")):
            code, out, err = self.write({"plain": self.one_failing()}, control)
            self.assertEqual(code, 1, (control, err))
            self.assertIn("negative=fail-elsewhere", out)
            self.assertTrue(out.strip().endswith("result=FAIL"), out)
            self.assertIn("can move where it first differs", err)

    def test_failing_cases_with_a_control_that_did_not_fail_are_no_verdict(self) -> None:
        for control, reason in ((self.control("pass"), "gating cases failed, and it did not fail"),
                                (self.control("unsupported"), "gating cases failed, and it did not fail"),
                                ([], "not consumed"),
                                (None, "negative controls: no report")):
            code, out, err = self.write({"plain": self.one_failing()}, control)
            self.assert_no_verdict(code, out, err, reason)

    def test_passing_cases_need_the_control_to_fail_where_recorded(self) -> None:
        for control, reason in ((self.control("fail", "/expected/y"), "so it must match the recorded failure"),
                                (self.control("error"), "so it must match the recorded failure"),
                                (self.control("pass"), "so it must match the recorded failure"),
                                (None, "negative controls: no report")):
            code, out, err = self.write({"plain": self.all_passed()}, control)
            self.assert_no_verdict(code, out, err, reason)
            self.assertNotIn("negative=fail-as-recorded", out)
        run = self.write({"plain": self.all_passed()}, self.NEGATIVE, index_negative=[])
        self.assert_no_verdict(*run, "lists no negative control")
        self.assertIn("negative=none", run[1])

    def test_passing_cases_accept_both_recorded_control_forms(self) -> None:
        oracle = {"caseId": self.ORACLE_CONTROL["caseId"], "status": "error",
                  "file": "negative/oracle.json",
                  "message": self.ORACLE_CONTROL["error"]}
        code, out, err = self.write({"plain": self.all_passed()}, [*self.NEGATIVE, oracle],
                                    index_negative=[*self.INDEX_NEGATIVE, self.ORACLE_CONTROL])
        self.assertEqual(code, 0, err)
        self.assertIn("negative=fail-as-recorded", out)
        self.assertTrue(out.strip().endswith("result=pass"), out)

    def test_passing_cases_require_the_exact_recorded_error(self) -> None:
        for result in ({"status": "error", "message": self.ORACLE_CONTROL["error"] + " (retry)"},
                       {"status": "fail", "path": "/expected/x", "message": self.ORACLE_CONTROL["error"]}):
            with self.subTest(result=result):
                oracle = {"caseId": self.ORACLE_CONTROL["caseId"], "file": "negative/oracle.json", **result}
                code, out, err = self.write({"plain": self.all_passed()}, [*self.NEGATIVE, oracle],
                                            index_negative=[*self.INDEX_NEGATIVE, self.ORACLE_CONTROL])
                self.assert_no_verdict(code, out, err, "so it must match the recorded failure")
                self.assertIn("negative=not-caught", out)

    def test_malformed_control_entries_are_no_verdict(self) -> None:
        entry = self.INDEX_NEGATIVE[0]
        common = {k: entry[k] for k in ("caseId", "file")}
        for malformed in (common, {**entry, "error": "an error"}, {**entry, "extra": "unexpected"},
                          {**entry, "wrongAt": None}, {**entry, "file": "/wrong.json"},
                          {**entry, "file": "../wrong.json"}):
            with self.subTest(entry=malformed):
                code, out, err = self.write({"plain": self.one_failing()}, self.NEGATIVE,
                                            index_negative=[malformed])
                self.assert_no_verdict(code, out, err, "malformed negative control")
                self.assertIn("negative=invalid-index", out)

    def test_failing_cases_accept_an_error_control_that_errs_differently(self) -> None:
        oracle = {"caseId": self.ORACLE_CONTROL["caseId"], "status": "error",
                  "file": "negative/oracle.json",
                  "message": "a different adapter error"}
        code, out, err = self.write({"plain": self.one_failing()}, [*self.NEGATIVE, oracle],
                                    index_negative=[*self.INDEX_NEGATIVE, self.ORACLE_CONTROL])
        self.assertEqual(code, 1, err)
        self.assertIn("negative=fail-elsewhere", out)
        self.assertTrue(out.strip().endswith("result=FAIL"), out)

    def test_failing_cases_with_an_error_control_that_passes_are_no_verdict(self) -> None:
        oracle = {"caseId": self.ORACLE_CONTROL["caseId"], "file": "negative/oracle.json", "status": "pass"}
        code, out, err = self.write({"plain": self.one_failing()}, [*self.NEGATIVE, oracle],
                                    index_negative=[*self.INDEX_NEGATIVE, self.ORACLE_CONTROL])
        self.assert_no_verdict(code, out, err, "gating cases failed, and it did not fail")
        self.assertIn("negative=not-caught", out)

    def test_an_extra_or_repeated_control_report_is_no_verdict(self) -> None:
        for extra in ({"caseId": "satin/negative/extra", "status": "error"}, self.NEGATIVE[0]):
            with self.subTest(extra=extra):
                code, out, err = self.write({"plain": self.one_failing()}, [*self.NEGATIVE, extra])
                self.assert_no_verdict(code, out, err, "negative control report has extra or repeated case IDs")
                self.assertIn("negative=not-caught", out)

    def test_a_control_report_must_name_the_indexed_file(self) -> None:
        index_negative = [{**self.INDEX_NEGATIVE[0], "file": "negative/other.json"}]
        for gating, control in ((self.all_passed(), self.NEGATIVE),
                                (self.one_failing(), self.control("error"))):
            with self.subTest(gating=gating):
                run = self.write({"plain": gating}, control, index_negative=index_negative)
                self.assert_no_verdict(*run, "expected fixture negative/other.json")
                self.assertIn("negative=not-caught", run[1])

    def test_a_changed_repository_is_refused(self) -> None:
        for provenance, repo, want in (("default-branch", "changed", 3), ("default-branch", "stale", 3),
                                       ("default-branch", "moved", 0), ("unchecked", "moved", 3),
                                       ("default-branch", "same", 0), ("unchecked", "same", 0),
                                       ("default-branch", "unchecked", 0)):
            code, out, err = self.write({"plain": self.all_passed()}, self.NEGATIVE,
                                        provenance=provenance, repo=repo, caseset="kept")
            self.assertEqual(code, want, (provenance, repo, err))
            self.assertIn(f"provenance={provenance} repo={repo} caseset=kept", out)

    def test_a_refused_pin_exits_3_and_failing_cases_still_exit_1(self) -> None:
        for provenance, caseset in (("unmerged", "unmoved"), ("default-branch", "undeclared")):
            code, out, err = self.write({"plain": self.all_passed()}, self.NEGATIVE,
                                        provenance=provenance, caseset=caseset)
            self.assertEqual(code, 3, err)
            self.assertIn(f"provenance={provenance} repo=unchecked caseset={caseset} result=REFUSED", out)
        code, out, _ = self.write({"plain": self.one_failing()}, self.NEGATIVE, provenance="unmerged")
        self.assertEqual(code, 1)
        for caseset in ("unmoved", "kept", "declared"):
            code, out, err = self.write({"plain": self.all_passed()}, self.NEGATIVE,
                                        provenance="default-branch", caseset=caseset)
            self.assertEqual(code, 0, err)

    def test_a_checker_that_cannot_read_the_run_is_no_verdict(self) -> None:
        run = Path(tempfile.mkdtemp(dir=self.tmp))
        (run / "suite" / "slice").mkdir(parents=True)
        (run / "work").mkdir()
        (run / "suite" / "slice" / "index.json").write_text("{ not json")
        code, out, err = self.run_quietly(eest.cmd_check, Namespace(
            suite=str(run / "suite"), work=str(run / "work"), modes="plain", rev="f" * 40,
            provenance="unchecked", caseset="unchecked"))
        self.assertEqual(code, 2, err)
        self.assertTrue(out.strip().endswith("stage=check result=INCOMPLETE"), out)


class Refusal(Case):
    """The causes a refused run's annotation and log name, read off the summary line."""

    LINE = "MEGA-EEST rev={} run=full cases=2 negative=fail-as-recorded {} result=REFUSED".format("f" * 40, "{}")

    def refusal(self, fields: str) -> str:
        code, out, _ = self.run_quietly(eest.cmd_refusal, Namespace(summary=self.LINE.format(fields)))
        self.assertEqual(code, 0)
        return out.strip()

    def test_each_cause_is_named_by_its_field(self) -> None:
        for fields, cause in (
                ("provenance=unmerged repo=same caseset=kept", "(provenance=unmerged)"),
                ("provenance=default-branch repo=changed caseset=kept", "(repo=changed)"),
                ("provenance=default-branch repo=stale caseset=kept", "(repo=stale)"),
                ("provenance=unchecked repo=moved caseset=kept", "(repo=moved provenance=unchecked)"),
                ("provenance=default-branch repo=same caseset=undeclared", "(caseset=undeclared)")):
            said = self.refusal(fields)
            self.assertTrue(said.endswith(cause), (fields, said))
            self.assertNotIn(";", said, fields)

    def test_several_causes_are_all_named(self) -> None:
        said = self.refusal("provenance=unmerged repo=changed caseset=undeclared")
        for cause in ("(provenance=unmerged)", "(repo=changed)", "(caseset=undeclared)"):
            self.assertIn(cause, said)

    def test_the_checker_refuses_exactly_for_a_cause(self) -> None:
        for provenance in ("default-branch", "unmerged", "unchecked"):
            for repo in ("same", "moved", "changed", "stale", "unchecked"):
                for caseset in ("unmoved", "kept", "declared", "undeclared", "unchecked"):
                    fields = {"provenance": provenance, "repo": repo, "caseset": caseset}
                    refused = (provenance == "unmerged" or repo in ("changed", "stale")
                               or (repo == "moved" and provenance == "unchecked") or caseset == "undeclared")
                    self.assertEqual(bool(eest.refusal_causes(fields)), refused, fields)

    def test_the_jobs_annotation_names_the_cause(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "mega-eest.yml").read_text()
        arms = [line.strip() for line in workflow.splitlines() if line.strip().startswith("3) echo \"::error::")]
        self.assertEqual(len(arms), 1, arms)
        line = self.LINE.format("provenance=default-branch repo=same caseset=undeclared")
        run = subprocess.run(["bash", "-c", f'status=3; case "$status" in {arms[0]} esac', "_"],
                             cwd=ROOT, env={"PATH": f"{Path(sys.executable).parent}:/usr/bin:/bin",
                                            "line": line}, capture_output=True, text=True, timeout=60)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("MEGA_EEST_CHANGED_CASES do not list exactly (caseset=undeclared)", run.stdout)
        self.assertNotIn("not on Mega EEST's default branch", run.stdout)


class Executable(Case):
    """The executable is the one cargo reported building, not a file at a guessed path."""

    @staticmethod
    def artifact(name: str, path: Path | None) -> str:
        return json.dumps({"reason": "compiler-artifact", "target": {"name": name},
                           "executable": str(path) if path else None})

    def run_with(self, *lines: str) -> tuple[int, str, str]:
        stdin = io.StringIO("\n".join(lines) + "\n")
        old, sys.stdin = sys.stdin, stdin
        try:
            return self.run_quietly(eest.cmd_executable, Namespace(name="adapter-revm"))
        finally:
            sys.stdin = old

    def test_the_reported_executable_wins_over_a_stale_file(self) -> None:
        stale = self.tmp / "target" / "debug" / "adapter-revm"
        built = self.tmp / "target" / "aarch64-apple-darwin" / "debug" / "adapter-revm"
        for path in (stale, built):
            path.parent.mkdir(parents=True)
            path.write_text("#!/bin/sh\n")
        code, out, err = self.run_with("Compiling noise", self.artifact("mega-evm", None),
                                       self.artifact("adapter-revm", built),
                                       json.dumps({"reason": "build-finished", "success": True}))
        self.assertEqual(code, 0, err)
        self.assertEqual(out.strip(), str(built))

    def test_no_executable_two_executables_or_a_missing_file_are_refused(self) -> None:
        a, b = self.tmp / "a" / "adapter-revm", self.tmp / "b" / "adapter-revm"
        for path in (a, b):
            path.parent.mkdir(parents=True)
            path.write_text("")
        for lines, reason in (
                ([json.dumps({"reason": "build-finished", "success": True})], "0 executables"),
                ([self.artifact("adapter-revm", a), self.artifact("adapter-revm", b)], "2 executables"),
                ([self.artifact("adapter-revm", self.tmp / "gone" / "adapter-revm")], "no such file"),
        ):
            code, _, err = self.run_with(*lines)
            self.assertEqual(code, 1, lines)
            self.assertIn(reason, err)


class CaseDiff(Case):
    """A pin move lists the case set's changes, and removals must be declared exactly."""

    BASE = {"satin/a": "sha256:1", "satin/b": "sha256:2", "satin/c": "sha256:3"}

    def diff(self, head: dict[str, str], declared: str = "", changes: str = "") -> tuple[int, str, str, str]:
        run = Path(tempfile.mkdtemp(dir=self.tmp))
        for name, cases in (("base.json", self.BASE), ("head.json", head)):
            (run / name).write_text(json.dumps({"cases": [{"caseId": c, "fileDigest": d}
                                                          for c, d in cases.items()]}))
        code, out, err = self.run_quietly(eest.cmd_case_diff, Namespace(
            base_rev="a" * 40, rev="b" * 40, base_index=str(run / "base.json"),
            index=str(run / "head.json"), declared=declared, declared_changes=changes,
            summary=str(run / "diff.md")))
        return code, out.strip(), err, (run / "diff.md").read_text()

    def test_a_move_that_only_adds_cases_is_kept(self) -> None:
        code, out, err, md = self.diff({**self.BASE, "satin/d": "sha256:4"})
        self.assertEqual((code, out), (0, "kept"), err)
        self.assertIn("0 case(s) removed, 1 added, 0 changed", err)
        self.assertIn("added satin/d", err)
        self.assertIn("- `satin/d`", md)

    def test_changes_under_an_id_must_be_declared_exactly(self) -> None:
        head = {**self.BASE, "satin/a": "sha256:9", "satin/d": "sha256:4"}
        code, out, err, md = self.diff(head)
        self.assertEqual((code, out), (0, "undeclared"), err)
        self.assertIn("changed satin/a", err)
        self.assertIn("MEGA_EEST_CHANGED_CASES must list exactly", err)
        self.assertIn("changed and not declared: satin/a", err)
        self.assertIn("<summary>Changed (1)</summary>", md)
        self.assertEqual(self.diff(head, changes="satin/a")[1], "declared")
        for changes in ("satin/a satin/b", "satin/b"):
            self.assertEqual(self.diff(head, changes=changes)[1], "undeclared", changes)
        # A removal and a change, each declared in its own line.
        both = {"satin/a": "sha256:9", "satin/b": "sha256:2"}
        self.assertEqual(self.diff(both, declared="satin/c", changes="satin/a")[1], "declared")
        self.assertEqual(self.diff(both, declared="satin/c satin/a")[1], "undeclared")

    def test_removals_must_be_declared_exactly(self) -> None:
        head = {"satin/a": "sha256:1", "satin/b": "sha256:2"}
        self.assertEqual(self.diff(head, "satin/c")[1], "declared")
        for declared in ("", "satin/c satin/b", "satin/x"):
            code, out, err, md = self.diff(head, declared)
            self.assertEqual((code, out), (0, "undeclared"), declared)
            self.assertIn("must list exactly the case ids the pin removes", err)
            self.assertIn("removals and changes: **undeclared**", md)
        # A declaration left over from an earlier move, with nothing removed now.
        self.assertEqual(self.diff(dict(self.BASE), "satin/c")[1], "undeclared")


# ---- the script itself, on a stand-in suite ----------------------------------------------------

# A stand-in for cargo: `metadata` succeeds, and `build` writes the stand-in implementation of the
# binary the manifest builds where cargo would (under $CARGO_BUILD_TARGET when it is set) and
# reports it as cargo does. FAKE_FAIL names a step that fails: metadata, mega-fixture or
# adapter-revm.
FAKE_CARGO = """#!/bin/sh
cmd=$1
shift
case "$cmd" in
    metadata) [ "${FAKE_FAIL:-}" = metadata ] && exit 101; exit 0 ;;
    build) ;;
    *) exit 99 ;;
esac
manifest=""
while [ $# -gt 0 ]; do
    case "$1" in --manifest-path) manifest=$2; shift ;; esac
    shift
done
case "$manifest" in
    */adapter-revm/Cargo.toml) name=adapter-revm ;;
    *) name=mega-fixture ;;
esac
if [ "${FAKE_FAIL:-}" = "$name" ]; then echo "error: could not compile $name" >&2; exit 101; fi
dir="$CARGO_TARGET_DIR/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug"
mkdir -p "$dir"
cp "$FAKE_IMPL/$name" "$dir/$name"
chmod +x "$dir/$name"
echo '{"reason":"compiler-artifact","target":{"name":"'"$name"'"},"executable":"'"$dir/$name"'"}'
echo '{"reason":"build-finished","success":true}'
"""

# The adapter a build of this tree produces: it says the cases fail when FAKE_VERDICT is fail.
FAKE_ADAPTER = """#!/bin/sh
case "$1" in
    capabilities) echo '{"adapter":"fresh"}' ;;
    verdict) echo "${FAKE_VERDICT:-pass}" ;;
esac
"""

# An adapter an earlier run left behind, which passes everything.
STALE_ADAPTER = """#!/bin/sh
case "$1" in
    capabilities) echo '{"adapter":"stale"}' ;;
    verdict) echo pass ;;
esac
"""

# The consumer: every case of the index passes, or fails when the adapter says so; the wrong
# expectation fails where the index records it, or at FAKE_CONTROL_PATH when that is set.
FAKE_CLI = """#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
assert args[0] == "consume"
adapter, report, path = args[args.index("--adapter") + 1], args[args.index("--report") + 1], Path(args[-1])
index = json.loads((path.parent / "index.json").read_text())
if path.name == "negative":
    status = "fail"
    if "--instrumented" in args:
        status = os.environ.get("FAKE_INSTRUMENTED_CONTROL", status)
        if status == "no-report":
            sys.exit(2)
    cases = [{"caseId": n["caseId"], "file": str(path / "wrong.json"), "fileDigest": "sha256:n", "status": status,
              "path": os.environ.get("FAKE_CONTROL_PATH") or n["wrongAt"]} for n in index["negative"]]
else:
    failing = subprocess.run([adapter, "verdict"], capture_output=True, text=True).stdout.strip() == "fail"
    cases = [{"caseId": c["caseId"], "fileDigest": c["fileDigest"], "observationsDigest": "sha256:o",
              "status": "fail" if failing else "pass", "path": "/expected/x", "expected": 1, "actual": 2,
              "rules": ["R-1"]} for c in index["cases"]]
Path(report).write_text(json.dumps({"cases": cases}))
passed = sum(c["status"] == "pass" for c in cases)
print(f"CONSUME adapter=fake cases={len(cases)} pass={passed} fail={len(cases) - passed} unsupported=0 error=0")
if path.name == "negative" and "--instrumented" in args and "FAKE_INSTRUMENTED_CONTROL_EXIT" in os.environ:
    sys.exit(int(os.environ["FAKE_INSTRUMENTED_CONTROL_EXIT"]))
sys.exit(0 if passed == len(cases) else 1)
"""


class Script(Case):
    """scripts/mega_eest.sh on a stand-in suite: its exit code and summary line for each way a run
    ends, under the system's bash (3.2 on macOS)."""

    def setUp(self) -> None:
        super().setUp()
        self.git_env = {"HOME": str(self.tmp), "GIT_CONFIG_NOSYSTEM": "1", "GIT_AUTHOR_NAME": "t",
                        "GIT_AUTHOR_EMAIL": "t@example.com", "GIT_COMMITTER_NAME": "t",
                        "GIT_COMMITTER_EMAIL": "t@example.com", "PATH": "/usr/bin:/bin"}
        # The tree: the two scripts, a pin file, and the manifests and lock the adapter needs.
        self.tree = self.tmp / "tree"
        (self.tree / "scripts").mkdir(parents=True)
        for name in ("mega_eest.sh", "mega_eest.py"):
            shutil.copy(SCRIPTS / name, self.tree / "scripts" / name)
        (self.tree / "crates" / "mega-evm").mkdir(parents=True)
        (self.tree / "Cargo.toml").write_text(ROOT_MANIFEST)
        (self.tree / "crates" / "mega-evm" / "Cargo.toml").write_text('[package]\nname = "mega-evm"\n')
        (self.tree / "Cargo.lock").write_text(lock(
            {"name": "mega-evm", "version": "2.0.0", "dependencies": ["revm"]},
            {"name": "revm", "version": "40.0.3", "source": FORK}))
        # The suite: a git repository whose default branch is main.
        self.suite = self.tmp / "suite"
        self.git("init", "-q", "-b", "main", str(self.suite))
        adapter = self.suite / "adapters" / "revm"
        (adapter / "src").mkdir(parents=True)
        (adapter / "Cargo.toml").write_text(ADAPTER_MANIFEST)
        (adapter / "src" / "main.rs").write_text("fn main() {}\n")
        (self.suite / "crates" / "mega-fixture-format").mkdir(parents=True)
        (self.suite / "crates" / "mega-fixture-format" / "Cargo.toml").write_text("[package]\n")
        (self.suite / "specs").mkdir()
        (self.suite / "specs" / "satin.json").write_text("{}\n")
        (self.suite / "Cargo.toml").write_text("[workspace]\n")
        (self.suite / "Cargo.lock").write_text("version = 4\n")
        self.rev = self.commit_cases(["satin/a", "satin/b", "satin/c"])
        # The stand-ins.
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        (self.bin / "python3").symlink_to(sys.executable)
        impl = self.tmp / "impl"
        impl.mkdir()
        for path, text in ((self.bin / "cargo", FAKE_CARGO), (impl / "adapter-revm", FAKE_ADAPTER),
                           (impl / "mega-fixture", FAKE_CLI)):
            path.write_text(text)
            path.chmod(0o755)
        self.impl = impl

    def git(self, *args: str, cwd: Path | None = None) -> str:
        return subprocess.run(["git", *args], cwd=cwd, env=self.git_env, check=True,
                              capture_output=True, text=True).stdout.strip()

    def commit_cases(self, ids: list[str], branch: str | None = None) -> str:
        """Commit a slice holding `ids` (on `branch`, a new branch off the current commit, when
        given) and return the commit; main stays checked out."""
        if branch:
            self.git("checkout", "-q", "-b", branch, cwd=self.suite)
        slice_dir = self.suite / "slice"
        shutil.rmtree(slice_dir, ignore_errors=True)
        (slice_dir / "cases").mkdir(parents=True)
        (slice_dir / "negative").mkdir()
        for case_id in ids:
            (slice_dir / "cases" / f"{case_id.split('/')[-1]}.json").write_text("{}\n")
        (slice_dir / "negative" / "wrong.json").write_text("{}\n")
        (slice_dir / "index.json").write_text(json.dumps({
            "cases": [{"caseId": c, "fileDigest": f"sha256:{c}"} for c in ids],
            "negative": [{"caseId": "satin/negative/wrong", "file": "negative/wrong.json",
                          "wrongAt": "/expected/x"}]}))
        self.git("add", "-A", cwd=self.suite)
        self.git("commit", "-q", "-m", "cases", cwd=self.suite)
        rev = self.git("rev-parse", "HEAD", cwd=self.suite)
        if branch:
            self.git("checkout", "-q", "main", cwd=self.suite)
        return rev

    def run_script(self, rev: str | None = None, declared: str | None = None, changed: str | None = None,
                   moved_from: str | None = None, **env: str) -> tuple[int, str, str]:
        pin = [f"MEGA_EEST_REPO=file://{self.suite}", f"MEGA_EEST_REV={rev or self.rev}"]
        if moved_from is not None:
            pin.append(f"MEGA_EEST_REPO_MOVED_FROM={moved_from}")
        if declared is not None:
            pin.append(f"MEGA_EEST_REMOVED_CASES={declared}")
        if changed is not None:
            pin.append(f"MEGA_EEST_CHANGED_CASES={changed}")
        (self.tree / "scripts" / "mega_eest.env").write_text("\n".join(pin) + "\n")
        run = subprocess.run(
            ["bash", str(self.tree / "scripts" / "mega_eest.sh")], capture_output=True, text=True, timeout=120,
            env={**self.git_env, "PATH": f"{self.bin}:/usr/bin:/bin", "FAKE_IMPL": str(self.impl),
                 "MEGA_EEST_JOBS": "1", **env})
        return run.returncode, run.stdout, run.stderr

    def summary(self, out: str) -> str:
        lines = [line for line in out.splitlines() if line.startswith("MEGA-EEST ")]
        self.assertEqual(len(lines), 1, out)
        return lines[0]

    def test_a_full_run_that_passes_exits_0(self) -> None:
        code, out, err = self.run_script()
        self.assertEqual(code, 0, err)
        line = self.summary(out)
        self.assertRegex(line, r"^MEGA-EEST rev=[0-9a-f]{40} run=full cases=3 plain=pass:3,.* "
                               r"instrumented=pass:3,.* provenance=default-branch repo=unchecked caseset=unchecked result=pass$")
        self.assertEqual(json.loads((self.tree / "target/mega-eest/capabilities.json").read_text()),
                         {"adapter": "fresh"})

    def test_failing_cases_are_the_only_exit_1(self) -> None:
        code, out, err = self.run_script(FAKE_VERDICT="fail", MEGA_EEST_MODES="plain")
        self.assertEqual(code, 1, err)
        self.assertRegex(self.summary(out), r" run=partial cases=3 plain=pass:0,fail:3,.* result=FAIL$")

    def test_every_requested_mode_needs_a_failing_control(self) -> None:
        for modes in ("plain instrumented", "instrumented"):
            for gating in ("pass", "fail"):
                for control in ("no-report", "unsupported", "pass"):
                    with self.subTest(modes=modes, gating=gating, control=control):
                        code, out, err = self.run_script(MEGA_EEST_MODES=modes, FAKE_VERDICT=gating,
                                                         FAKE_INSTRUMENTED_CONTROL=control)
                        self.assertEqual(code, 2, (out, err))
                        self.assertRegex(self.summary(out), r" result=INCOMPLETE$")
                        self.assertIn("instrumented: negative control", err)

    def test_a_control_report_cannot_mask_the_consumer_exit(self) -> None:
        for gating in ("pass", "fail"):
            for exit_code in (0, 2, 101):
                with self.subTest(gating=gating, exit_code=exit_code):
                    code, out, err = self.run_script(FAKE_VERDICT=gating,
                                                     FAKE_INSTRUMENTED_CONTROL_EXIT=str(exit_code))
                    self.assertEqual(code, 2, (out, err))
                    self.assertRegex(self.summary(out), r" negative=not-caught .* result=INCOMPLETE$")
                    self.assertIn(f"instrumented: negative controls: `mega-fixture consume` exited {exit_code}", err)

    def test_failing_cases_that_move_the_control_are_still_exit_1(self) -> None:
        # A change that fails the cases can also move where the control first differs.
        code, out, err = self.run_script(FAKE_VERDICT="fail", FAKE_CONTROL_PATH="/expected/gasLedgers/gasUsed")
        self.assertEqual(code, 1, err)
        self.assertRegex(self.summary(out), r" negative=fail-elsewhere .* result=FAIL$")
        # With every case passing, the control must fail where recorded.
        code, out, err = self.run_script(FAKE_CONTROL_PATH="/expected/gasLedgers/gasUsed")
        self.assertEqual(code, 2, err)
        self.assertRegex(self.summary(out), r" negative=not-caught .* result=INCOMPLETE$")

    def test_a_step_that_fails_before_the_verdict_exits_2(self) -> None:
        for fail, stage in (("mega-fixture", "cli-build"), ("metadata", "adapter-resolve"),
                            ("adapter-revm", "adapter-build")):
            # Whatever the cases would have done.
            code, out, err = self.run_script(FAKE_FAIL=fail, FAKE_VERDICT="fail")
            self.assertEqual(code, 2, (fail, err))
            self.assertRegex(self.summary(out), rf" stage={stage} result=INCOMPLETE$")

    def test_a_stale_binary_is_never_run(self) -> None:
        # An earlier run left a passing adapter where a default build puts it; this build goes
        # under a build target, and its adapter fails the cases.
        self.assertEqual(self.run_script()[0], 0)
        stale = self.tree / "target/mega-eest/target/debug/adapter-revm"
        stale.write_text(STALE_ADAPTER)
        code, out, err = self.run_script(CARGO_BUILD_TARGET="aarch64-apple-darwin", FAKE_VERDICT="fail")
        self.assertEqual(code, 1, err)
        self.assertIn("result=FAIL", self.summary(out))
        built = self.tree / "target/mega-eest/target/aarch64-apple-darwin/debug/adapter-revm"
        self.assertEqual((self.tree / "target/mega-eest/adapter.path").read_text().strip(), str(built))

    def test_no_mode_exits_2(self) -> None:
        for modes in (" ", "\t\n"):
            code, out, err = self.run_script(MEGA_EEST_MODES=modes)
            self.assertEqual(code, 2, err)
            self.assertIn("names no mode", err)
            self.assertIn("result=INCOMPLETE", self.summary(out))

    def test_a_checkout_with_hidden_changes_is_refused(self) -> None:
        for flag in ("--skip-worktree", "--assume-unchanged"):
            self.git("update-index", flag, "slice/index.json", cwd=self.suite)
            code, out, err = self.run_script(MEGA_EEST_DIR=str(self.suite), MEGA_EEST_SKIP_PROVENANCE="1")
            self.assertEqual(code, 2, err)
            self.assertIn("skip-worktree or assume-unchanged", err)
            self.assertIn("stage=fetch result=INCOMPLETE", self.summary(out))
            self.git("update-index", flag.replace("--", "--no-"), "slice/index.json", cwd=self.suite)
        self.assertEqual(self.run_script(MEGA_EEST_DIR=str(self.suite))[0], 0)

    def test_a_checkout_git_cannot_read_is_refused(self) -> None:
        # HEAD still resolves, but `git status` fails and prints nothing, which is not clean.
        (self.suite / ".git" / "index").write_bytes(b"not an index")
        code, out, err = self.run_script(MEGA_EEST_DIR=str(self.suite), MEGA_EEST_SKIP_PROVENANCE="1")
        self.assertEqual(code, 2, err)
        self.assertIn("cannot be read by git status", err)
        self.assertIn("stage=fetch result=INCOMPLETE", self.summary(out))

    def test_a_pin_off_the_default_branch_is_refused(self) -> None:
        draft = self.commit_cases(["satin/a", "satin/b", "satin/c", "satin/d"], branch="draft")
        code, out, err = self.run_script(rev=draft)
        self.assertEqual(code, 3, err)
        self.assertIn("draft state", err)
        self.assertRegex(self.summary(out), r" cases=4 .* provenance=unmerged repo=unchecked caseset=unchecked result=REFUSED$")
        # Once merged, the same pin passes.
        self.git("merge", "-q", "--ff-only", "draft", cwd=self.suite)
        code, out, err = self.run_script(rev=draft)
        self.assertEqual(code, 0, err)
        self.assertIn("provenance=default-branch", self.summary(out))

    def commit_digests(self, digests: dict[str, str]) -> str:
        """Commit an index whose cases carry the given file digests (the files stay as they are)."""
        index = json.loads((self.suite / "slice" / "index.json").read_text())
        for case in index["cases"]:
            case["fileDigest"] = digests.get(case["caseId"], case["fileDigest"])
        (self.suite / "slice" / "index.json").write_text(json.dumps(index))
        self.git("commit", "-qam", "change a case", cwd=self.suite)
        return self.git("rev-parse", "HEAD", cwd=self.suite)

    def test_a_pin_move_must_declare_the_cases_it_changes(self) -> None:
        base = self.rev
        head = self.commit_digests({"satin/b": "sha256:rewritten"})
        code, out, err = self.run_script(rev=head, MEGA_EEST_BASE_REV=base)
        self.assertEqual(code, 3, err)
        self.assertIn("changed and not declared: satin/b", err)
        self.assertIn("caseset=undeclared result=REFUSED", self.summary(out))
        self.assertIn("<summary>Changed (1)</summary>", (self.tree / "target/mega-eest/case-diff.md").read_text())
        (self.tree / "scripts" / "mega_eest.env").write_text("")  # rewritten by run_script
        code, out, err = self.run_script(rev=head, changed="satin/b", MEGA_EEST_BASE_REV=base)
        self.assertEqual(code, 0, err)
        self.assertIn("caseset=declared result=pass", self.summary(out))

    def mirror(self) -> str:
        """Another repository holding the suite's history: the base's, for a move from it."""
        mirror = self.tmp / "mirror"
        if not mirror.exists():
            self.git("clone", "-q", str(self.suite), str(mirror))
        return f"file://{mirror}"

    def test_a_pin_move_to_another_repository_is_refused(self) -> None:
        mirror = self.mirror()
        code, out, err = self.run_script(MEGA_EEST_BASE_REV=self.rev, MEGA_EEST_BASE_REPO=mirror)
        self.assertEqual(code, 3, err)
        self.assertIn("MEGA_EEST_REPO moves from", err)
        self.assertIn("repo=changed", self.summary(out))
        self.assertIn("the pin is refused: MEGA_EEST_REPO is not the base's", err)
        code, out, err = self.run_script(MEGA_EEST_BASE_REV=self.rev, MEGA_EEST_BASE_REPO=f"file://{self.suite}")
        self.assertEqual(code, 0, err)
        self.assertIn("repo=same caseset=unmoved result=pass", self.summary(out))

    def test_a_declared_repository_move_passes(self) -> None:
        mirror = self.mirror()
        code, out, err = self.run_script(moved_from=mirror, MEGA_EEST_BASE_REV=self.rev, MEGA_EEST_BASE_REPO=mirror)
        self.assertEqual(code, 0, err)
        self.assertIn("as MEGA_EEST_REPO_MOVED_FROM declares", err)
        self.assertIn("provenance=default-branch repo=moved caseset=unmoved result=pass", self.summary(out))

    def test_a_declared_move_from_another_repository_than_the_bases_is_refused(self) -> None:
        mirror = self.mirror()
        code, out, err = self.run_script(moved_from=f"{mirror}-elsewhere", MEGA_EEST_BASE_REV=self.rev,
                                         MEGA_EEST_BASE_REPO=mirror)
        self.assertEqual(code, 3, err)
        self.assertIn(f"is '{mirror}-elsewhere', not the base's repository", err)
        self.assertIn("repo=changed caseset=kept result=REFUSED", self.summary(out))

    def test_a_declared_move_whose_pin_is_off_the_new_default_branch_is_refused(self) -> None:
        mirror = self.mirror()
        off = self.commit_cases(["satin/a", "satin/b", "satin/c", "satin/d"], branch="draft")
        code, out, err = self.run_script(rev=off, moved_from=mirror, MEGA_EEST_BASE_REV=self.rev,
                                         MEGA_EEST_BASE_REPO=mirror)
        self.assertEqual(code, 3, err)
        self.assertIn("provenance=unmerged repo=moved", self.summary(out))
        self.assertIn("the pin is refused: the pin is not on the default branch", err)

    def test_a_move_declaration_without_a_move_is_stale(self) -> None:
        mirror = self.mirror()
        code, out, err = self.run_script(moved_from=mirror, MEGA_EEST_BASE_REV=self.rev,
                                         MEGA_EEST_BASE_REPO=f"file://{self.suite}")
        self.assertEqual(code, 3, err)
        self.assertIn(f"declares a move from {mirror}", err)
        self.assertIn("repo=stale caseset=unmoved result=REFUSED", self.summary(out))
        self.assertIn("(repo=stale)", err)
        # The declaration a merged move left in the base only warns, until it is cleared.
        code, out, err = self.run_script(moved_from=mirror, MEGA_EEST_BASE_REV=self.rev,
                                         MEGA_EEST_BASE_REPO=f"file://{self.suite}",
                                         MEGA_EEST_BASE_REPO_MOVED_FROM=mirror)
        self.assertEqual(code, 0, err)
        self.assertIn("still declares the last move", err)
        self.assertIn("repo=same caseset=unmoved result=pass", self.summary(out))

    def test_a_pin_move_must_declare_the_cases_it_removes(self) -> None:
        base = self.rev
        head = self.commit_cases(["satin/a", "satin/b", "satin/e"])
        code, out, err = self.run_script(rev=head, MEGA_EEST_BASE_REV=base)
        self.assertEqual(code, 3, err)
        self.assertIn("removed and not declared: satin/c", err)
        self.assertIn("caseset=undeclared result=REFUSED", self.summary(out))
        self.assertIn("- `satin/c`", (self.tree / "target/mega-eest/case-diff.md").read_text())
        code, out, err = self.run_script(rev=head, declared="satin/c", MEGA_EEST_BASE_REV=base)
        self.assertEqual(code, 0, err)
        self.assertIn("caseset=declared result=pass", self.summary(out))
        code, out, err = self.run_script(rev=head, MEGA_EEST_BASE_REV=head)
        self.assertEqual(code, 0, err)
        self.assertIn("caseset=unmoved", self.summary(out))


if __name__ == "__main__":
    unittest.main()
