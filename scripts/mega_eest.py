#!/usr/bin/env python3
"""The parts of scripts/mega_eest.sh that read and write structured files.

Subcommands:

  adapter     --tree <mega-evm checkout> --suite <Mega EEST checkout> --out <dir>
              [--engine-rustflags <flags>]
        Write the Mega EEST revm adapter as a workspace of its own at <dir>, built against the
        mega-evm checkout rather than the commit the adapter pins. Its sources are copied as they
        are; its manifest is the adapter's with three changes. A dependency on a package of this
        workspace (mega-evm) is a path dependency on that package here. A dependency this
        workspace's root manifest declares in `[workspace.dependencies]` takes its source (version,
        git, rev, tag, branch) from there, keeping the adapter's features, so the adapter names the
        crates mega-evm links rather than a second copy of them. And the `[patch]` and `[profile]`
        tables are this workspace's root manifest's, replacing the adapter's, so a change that moves
        the revm or op-revm fork is built with the fork it moves to. A path dependency outside this
        workspace (the format crate) stays a path dependency on the suite's checkout. The adapter's
        own Cargo.lock, resolved for the commit it pins, is not used: the workspace's lock starts as
        this checkout's Cargo.lock, which cargo then prunes to what the adapter links and extends
        with anything only the adapter needs. `--engine-rustflags` adds compiler flags to this
        workspace's packages and the adapter's own crate alone (cargo's `profile-rustflags`, which
        the pinned nightly has), so a coverage build instruments mega-evm and the instances of its
        generic code the adapter compiles, and not the crates.io and git crates it links.

  check-lock  --tree <mega-evm checkout> --adapter <dir>
        After cargo resolved the adapter workspace: every package mega-evm links in it, through
        any number of dependencies, must be a package this checkout's Cargo.lock holds, at the same
        version and from the same source, and there is exactly one revm. Packages only the adapter
        or the format crate links are listed when this checkout does not lock them.

  executable  --name <target name>
        Read the JSON messages of `cargo build --message-format=json…` on standard input and print
        the path of the one executable the build reported for that target, which must exist: the
        file this build produced, wherever the target directory or the build target put it.

  case-diff   --base-rev <commit> --rev <commit> --base-index <index.json> --index <index.json>
              --declared <case ids> --summary <file>
        Compare the case sets of two pins' slice indexes: the case ids removed, added and changed
        (same id, another file digest) are listed on stderr and as Markdown in <file>, and the word
        printed on stdout says whether the removals are what scripts/mega_eest.env's
        MEGA_EEST_REMOVED_CASES declares, exactly: `kept` (none removed, none declared), `declared`
        (the removed ids and no other) or `undeclared` (anything else).

  check       --suite <Mega EEST checkout> --work <dir> --modes <"plain instrumented"> --rev <commit>
              [--provenance <default-branch|unmerged|unchecked>]
              [--caseset <unmoved|kept|declared|undeclared|unchecked>]
        Read the run reports `mega-fixture consume` wrote into <dir> (<mode>.json, and
        negative.json for the suite's deliberately wrong expectation) and decide: in every mode, the
        cases consumed are exactly the slice's index, by case id and file digest, and every one
        passed; with both modes, every case's observations are the same bytes plain and
        instrumented. The wrong expectation, which the index must list, is the control that shows
        the consumer and the adapter can report a failure at all: it decides only when every
        gating case passed, and then it must fail at the path it records; when gating cases
        failed, the failures already show that, and the control need only have run and not passed
        (failed or erred, at any path), since a change of behaviour can move where it first
        differs. Prints the failures (case id, first differing path, both values, the rules the
        case pins) and one summary line, `MEGA-EEST … result=<result>`, and exits with its code:
        0 pass; 1 FAIL, a verdict (complete reports of exactly the index in every mode, a case
        failed or erred, or the modes' observations differ, and the control ran and did not pass);
        2 INCOMPLETE, no verdict (no mode, a report missing, cases that are not the index, a
        consume exit its report does not explain, a case the adapter does not support, no control,
        a control not run or passing, a control that failed elsewhere while every gating case
        passed, or the checker itself failing); 3 REFUSED, every case passed but the pin's
        provenance is `unmerged` or its case set `undeclared`. `run=full` in the line is both
        modes; `run=partial` one.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from collections import Counter
from pathlib import Path

# tomllib is new in Python 3.11: say so rather than fail on the import below.
if sys.version_info < (3, 11):
    sys.exit(f"scripts/mega_eest.py needs Python 3.11 or newer, for tomllib; this is Python "
             f"{sys.version.split()[0]}.")

import tomllib  # noqa: E402

# The keys of a dependency that say where it comes from; the others (features, default-features,
# optional, package) say how it is used.
SOURCE_KEYS = ("version", "git", "rev", "tag", "branch", "registry", "path")

# The adapter files that are the adapter's resolution and toolchain, which the generated workspace
# replaces with this checkout's: the manifest is rewritten, the lock is seeded from this checkout's,
# and cargo runs from this checkout, so rustup picks its rust-toolchain.toml.
NOT_COPIED = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "target"}

# Where the generated workspace records what it seeded its lock from.
SEED_MARKER = ".mega-eest-seed"

# How many failures of one mode are printed in full; the report holds them all.
MAX_SHOWN = 20


class GateError(Exception):
    """A condition the gate cannot pass."""


# ---- TOML output ----------------------------------------------------------------------------

BARE_KEY = re.compile(r"^[A-Za-z0-9_-]+$")


def toml_key(key: str) -> str:
    return key if BARE_KEY.match(key) else json.dumps(key, ensure_ascii=False)


def toml_value(value) -> str:
    # bool first: a bool is an int in Python.
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, str):
        # A JSON string without ASCII escaping is a TOML basic string: the escapes JSON writes
        # (\", \\, \n, \t, \uXXXX for control characters) are TOML's.
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{toml_key(k)} = {toml_value(v)}" for k, v in value.items()) + " }"
    raise GateError(f"cannot write {value!r} as TOML")


def render_toml(doc: dict) -> str:
    """A TOML document for `doc`: tables of the top two levels as headers, leaves inline.

    The text is read back and compared with `doc`, so a value this writer gets wrong fails here
    rather than as a manifest that means something else.
    """
    out: list[str] = []

    def is_leaf(value: dict) -> bool:
        return not any(isinstance(v, dict) for v in value.values())

    def walk(path: list[str], table: dict) -> None:
        depth = len(path)
        plain, tables, arrays = [], [], []
        for key, value in table.items():
            if isinstance(value, dict) and not (depth + 1 >= 2 and is_leaf(value)):
                tables.append((key, value))
            elif isinstance(value, list) and value and all(isinstance(v, dict) for v in value):
                arrays.append((key, value))
            else:
                plain.append((key, value))
        if path and (plain or not (tables or arrays)):
            out.append("[" + ".".join(toml_key(p) for p in path) + "]")
        for key, value in plain:
            out.append(f"{toml_key(key)} = {toml_value(value)}")
        if plain or (path and not (tables or arrays)):
            out.append("")
        for key, value in tables:
            walk([*path, key], value)
        for key, items in arrays:
            for item in items:
                out.append("[[" + ".".join(toml_key(p) for p in [*path, key]) + "]]")
                for k, v in item.items():
                    out.append(f"{toml_key(k)} = {toml_value(v)}")
                out.append("")

    walk([], doc)
    text = "\n".join(out).rstrip("\n") + "\n"
    if tomllib.loads(text) != doc:
        raise GateError("the generated manifest does not read back as the manifest meant")
    return text


# ---- adapter --------------------------------------------------------------------------------


def workspace_members(tree: Path, root: dict) -> dict[str, Path]:
    """The packages of this workspace, by name, with their directories."""
    members: dict[str, Path] = {}
    for member in root.get("workspace", {}).get("members", []):
        if any(c in member for c in "*?["):
            raise GateError(f"workspace member {member!r} is a glob, which this script does not expand")
        manifest = tree / member / "Cargo.toml"
        name = tomllib.loads(manifest.read_text())["package"]["name"]
        members[name] = (tree / member).resolve()
    return members


def rewrite_dependency(name: str, spec, *, adapter_dir: Path, members: dict[str, Path],
                       workspace_deps: dict) -> tuple[object, str]:
    """The adapter's dependency `name` as the generated workspace declares it, and why."""
    if isinstance(spec, str):
        spec = {"version": spec}
    if not isinstance(spec, dict):
        raise GateError(f"dependency {name!r} is neither a version nor a table: {spec!r}")
    if spec.get("workspace"):
        raise GateError(f"dependency {name!r} inherits from the adapter's workspace, which has none")
    package = spec.get("package", name)
    usage = {k: v for k, v in spec.items() if k not in SOURCE_KEYS}
    if package in members:
        return {"path": str(members[package]), **usage}, "this checkout's package"
    if name in workspace_deps:
        root_spec = workspace_deps[name]
        if isinstance(root_spec, str):
            root_spec = {"version": root_spec}
        if "path" not in root_spec and root_spec.get("package", name) == package:
            source = {k: v for k, v in root_spec.items() if k in SOURCE_KEYS}
            return {**source, **usage}, "the source this checkout's root manifest declares"
    if "path" in spec:
        return {**spec, "path": str((adapter_dir / spec["path"]).resolve())}, "the suite's own path"
    return spec, "the adapter's own declaration"


