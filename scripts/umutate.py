#!/usr/bin/env python3
"""Custom-operator mutation driver (universalmutator + comby) for mega-evm.

This is the engine for *custom operator packs* — the domain-specific complement
to cargo-mutants. Each pack lives under `mutants/operators/<name>/` with a
`manifest.toml` (see that dir for the schema). The engine is pack-agnostic: it
runs whatever packs exist and feeds their results to the SAME shared gate that
cargo-mutants uses (`scripts/mutation_gate.py`), via the `caught.txt`/`missed.txt`
file contract. The killed/not-killed -> caught/missed adapter is the `_adapt`
step below.

Subcommands:

  plan [--diff <base>] [--packs a,b]
        Generate rules, resolve target files, generate the mutants, and print
        what WOULD be tested (counts + each mutant's stable id). Runs no tests
        and does not touch the working tree's tracked files. Use this to review
        operators before a real run.

  run  [--diff <base>] [--packs a,b] --output <dir>
        Full pipeline: generate -> mutate -> analyze (run the test command per
        mutant) -> write caught.txt/missed.txt/unviable.txt/timeout.txt into
        <dir>, ready for `mutation_gate.py report --results <dir>`.

In `--diff <base>` mode only files AND lines changed vs <base> are mutated
(the PR-gate scope); without it, every gate site in the crate is mutated.
"""
from __future__ import annotations

import argparse
import difflib
import fnmatch
import glob
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OPERATORS_DIR = ROOT / "mutants" / "operators"
DEFAULT_TEST_CMD = "cargo nextest run -p mega-evm --all-features"
ANALYZE_TIMEOUT = "600"  # seconds per mutant; generous for a full nextest run


def _run(cmd, **kw):
    return subprocess.run(cmd, cwd=ROOT, text=True, capture_output=True, **kw)


def require_tools() -> None:
    missing = [t for t in ("mutate", "analyze_mutants", "comby") if shutil.which(t) is None]
    if missing:
        sys.exit(
            f"missing required tools: {', '.join(missing)}\n"
            f"  mutate/analyze_mutants: `mise install` (declared in .mise.toml)\n"
            f"  comby: `paru -S comby-bin` (Arch) or `bash <(curl -sL get.comby.dev)`\n"
            f"see scripts/umutate_setup.sh."
        )


def load_packs(selected: list[str] | None) -> list[dict]:
    packs = []
    for manifest in sorted(OPERATORS_DIR.glob("*/manifest.toml")):
        data = tomllib.loads(manifest.read_text())
        data["_dir"] = manifest.parent
        if selected and data.get("name") not in selected:
            continue
        packs.append(data)
    if selected:
        found = {p.get("name") for p in packs}
        for name in selected:
            if name not in found:
                sys.exit(f"no operator pack named '{name}' under {OPERATORS_DIR}")
    return packs


def ensure_rules(pack: dict) -> Path:
    """Run the pack's generator (if any) and return the rules file path."""
    pdir: Path = pack["_dir"]
    rules = pdir / pack.get("rules", "rules.comby")
    if "generator" in pack:
        gen = pdir / pack["generator"]
        r = _run([sys.executable, str(gen), "--output", str(rules)])
        if r.returncode != 0:
            sys.exit(f"[{pack['name']}] generator failed:\n{r.stderr}")
    if not rules.exists():
        sys.exit(f"[{pack['name']}] rules file not found: {rules}")
    return rules


def changed_files_and_lines(base: str) -> tuple[set[str], dict[str, set[int]]]:
    """Parse `git diff --unified=0 <base>...HEAD` into changed files and the set
    of new-side line numbers per file (the lines the PR added/modified)."""
    r = _run(["git", "diff", "--unified=0", "--no-color", f"{base}...HEAD", "--",
              "crates/mega-evm/**"])
    if r.returncode != 0:
        sys.exit(f"git diff failed:\n{r.stderr}")
    files: set[str] = set()
    lines: dict[str, set[int]] = {}
    cur: str | None = None
    hunk = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
    for ln in r.stdout.splitlines():
        if ln.startswith("+++ b/"):
            cur = ln[6:]
            files.add(cur)
            lines.setdefault(cur, set())
        elif cur and (m := hunk.match(ln)):
            start, count = int(m.group(1)), int(m.group(2) or "1")
            # A pure-deletion hunk has new-side `+N,0` (count == 0): it adds no
            # new line, so contribute nothing. (A single-line add omits the count
            # entirely -> defaulted to 1 above.)
            for i in range(start, start + count):
                lines[cur].add(i)
    return files, lines


