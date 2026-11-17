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
import sys
import tempfile
import tomllib
import unittest
from argparse import Namespace
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent


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
    """The run reports decide the gate."""

    CASES = [("satin/a", "sha256:1"), ("satin/b", "sha256:2")]

    def write(self, reports: dict[str, list[dict] | None], negative: list[dict] | None = None,
              exits: dict[str, int] | None = None) -> tuple[int, str, str]:
        # A fresh pair for every call: a test may write several runs.
        run = Path(tempfile.mkdtemp(dir=self.tmp))
        suite, work = run / "suite", run / "work"
        (suite / "slice").mkdir(parents=True)
        work.mkdir()
        (suite / "slice" / "index.json").write_text(json.dumps({
            "cases": [{"caseId": c, "fileDigest": d} for c, d in self.CASES],
            "negative": [{"caseId": "satin/negative/wrong", "wrongAt": "/expected/x"}],
        }))
        for mode, cases in reports.items():
            if cases is not None:
                (work / f"{mode}.json").write_text(json.dumps({"cases": cases}))
            (work / f"{mode}.exit").write_text(f"{(exits or {}).get(mode, 0)}\n")
        if negative is not None:
            (work / "negative.json").write_text(json.dumps({"cases": negative}))
        return self.run_quietly(eest.cmd_check, Namespace(
            suite=str(suite), work=str(work), modes=" ".join(reports), rev="f" * 40))

    @staticmethod
    def passed(case_id: str, digest: str, observations: str = "sha256:o") -> dict:
        return {"caseId": case_id, "fileDigest": digest, "status": "pass", "observationsDigest": observations,
                "file": f"{case_id}.json"}

    NEGATIVE = [{"caseId": "satin/negative/wrong", "status": "fail", "path": "/expected/x"}]

    def all_passed(self) -> list[dict]:
        return [self.passed(c, d) for c, d in self.CASES]

    def test_every_case_passing_in_both_modes_passes(self) -> None:
        code, out, err = self.write({"plain": self.all_passed(), "instrumented": self.all_passed()}, self.NEGATIVE)
        self.assertEqual(code, 0, err)
        self.assertEqual(out.strip(), f"MEGA-EEST rev={'f' * 40} cases=2 plain=pass:2,fail:0,unsupported:0,error:0 "
                         "instrumented=pass:2,fail:0,unsupported:0,error:0 parity=identical "
                         "negative=fail-as-recorded result=pass")

    def test_a_failing_case_is_named_with_its_difference(self) -> None:
        failed = {"caseId": "satin/b", "fileDigest": "sha256:2", "status": "fail", "path": "/expected/gasLedgers/state",
                  "expected": "0x1", "actual": "0x2", "rules": ["R-1"], "file": "b.json"}
        code, out, err = self.write({"plain": [self.passed("satin/a", "sha256:1"), failed]}, self.NEGATIVE)
        self.assertEqual(code, 1)
        self.assertIn("result=FAIL", out)
        self.assertIn("plain=pass:1,fail:1", out)
        self.assertIn("fail satin/b\n  first difference at /expected/gasLedgers/state: expected \"0x1\" actual \"0x2\"", err)
        self.assertIn("rules: R-1", err)

    def test_cases_that_are_not_the_index_fail(self) -> None:
        for cases in ([self.passed("satin/a", "sha256:1")],
                      [self.passed("satin/a", "sha256:1"), self.passed("satin/b", "sha256:other")],
                      []):
            code, out, err = self.write({"plain": cases}, self.NEGATIVE)
            self.assertEqual(code, 1, cases)
            self.assertIn("the cases consumed are not the slice's index", err)

    def test_a_mode_without_a_report_fails(self) -> None:
        code, out, err = self.write({"plain": self.all_passed(), "instrumented": None}, self.NEGATIVE,
                                    exits={"instrumented": 2})
        self.assertEqual(code, 1)
        self.assertIn("instrumented=no-report", out)
        self.assertIn("wrote no report (exit 2)", err)

    def test_observations_that_differ_between_modes_fail(self) -> None:
        other = [self.passed("satin/a", "sha256:1"), self.passed("satin/b", "sha256:2", "sha256:other")]
        code, out, err = self.write({"plain": self.all_passed(), "instrumented": other}, self.NEGATIVE)
        self.assertEqual(code, 1)
        self.assertIn("parity=1-differ", out)

    def test_a_wrong_expectation_that_is_not_caught_fails(self) -> None:
        for negative in (None, [{"caseId": "satin/negative/wrong", "status": "pass"}],
                         [{"caseId": "satin/negative/wrong", "status": "fail", "path": "/expected/y"}]):
            code, out, err = self.write({"plain": self.all_passed()}, negative)
            self.assertEqual(code, 1, negative)
            self.assertNotIn("negative=fail-as-recorded", out)

    def test_a_nonzero_consume_exit_fails_even_when_every_case_passed(self) -> None:
        code, out, _ = self.write({"plain": self.all_passed()}, self.NEGATIVE, exits={"plain": 1})
        self.assertEqual(code, 1)
        self.assertIn("result=FAIL", out)


if __name__ == "__main__":
    unittest.main()