def write_if_changed(path: Path, data: bytes) -> None:
    """Write `data` unless the file holds it already, so cargo's fingerprints see no change."""
    if path.exists() and path.read_bytes() == data:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def sync_tree(src: Path, dst: Path, keep: set[str]) -> None:
    """Make `dst` hold `src`'s files but those named in NOT_COPIED; `keep` names files of `dst`
    that are not `src`'s and stay."""
    wanted: set[Path] = set()
    for path in sorted(src.rglob("*")):
        rel = path.relative_to(src)
        if rel.parts[0] in NOT_COPIED or not path.is_file():
            continue
        wanted.add(rel)
        write_if_changed(dst / rel, path.read_bytes())
    for path in sorted(dst.rglob("*"), reverse=True):
        rel = path.relative_to(dst)
        if rel.parts[0] in keep or rel.parts[0] in NOT_COPIED:
            continue
        if path.is_file() and rel not in wanted:
            path.unlink()
        elif path.is_dir() and not any(path.iterdir()):
            path.rmdir()


def cmd_adapter(args: argparse.Namespace) -> int:
    tree, suite, out = Path(args.tree).resolve(), Path(args.suite).resolve(), Path(args.out)
    adapter_dir = suite / "adapters" / "revm"
    adapter = tomllib.loads((adapter_dir / "Cargo.toml").read_text())
    root = tomllib.loads((tree / "Cargo.toml").read_text())
    members = workspace_members(tree, root)
    workspace_deps = root.get("workspace", {}).get("dependencies", {})

    manifest: dict = {}
    for key, value in adapter.items():
        if key in ("patch", "profile", "replace"):
            continue
        manifest[key] = value
    if "workspace" not in manifest:
        raise GateError(f"{adapter_dir / 'Cargo.toml'} is no workspace of its own")
    routed = Counter()
    tables = [("dependencies",), ("build-dependencies",), ("dev-dependencies",)]
    tables += [("target", t, d) for t in adapter.get("target", {})
               for d in ("dependencies", "build-dependencies", "dev-dependencies")]
    for path in tables:
        table = manifest
        for part in path[:-1]:
            table = table.get(part, {})
        deps = table.get(path[-1])
        if not deps:
            continue
        rewritten = {}
        for name, spec in deps.items():
            rewritten[name], why = rewrite_dependency(
                name, spec, adapter_dir=adapter_dir, members=members, workspace_deps=workspace_deps)
            routed[why] += 1
        table[path[-1]] = rewritten
    if not routed["this checkout's package"]:
        raise GateError(
            f"{adapter_dir / 'Cargo.toml'} depends on no package of this checkout "
            f"({', '.join(sorted(members))}): it would not test this tree")
    for key in ("patch", "profile"):
        if key in root:
            manifest[key] = json.loads(json.dumps(root[key]))
    flags = args.engine_rustflags.split()
    if flags:
        manifest["cargo-features"] = ["profile-rustflags"]
        overrides = manifest.setdefault("profile", {}).setdefault("dev", {}).setdefault("package", {})
        # The adapter's own crate too: mega-evm's generic code is compiled where it is used.
        overrides.setdefault(manifest["package"]["name"], {})["rustflags"] = flags
        for name, spec in manifest["dependencies"].items():
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if package in members:
                overrides.setdefault(package, {})["rustflags"] = flags

    out.mkdir(parents=True, exist_ok=True)
    sync_tree(adapter_dir, out, keep={SEED_MARKER})
    text = render_toml(manifest)
    header = (
        "# Generated by scripts/mega_eest.py from the Mega EEST revm adapter's manifest: built\n"
        "# against the mega-evm checkout below, with its root manifest's [patch] and [profile].\n"
        f"# mega-evm checkout: {tree}\n# Mega EEST checkout: {suite}\n\n"
    )
    write_if_changed(out / "Cargo.toml", (header + text).encode())

    # Seed the lock from this checkout's, unless it was seeded from the same lock and manifest
    # before: cargo has resolved it since, and a fresh seed would only make it resolve again.
    # The digest names the checkouts' paths by placeholders, so a copy of this tree (a mutation
    # build directory, which carries the warm-up's resolved workspace) keeps the resolved lock.
    tree_lock = (tree / "Cargo.lock").read_bytes()
    placed = text.replace(str(tree), "<mega-evm>").replace(str(suite), "<mega-eest>")
    seed = hashlib.sha256(tree_lock + b"\0" + placed.encode()).hexdigest()
    marker = out / SEED_MARKER
    if not (out / "Cargo.lock").exists() or not marker.exists() or marker.read_text().strip() != seed:
        write_if_changed(out / "Cargo.lock", tree_lock)
        marker.write_text(seed + "\n")
    print(
        "mega-eest: adapter workspace at {}: {}".format(
            out, ", ".join(f"{n} {why}" for why, n in sorted(routed.items()))),
        file=sys.stderr,
    )
    return 0