def resolve_targets(pack: dict, changed: set[str] | None,
                    files_glob: str | None = None) -> list[str]:
    cand: set[str] = set()
    for pat in pack.get("targets", []):
        cand.update(glob.glob(pat, root_dir=str(ROOT), recursive=True))
    cand = {f for f in cand if f.endswith(".rs") and (ROOT / f).is_file()}
    if changed is not None:
        cand &= changed
    if files_glob:
        cand = {f for f in cand if fnmatch.fnmatch(f, files_glob)}
    match = pack.get("match")
    if match:
        rx = re.compile(match)
        cand = {f for f in cand if rx.search((ROOT / f).read_text(errors="ignore"))}
    return sorted(cand)


def generate_mutants(pack: dict, rules: Path, src: str, mutant_dir: Path,
                     allowed_lines: set[int] | None) -> None:
    mutant_dir.mkdir(parents=True, exist_ok=True)
    if allowed_lines is not None and not allowed_lines:
        return  # diff mode, but nothing changed in this file
    # `--only <rules>` restricts to our pack's rules (no default language rules).
    # The rules path is absolute and contains ".rules", so universalmutator's
    # built-in lookup fails and it falls back to opening the file directly.
    #
    # We do NOT use universalmutator's `--lines` for diff scoping: in comby mode
    # its line filter compares against comby's byte-offset range (not line
    # numbers), so it silently drops every mutant. Instead we generate all mutants
    # and prune by the actual changed line, detected the same way the adapter does.
    cmd = ["mutate", src, "--only", str(rules), "--comby", "--noCheck",
           "--mutantDir", str(mutant_dir)]
    r = _run(cmd)
    if r.returncode != 0:
        sys.exit(f"[{pack['name']}] mutate failed on {src}:\n{r.stderr}\n{r.stdout}")
    if allowed_lines is not None:
        for m in list(mutant_dir.glob("*")):
            if not m.is_file():
                continue
            change = _first_change(ROOT / src, m)
            if change is None or change[0] not in allowed_lines:
                m.unlink()  # outside the PR's changed lines


def _ascii(s: str) -> str:
    # comby mode rewrites the file from an ASCII-normalized source, so we must
    # normalize the original the same way before diffing — otherwise non-ASCII
    # chars in comments (×, ≠, …) show up as spurious "changes".
    return s.encode("ascii", "ignore").decode()


def _first_change(src_path: Path, mutant_path: Path) -> tuple[int, str, str] | None:
    """Return (1-based line, before, after) of the first differing line."""
    a = [_ascii(l) for l in src_path.read_text(errors="ignore").splitlines()]
    b = [_ascii(l) for l in mutant_path.read_text(errors="ignore").splitlines()]
    sm = difflib.SequenceMatcher(a=a, b=b, autojunk=False)
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag != "equal":
            before = a[i1].strip() if i1 < len(a) else ""
            after = b[j1].strip() if j1 < len(b) else ""
            return (i1 + 1, before, after)
    return None


def _adapt(pack: dict, src: str, mutant_basename: str, mutant_dir: Path,
           allowed_lines: set[int] | None) -> str | None:
    """Map one universalmutator mutant file to a stable, gate-format id line.

    Returns `<file>:<line>:1: <pack> <before> -> <after>` or None if the mutant
    falls outside the diff scope (belt-and-suspenders alongside --lines)."""
    mpath = mutant_dir / mutant_basename
    if not mpath.exists():
        return None
    change = _first_change(ROOT / src, mpath)
    if change is None:
        return None
    line, before, after = change
    if allowed_lines is not None and line not in allowed_lines:
        return None
    return f"{src}:{line}:1: {pack['name']} {before} -> {after}"