# ---- check-lock -----------------------------------------------------------------------------


def lock_packages(path: Path) -> list[dict]:
    return tomllib.loads(path.read_text()).get("package", [])


def resolve_reference(ref: str, packages: list[dict]) -> dict:
    """The package a lock entry's dependency string names: `name`, `name version` or
    `name version (source)`."""
    m = re.fullmatch(r"(\S+)(?: (\S+))?(?: \((.+)\))?", ref)
    if not m:
        raise GateError(f"cannot read the lock reference {ref!r}")
    name, version, source = m.groups()
    found = [p for p in packages if p["name"] == name
             and (version is None or p["version"] == version)
             and (source is None or p.get("source") == source)]
    if len(found) != 1:
        raise GateError(f"the lock reference {ref!r} names {len(found)} packages, not one")
    return found[0]


def cmd_check_lock(args: argparse.Namespace) -> int:
    tree, adapter = Path(args.tree).resolve(), Path(args.adapter)
    ours = lock_packages(tree / "Cargo.lock")
    locked = {(p["name"], p["version"], p.get("source")) for p in ours}
    member_names = {p["name"] for p in ours if "source" not in p}
    theirs = lock_packages(adapter / "Cargo.lock")
    roots = [p for p in theirs if p["name"] == "mega-evm" and "source" not in p]
    if len(roots) != 1:
        raise GateError(f"{adapter / 'Cargo.lock'} holds {len(roots)} path packages named mega-evm, not one")
    seen: dict[tuple, dict] = {}
    queue = list(roots)
    while queue:
        pkg = queue.pop()
        key = (pkg["name"], pkg["version"], pkg.get("source"))
        if key in seen:
            continue
        seen[key] = pkg
        queue.extend(resolve_reference(ref, theirs) for ref in pkg.get("dependencies", []))
    differing = sorted(k for k in seen if k not in locked)
    foreign_paths = sorted(k[0] for k in seen if k[2] is None and k[0] not in member_names)
    revms = sorted(f"{p['version']} {p.get('source', '(path)')}" for p in theirs if p["name"] == "revm")
    problems = []
    if differing:
        problems.append("mega-evm links packages this checkout's Cargo.lock does not lock: "
                        + ", ".join(f"{n} {v} {s or '(path)'}" for n, v, s in differing))
    if foreign_paths:
        problems.append("mega-evm links path packages that are not this checkout's: " + ", ".join(foreign_paths))
    if len(revms) != 1:
        problems.append(f"the adapter workspace resolves {len(revms)} revm packages, not one: " + "; ".join(revms))
    if problems:
        for p in problems:
            print(f"mega-eest: error: {p}", file=sys.stderr)
        print("mega-eest: error: the adapter would not build mega-evm as this checkout locks it",
              file=sys.stderr)
        return 1
    extra = sorted({(p["name"], p["version"]) for p in theirs
                    if (p["name"], p["version"], p.get("source")) not in locked and "source" in p})
    print(f"mega-eest: lock: mega-evm links {len(seen)} packages, each as this checkout locks it; "
          f"one revm ({revms[0]})", file=sys.stderr)
    if extra:
        print("mega-eest: lock: resolved outside this checkout's lock, for the adapter alone: "
              + ", ".join(f"{n} {v}" for n, v in extra), file=sys.stderr)
    return 0


# ---- check ----------------------------------------------------------------------------------


def canonical(value) -> str:
    return "(absent)" if value is None else json.dumps(value, sort_keys=True, separators=(",", ":"))


# The results of `check`, by the exit code each has.
RESULT_EXIT = {"pass": 0, "FAIL": 1, "INCOMPLETE": 2, "REFUSED": 3}


def cmd_check(args: argparse.Namespace) -> int:
    """`check`; no failure of the checker itself can read as a verdict."""
    try:
        return check(args)
    except Exception as err:  # noqa: BLE001 - any error here is no verdict, never a pass or a FAIL
        print(f"mega-eest: error: the checker could not read the run: {err!r}", file=sys.stderr)
        print(f"MEGA-EEST rev={args.rev} run=unknown stage=check result=INCOMPLETE")
        return RESULT_EXIT["INCOMPLETE"]


def check(args: argparse.Namespace) -> int:
    suite, work = Path(args.suite), Path(args.work)
    modes = list(dict.fromkeys(args.modes.split()))
    index = json.loads((suite / "slice" / "index.json").read_text())
    expected = Counter((c["caseId"], c["fileDigest"]) for c in index["cases"])
    total = sum(expected.values())
    run = "full" if {"plain", "instrumented"} <= set(modes) else "partial"
    summary = [f"rev={args.rev}", f"run={run}", f"cases={total}"]
    # Why the run reached no verdict; and whether a case failed in a run that did.
    incomplete: list[str] = []
    cases_failed = False
    observations: dict[str, dict[str, str | None]] = {}

    if not modes:
        incomplete.append("no mode was run")
    for mode in modes:
        if mode not in ("plain", "instrumented"):
            incomplete.append(f"no such mode as {mode!r}")
    if total == 0:
        incomplete.append("the slice's index lists no case")

    for mode in modes:
        report_path = work / f"{mode}.json"
        exit_path = work / f"{mode}.exit"
        code = exit_path.read_text().strip() if exit_path.exists() else "?"
        if not report_path.exists():
            incomplete.append(f"{mode}: `mega-fixture consume` wrote no report (exit {code}); "
                              f"see {work / (mode + '.log')}")
            summary.append(f"{mode}=no-report")
            continue
        cases = json.loads(report_path.read_text())["cases"]
        statuses = Counter(c["status"] for c in cases)
        got = Counter((c.get("caseId"), c.get("fileDigest")) for c in cases)
        missing, extra = expected - got, got - expected
        not_passed = [c for c in cases if c["status"] != "pass"]
        observations[mode] = {c.get("caseId"): c.get("observationsDigest") for c in cases}
        for c in not_passed[:MAX_SHOWN]:
            line = f"mega-eest: {mode}: {c['status']} {c.get('caseId') or c['file']}"
            if c["status"] == "fail":
                line += (f"\n  first difference at {c.get('path')}: expected {canonical(c.get('expected'))} "
                         f"actual {canonical(c.get('actual'))}\n  rules: {', '.join(c.get('rules', [])) or '(none)'}")
            elif c["status"] == "unsupported":
                line += f"\n  missing: {', '.join(c.get('missing', []))}"
            elif c["status"] == "error":
                line += f"\n  {c.get('message')}"
            print(line, file=sys.stderr)
        if len(not_passed) > MAX_SHOWN:
            print(f"mega-eest: {mode}: ... and {len(not_passed) - MAX_SHOWN} more, in {report_path}",
                  file=sys.stderr)
        if missing or extra:
            incomplete.append(
                f"{mode}: the cases consumed are not the slice's index: "
                f"{sum(missing.values())} missing, {sum(extra.values())} not in it"
                + (f", {next(iter(missing))[0]} among the missing" if missing else "")
                + (f", {next(iter(extra))[0]} among the others" if extra else ""))
        # `consume` exits 0 when every case passed and 1 when one did not; any other exit, or one
        # its report does not explain, is a run that did not finish as reported.
        if code not in ("0", "1") or (code == "0") != (not not_passed):
            incomplete.append(f"{mode}: `mega-fixture consume` exited {code} with "
                              f"{len(not_passed)} case(s) not passing in its report")
        if statuses.get("unsupported"):
            incomplete.append(f"{mode}: {statuses['unsupported']} case(s) the adapter does not support")
        if statuses.get("fail") or statuses.get("error"):
            cases_failed = True
        summary.append(f"{mode}=" + ",".join(
            f"{s}:{statuses.get(s, 0)}" for s in ("pass", "fail", "unsupported", "error")))

    if {"plain", "instrumented"} <= observations.keys():
        plain, inst = observations["plain"], observations["instrumented"]
        differ = sorted(k for k in plain.keys() | inst.keys()
                        if plain.get(k) is None or plain.get(k) != inst.get(k))
        for case in differ[:MAX_SHOWN]:
            print(f"mega-eest: parity: {case}: plain {plain.get(case)} instrumented {inst.get(case)}",
                  file=sys.stderr)
        summary.append(f"parity={'identical' if not differ else f'{len(differ)}-differ'}")
        cases_failed |= bool(differ)

    # The wrong expectation is the control that shows the consumer and the adapter can report a
    # failure at all. When every gating case passed, nothing else shows it, so the control must
    # fail at the path it records. When gating cases failed, the failures show it, and a change
    # of behaviour can also move where the control first differs, so it need only have run and
    # not passed (failed or erred, at any path). A control that was not run, or that passed,
    # says the consumer cannot be trusted either way. Without one, there is no verdict.
    negative_path = work / "negative.json"
    wrong_at = {n["caseId"]: n["wrongAt"] for n in index.get("negative", [])}
    if not wrong_at:
        verdict = "none"
        incomplete.append("the slice's index lists no wrong expectation, which shows the consumer can fail")
    elif not negative_path.exists():
        verdict = "no-report"
        incomplete.append("`mega-fixture consume` wrote no report of the wrong expectation")
    else:
        cases = {c.get("caseId"): c for c in json.loads(negative_path.read_text())["cases"]}
        states = []
        for case_id, path in wrong_at.items():
            c = cases.get(case_id)
            seen = f"{c['status']} at {c.get('path')}" if c else "not consumed"
            if c and c["status"] == "fail" and c.get("path") == path:
                states.append("fail-as-recorded")
            elif c and c["status"] in ("fail", "error") and cases_failed:
                states.append("fail-elsewhere")
                print(f"mega-eest: the wrong expectation {case_id} did not fail at {path} ({seen}); "
                      f"gating cases failed, which can move where it first differs", file=sys.stderr)
            else:
                states.append("not-caught")
                why = ("gating cases failed, and it did not fail" if cases_failed
                       else "every gating case passed, so it must fail where recorded")
                incomplete.append(f"the wrong expectation {case_id} did not fail at {path} ({seen}): {why}")
        verdict = ("not-caught" if "not-caught" in states
                   else "fail-elsewhere" if "fail-elsewhere" in states else "fail-as-recorded")
    summary.append(f"negative={verdict}")
    summary.append(f"provenance={args.provenance}")
    summary.append(f"caseset={args.caseset}")

    if incomplete:
        result = "INCOMPLETE"
    elif cases_failed:
        result = "FAIL"
    elif args.provenance == "unmerged" or args.caseset == "undeclared":
        result = "REFUSED"
    else:
        result = "pass"
    for reason in incomplete:
        print(f"mega-eest: error: no verdict: {reason}", file=sys.stderr)
    summary.append(f"result={result}")
    print("MEGA-EEST " + " ".join(summary))
    return RESULT_EXIT[result]