def analyze(pack: dict, src: str, mutant_dir: Path) -> tuple[list[str], list[str]]:
    """Run analyze_mutants; return (killed_basenames, notkilled_basenames).

    analyze_mutants writes `<prefix>.killed.txt` / `<prefix>.notkilled.txt`
    (it inserts a literal '.' between the prefix and the filename)."""
    prefix = str(mutant_dir / "_result")
    test_cmd = pack.get("test_cmd", DEFAULT_TEST_CMD)
    r = _run(["analyze_mutants", src, test_cmd, "--mutantDir", str(mutant_dir),
              "--prefix", prefix, "--noShuffle", "--timeout", ANALYZE_TIMEOUT])
    if r.returncode != 0:
        sys.exit(f"[{pack['name']}] analyze_mutants failed on {src}:\n{r.stderr}")
    killed = _read(Path(prefix + ".killed.txt"))
    notkilled = _read(Path(prefix + ".notkilled.txt"))
    return killed, notkilled


def _read(p: Path) -> list[str]:
    return [ln.strip() for ln in p.read_text().splitlines() if ln.strip()] if p.exists() else []


def cmd_plan(args) -> int:
    require_tools()
    packs = load_packs(args.packs)
    changed_files = changed_lines = None
    if args.diff:
        changed_files, changed_lines = changed_files_and_lines(args.diff)
    total = 0
    with tempfile.TemporaryDirectory() as tmp:
        for pack in packs:
            rules = ensure_rules(pack)
            targets = resolve_targets(pack, changed_files, args.files)
            print(f"\n## pack '{pack['name']}' — {len(targets)} target file(s)")
            for src in targets:
                md = Path(tmp) / pack["name"] / src.replace("/", "_")
                allowed = changed_lines.get(src) if changed_lines is not None else None
                generate_mutants(pack, rules, src, md, allowed)
                ids = []
                for m in sorted(md.glob("*")):
                    if m.is_file() and m.name != "_lines.txt":
                        a = _adapt(pack, src, m.name, md, allowed)
                        if a:
                            ids.append(a)
                total += len(ids)
                if ids:
                    print(f"  {src}: {len(ids)} mutant(s)")
                    for i in ids:
                        print(f"      {i}")
    print(f"\n=== {total} mutant(s) would be tested ===")
    return 0


def cmd_run(args) -> int:
    require_tools()
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    caught, missed = [], []
    packs = load_packs(args.packs)
    changed_files = changed_lines = None
    if args.diff:
        changed_files, changed_lines = changed_files_and_lines(args.diff)
    with tempfile.TemporaryDirectory() as tmp:
        for pack in packs:
            rules = ensure_rules(pack)
            for src in resolve_targets(pack, changed_files, args.files):
                md = Path(tmp) / pack["name"] / src.replace("/", "_")
                allowed = changed_lines.get(src) if changed_lines is not None else None
                generate_mutants(pack, rules, src, md, allowed)
                if not any(p.is_file() and p.name != "_lines.txt" for p in md.glob("*")):
                    continue
                killed, notkilled = analyze(pack, src, md)
                for names, sink in ((killed, caught), (notkilled, missed)):
                    for name in names:
                        a = _adapt(pack, src, name, md, allowed)
                        if a:
                            sink.append(a)
    (out / "caught.txt").write_text("\n".join(caught) + ("\n" if caught else ""))
    (out / "missed.txt").write_text("\n".join(missed) + ("\n" if missed else ""))
    # Spec-gate mutants are valid Rust by construction, so there is no separate
    # unviable bucket; write empty files so the gate's reader is happy.
    (out / "unviable.txt").write_text("")
    (out / "timeout.txt").write_text("")
    print(f"caught: {len(caught)}  missed: {len(missed)}  -> {out}")
    print(f"score with: python3 scripts/mutation_gate.py report --results {out} "
          f"--suppressions mutants/suppressions.toml")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    def common(sp):
        sp.add_argument("--diff", metavar="BASE", help="scope to files+lines changed vs BASE")
        sp.add_argument("--packs", type=lambda s: s.split(","), default=None,
                        help="comma-separated pack names (default: all)")
        sp.add_argument("--files", metavar="GLOB", default=None,
                        help="further restrict target files to those matching GLOB")

    pp = sub.add_parser("plan", help="show what would be mutated; run no tests")
    common(pp)
    pp.set_defaults(func=cmd_plan)

    pr = sub.add_parser("run", help="run the full pipeline and write gate results")
    common(pr)
    pr.add_argument("--output", required=True, help="results dir for mutation_gate.py")
    pr.set_defaults(func=cmd_run)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