# ---- executable, case-diff --------------------------------------------------------------------


def cmd_executable(args: argparse.Namespace) -> int:
    found = set()
    for line in sys.stdin:
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (message.get("reason") == "compiler-artifact"
                and (message.get("target") or {}).get("name") == args.name
                and message.get("executable")):
            found.add(message["executable"])
    if len(found) != 1:
        raise GateError(f"the build reported {len(found)} executables named {args.name}, not one: "
                        + (", ".join(sorted(found)) or "none"))
    path = Path(next(iter(found)))
    if not path.is_file():
        raise GateError(f"the build reported {path} for {args.name}, and there is no such file")
    print(path)
    return 0


def cmd_case_diff(args: argparse.Namespace) -> int:
    def cases(path: str) -> dict[str, str]:
        return {c["caseId"]: c["fileDigest"] for c in json.loads(Path(path).read_text())["cases"]}

    base, head = cases(args.base_index), cases(args.index)
    removed = sorted(base.keys() - head.keys())
    added = sorted(head.keys() - base.keys())
    changed = sorted(k for k in base.keys() & head.keys() if base[k] != head[k])
    declared = sorted(set(args.declared.split()))
    if removed == declared:
        caseset = "declared" if removed else "kept"
    else:
        caseset = "undeclared"
    print(f"mega-eest: the pin moves {args.base_rev} -> {args.rev}: {len(removed)} case(s) removed, "
          f"{len(added)} added, {len(changed)} changed", file=sys.stderr)
    for title, ids in (("removed", removed), ("added", added), ("changed", changed)):
        for case_id in ids:
            print(f"mega-eest:   {title} {case_id}", file=sys.stderr)
    if caseset == "undeclared":
        print("mega-eest: error: scripts/mega_eest.env's MEGA_EEST_REMOVED_CASES must list exactly the "
              "case ids the pin removes: removed and not declared: "
              f"{' '.join(sorted(set(removed) - set(declared))) or '(none)'}; declared and not removed: "
              f"{' '.join(sorted(set(declared) - set(removed))) or '(none)'}", file=sys.stderr)
    md = [f"### Mega EEST case set, {args.base_rev[:12]} → {args.rev[:12]}", "",
          f"{len(removed)} removed, {len(added)} added, {len(changed)} changed; removals: **{caseset}**.", ""]
    for title, ids in (("Removed", removed), ("Added", added), ("Changed", changed)):
        if ids:
            md += [f"<details><summary>{title} ({len(ids)})</summary>", ""]
            md += [f"- `{case_id}`" for case_id in ids]
            md += ["", "</details>", ""]
    Path(args.summary).write_text("\n".join(md) + "\n")
    print(caseset)
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    pa = sub.add_parser("adapter", help="write the adapter workspace built against this checkout")
    pa.add_argument("--tree", required=True)
    pa.add_argument("--suite", required=True)
    pa.add_argument("--out", required=True)
    pa.add_argument("--engine-rustflags", default="",
                    help="compiler flags for this workspace's packages alone, space-separated")
    pa.set_defaults(func=cmd_adapter)

    pl = sub.add_parser("check-lock", help="hold the resolved adapter workspace to this checkout's lock")
    pl.add_argument("--tree", required=True)
    pl.add_argument("--adapter", required=True)
    pl.set_defaults(func=cmd_check_lock)

    pc = sub.add_parser("check", help="decide the gate from the run reports")
    pc.add_argument("--suite", required=True)
    pc.add_argument("--work", required=True)
    pc.add_argument("--modes", required=True)
    pc.add_argument("--rev", required=True)
    pc.add_argument("--provenance", default="unchecked", choices=["default-branch", "unmerged", "unchecked"])
    pc.add_argument("--caseset", default="unchecked",
                    choices=["unmoved", "kept", "declared", "undeclared", "unchecked"])
    pc.set_defaults(func=cmd_check)

    pe = sub.add_parser("executable", help="the executable a cargo build reported, from its JSON messages")
    pe.add_argument("--name", required=True)
    pe.set_defaults(func=cmd_executable)

    pd = sub.add_parser("case-diff", help="compare two pins' case sets")
    pd.add_argument("--base-rev", required=True)
    pd.add_argument("--rev", required=True)
    pd.add_argument("--base-index", required=True)
    pd.add_argument("--index", required=True)
    pd.add_argument("--declared", default="")
    pd.add_argument("--summary", required=True)
    pd.set_defaults(func=cmd_case_diff)

    args = p.parse_args()
    try:
        return args.func(args)
    except GateError as err:
        print(f"mega-eest: error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
