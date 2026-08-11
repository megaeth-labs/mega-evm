#!/usr/bin/env python3
"""MegaETH domain mutator v0: domain-specific mutation operators.

Operators:
  - spec_gate: flip `.is_enabled(MegaSpecId::X)` to true/false
  - gas_const: ±1 on constants in constants.rs
  - adjacent_spec: replace MegaSpecId::X with predecessor/successor
  - call_delete: delete statement-position protocol/recording calls

Stdlib-only harness. Does not modify product code permanently: each mutant is
applied as a text edit, tested, then restored via journal compare-and-restore
(hash of current file vs original/mutated; never `git checkout`).

See README.md for usage and design notes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import signal
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, fields as dc_fields
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple

# ---------------------------------------------------------------------------
# Paths (relative to repo root)
# ---------------------------------------------------------------------------

SRC_ROOT = Path("crates/mega-evm/src")
CONSTANTS_FILE = SRC_ROOT / "constants.rs"
SPEC_RS = SRC_ROOT / "evm" / "spec.rs"
DEFAULT_STATE = Path("tools/mutation/state.json")
DEFAULT_REPORT = Path("tools/mutation/reports/report.md")
LOG_DIR = Path("tools/mutation/logs")
# Crash-recovery journal: written before each mutant apply, removed after restore.
DEFAULT_JOURNAL = Path("tools/mutation/.mutate-journal.json")

# Linear MegaSpecId order (must match crates/mega-evm/src/evm/spec.rs enum).
# Enumeration hard-fails if the source enum diverges from this list.
SPEC_ORDER: Tuple[str, ...] = (
    "EQUIVALENCE",
    "MINI_REX",
    "REX",
    "REX1",
    "REX2",
    "REX3",
    "REX4",
    "REX5",
    "REX6",
    "REX7",
)

# Statement-position protocol / recording calls eligible for call_delete.
CALL_DELETE_CALLEES: Tuple[str, ...] = (
    "check_limit",
    "push_frame",
    "pop_frame",
    "push_empty_frame",
    "before_frame_init",
    "before_frame_return_result",
    "on_sstore",
    "on_log",
    "record_oracle_hint_bytes",
    "record_compute_gas_all_dims",
)

# call_delete scope: crates/mega-evm/src/{limit,evm,sandbox,system}/**.rs
CALL_DELETE_SUBDIRS: Tuple[str, ...] = ("limit", "evm", "sandbox", "system")

# Full-tree clean whitelist (`git status --porcelain`).
# Only harness *runtime artifacts* may be dirty/untracked — never harness
# sources (mutate.py, README.md, TODO.md, .gitignore, …). Edits to those
# are treated as collateral writes and abort the campaign.
#
# Allowed:
#   tools/mutation/reports/**   campaign markdown / inventory outputs
#   tools/mutation/logs/**      per-mutant cargo logs
#   tools/mutation/state*.json  campaign state (gitignored)
#   tools/mutation/.mutate-journal.json(+tmp.*)  crash journal
#   any path ending in `.tmp.<pid>`  orphaned atomic-write temps (product or
#                                    harness); cleaned on success, may linger
#                                    after a kill mid-write
ALLOWED_ARTIFACT_DIR_PREFIXES = (
    "tools/mutation/reports/",
    "tools/mutation/logs/",
)
# Basename of allowed files that live directly under tools/mutation/ (no /).
# state*.json and .mutate-journal.json(+ atomic-write tmp suffix).
_ALLOWED_ARTIFACT_BASENAME = re.compile(
    r"^(?:state[^/]*\.json|\.mutate-journal\.json(?:\.tmp\.[^/]+)?)$"
)
# Atomic product/journal writes use `path.with_suffix(path.suffix + f".tmp.{pid}")`,
# e.g. `foo.rs.tmp.12345` or `.mutate-journal.json.tmp.12345`.
_ATOMIC_WRITE_TMP_SUFFIX = re.compile(r"\.tmp\.\d+$")

# Oracle packages (fixed; recorded in state for --resume binding).
ORACLE_L1_PACKAGE = "mega-evm"
ORACLE_L2_PACKAGE = "mega-state-test"
ORACLE_COMMAND = [
    f"cargo test -p {ORACLE_L1_PACKAGE} --quiet",
    f"cargo test -p {ORACLE_L2_PACKAGE} --quiet",
]

# Per-oracle-layer wall-clock ceiling for one `cargo test` call.
# Default (no explicit --test-timeout) is derived per layer from that layer's
# clean-tree baseline: max(FLOOR, ceil(FACTOR x baseline_seconds)).
TEST_TIMEOUT_FLOOR_S = 300
TEST_TIMEOUT_FACTOR = 5
# Used when no baseline wall time exists (--skip-baseline and no explicit
# --test-timeout), and for the baseline run itself (it cannot derive from
# a measurement it is about to take).
TEST_TIMEOUT_NO_BASELINE_S = 1800
# SIGTERM → grace → SIGKILL window when killing a timed-out process group.
TEST_TIMEOUT_KILL_GRACE_S = 10.0

# Match receiver.is_enabled(MegaSpecId::X) / receiver.is_enabled(crate::MegaSpecId::X)
IS_ENABLED_CALL = re.compile(
    r"\.is_enabled\s*\(\s*((?:crate::)?MegaSpecId::([A-Za-z0-9_]+))\s*\)"
)

# Leading comment-only line (after indent strip).
COMMENT_LINE = re.compile(r"^\s*(//|///|//!)")

# const NAME: type = rhs;
CONST_HEAD = re.compile(
    r"^(\s*(?:pub\s+)?const\s+([A-Za-z_][A-Za-z0-9_]*)\s*:\s*[^=\n]+?=\s*)(.*)$"
)


# ---------------------------------------------------------------------------
# Data model
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Mutant:
    """One concrete mutant (site × body)."""

    mid: str
    operator: str  # "spec_gate" | "gas_const" | "adjacent_spec" | "call_delete"
    file: str  # repo-relative path
    # 1-based line of the match start (for humans / filter).
    line: int
    # Human-readable site key (spec name or constant name).
    site: str
    # Body label: "true"/"false", "+1"/"-1", "pred=Y"/"succ=Y", or callee name.
    body: str
    # Exact original text span to replace (may span multiple lines).
    original: str
    # Replacement text.
    replacement: str
    # Byte offset of original in file content at enumeration time.
    # Recomputed on apply from content search for robustness after restores.
    # Kept for disambiguation when original appears more than once.
    occurrence: int = 0
    # Optional notes (e.g. multi-line const).
    note: str = ""

    def matches_filter(self, substr: str) -> bool:
        s = substr.lower()
        return (
            s in self.mid.lower()
            or s in self.file.lower()
            or s in self.site.lower()
            or s in self.body.lower()
            or s in self.operator.lower()
            or s in (self.note or "").lower()
        )


@dataclass
class MutantResult:
    mid: str
    operator: str
    file: str
    line: int
    site: str
    body: str
    status: str  # "killed" | "survived" | "error" | "skipped"
    kill_layer: Optional[str] = None  # "L1" | "L2" | None
    first_failing_test: Optional[str] = None
    duration_s: float = 0.0
    error: Optional[str] = None
    is_sentinel: bool = False
    note: str = ""
    # How a kill was obtained (None when not killed):
    #   "assertion" — the oracle ran to completion and reported failure
    #                 (failing test or compile error)
    #   "timeout"   — the oracle layer exceeded its wall-clock ceiling and its
    #                 process group was killed; a mutant that hangs the suite
    #                 counts as detected, but is reported separately so an
    #                 over-tight threshold is visible to an auditor.
    kill_kind: Optional[str] = None
    timed_out: bool = False
    # Wall-clock ceiling (seconds) that fired, for timeout results only.
    timeout_s: Optional[float] = None


# ---------------------------------------------------------------------------
# Source helpers
# ---------------------------------------------------------------------------


def repo_root() -> Path:
    """Resolve git repo root from CWD."""
    r = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode != 0:
        raise SystemExit(f"not inside a git repo: {r.stderr.strip()}")
    return Path(r.stdout.strip())


def run_git(args: Sequence[str], cwd: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )


def git_status_porcelain(cwd: Path) -> str:
    """Stable porcelain status for full-tree cleanliness checks."""
    r = run_git(["status", "--porcelain"], cwd)
    if r.returncode != 0:
        raise SystemExit(f"git status --porcelain failed: {r.stderr}")
    return r.stdout


# Back-compat alias used by report sampling.
def git_status_short(cwd: Path) -> str:
    return git_status_porcelain(cwd)


def _status_paths(line: str) -> List[str]:
    """Parse one porcelain line → list of repo-relative paths to check.

    Handles `XY path`, `?? path`, and rename/copy `XY orig -> new`.
    For renames **both** sides are returned so a product file cannot be
    hidden by renaming it into an allowed artifact directory.
    """
    raw = line.rstrip("\n")
    if not raw:
        return []
    if raw.startswith("?? "):
        p = raw[3:]
    elif len(raw) >= 3 and raw[2] == " ":
        p = raw[3:]
    else:
        p = raw
    p = p.strip()
    if " -> " in p:
        left, right = p.split(" -> ", 1)
        paths = [left.strip().strip('"'), right.strip().strip('"')]
    else:
        paths = [p.strip().strip('"')]
    return [x for x in paths if x]


# Back-compat for callers that only need the primary (destination) path.
def _status_path(line: str) -> Tuple[bool, str]:
    """Parse one porcelain line → (is_untracked, primary path).

    Primary path is the destination for renames (right-hand side).
    """
    raw = line.rstrip("\n")
    is_untracked = raw.startswith("?? ")
    paths = _status_paths(line)
    return is_untracked, (paths[-1] if paths else "")


def _path_matches_prefix(path: str, pref: str) -> bool:
    pref_dir = pref.rstrip("/") + "/"
    pref_bare = pref.rstrip("/")
    candidates = {path, path.rstrip("/") + "/", path.rstrip("/")}
    for c in candidates:
        if c == pref_bare or c == pref_dir or c.startswith(pref_dir) or c.startswith(pref):
            return True
    return False


def is_allowed_artifact_path(path: str) -> bool:
    """True if `path` is a harness runtime artifact (not harness source).

    Allowed:
      - under tools/mutation/reports/ or tools/mutation/logs/
      - tools/mutation/state*.json (basename only, no nested dirs)
      - tools/mutation/.mutate-journal.json(+tmp.*)
      - any `*.tmp.<pid>` orphaned atomic-write temp (product or harness)
    """
    if not path:
        return True
    # Orphaned temp from atomic_write_text / journal / state (any directory).
    if _ATOMIC_WRITE_TMP_SUFFIX.search(path):
        return True
    for pref in ALLOWED_ARTIFACT_DIR_PREFIXES:
        if _path_matches_prefix(path, pref):
            return True
    # Root-of-harness files only: tools/mutation/<basename>
    prefix = "tools/mutation/"
    if path.startswith(prefix):
        rest = path[len(prefix) :]
        if "/" not in rest and _ALLOWED_ARTIFACT_BASENAME.match(rest):
            return True
    return False


def is_allowed_dirty(line: str) -> bool:
    """True if every path on a porcelain line is a harness runtime artifact.

    Rename/copy lines require **both** sides to pass — renaming a product
    source into reports/ or logs/ must not whitelist the change.
    """
    paths = _status_paths(line)
    if not paths:
        return True
    return all(is_allowed_artifact_path(p) for p in paths)


def assert_tree_clean(
    cwd: Path,
    *,
    allow_harness: bool = True,
    context: str = "",
) -> None:
    """Full-repo clean assert via `git status --porcelain`.

    Whitelist is harness *runtime artifacts* only (reports/, logs/,
    state*.json, .mutate-journal.json). Harness sources (mutate.py,
    README.md, TODO.md, …) and any product path are **not** allowed dirty.
    Unexpected dirt aborts the campaign and **preserves the scene**
    (this function never restores files).
    """
    status = git_status_porcelain(cwd)
    if not status.strip():
        return
    bad: List[str] = []
    for line in status.splitlines():
        if not line.strip():
            continue
        if allow_harness and is_allowed_dirty(line):
            continue
        bad.append(line)
    if bad:
        where = f" ({context})" if context else ""
        msg = (
            f"worktree is not clean{where} (unexpected dirty paths):\n"
            + "\n".join(bad)
            + "\nScene preserved for inspection; campaign aborted."
        )
        raise SystemExit(msg)


def git_head(cwd: Path) -> str:
    r = run_git(["rev-parse", "HEAD"], cwd)
    if r.returncode != 0:
        raise SystemExit(f"git rev-parse HEAD failed: {r.stderr}")
    return r.stdout.strip()


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def inventory_fingerprint(mutants: Sequence[Mutant]) -> str:
    """Stable hash of the full mutant inventory (mids + originals + replacements)."""
    h = hashlib.sha256()
    for m in sorted(mutants, key=lambda x: x.mid):
        h.update(m.mid.encode("utf-8"))
        h.update(b"\0")
        h.update(m.original.encode("utf-8"))
        h.update(b"\0")
        h.update(m.replacement.encode("utf-8"))
        h.update(b"\0")
    return h.hexdigest()


def line_is_comment(line: str) -> bool:
    return bool(COMMENT_LINE.match(line))


def in_string_literal(line: str, col: int) -> bool:
    """Heuristic: is `col` inside a double-quoted string on this line?

    Handles simple escapes. Does not model raw strings or multi-line strings
    (none expected for is_enabled call sites).
    """
    i = 0
    in_str = False
    escape = False
    while i < len(line) and i < col:
        c = line[i]
        if in_str:
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_str = False
        else:
            if c == '"':
                in_str = True
            elif c == "'" and False:
                # Rust char literals are rare here; skip for simplicity.
                pass
        i += 1
    return in_str


def _skip_ws_left(text: str, i: int) -> int:
    """Index of the first non-whitespace character at or left of `i`, or -1."""
    while i >= 0 and text[i] in " \t\r\n":
        i -= 1
    return i


def find_receiver_start(text: str, dot_pos: int) -> int:
    """Walk left from the '.' of `.is_enabled(...)` to the start of the receiver expr.

    Multi-line method chains such as::

        mega_spec
            .is_enabled(MegaSpecId::REX4)

    or::

        if context
            .host
            .spec_id()
            .is_enabled(MegaSpecId::REX5)

    are resolved by reusing call_delete's segment walker (`_find_call_expr_start`),
    which allows whitespace/newlines only between a segment and a preceding `.`
    and therefore stops at statement keywords (`if`, `let`, …) rather than
    swallowing them.

    Returns `dot_pos` when no segment can be parsed to the left (an *empty*
    receiver); callers must treat that as a walk failure, not as a zero-width
    receiver — see `find_spec_gate_sites`.
    """
    # `_find_call_expr_start` expects the start of the method name after `.`.
    j = dot_pos + 1
    while j < len(text) and text[j] in " \t":
        j += 1
    return _find_call_expr_start(text, j)


def line_number_at(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def find_cfg_test_ranges(text: str) -> List[Tuple[int, int, str]]:
    """Byte ranges covering `#[cfg(test)]` modules / items (product-code exclusion).

    Walks line-by-line: on `#[cfg(test)]` finds the following `mod`/`fn` (skipping
    blank/comment/attribute lines), then brace-matches to the end of that item.
    """
    ranges: List[Tuple[int, int, str]] = []
    lines = text.splitlines(keepends=True)
    offsets: List[int] = []
    acc = 0
    for ln in lines:
        offsets.append(acc)
        acc += len(ln)

    i = 0
    n = len(lines)
    while i < n:
        if re.search(r"#\[cfg\s*\(\s*test\s*\)\]", lines[i]):
            j = i + 1
            while j < n and (
                lines[j].strip() == ""
                or lines[j].strip().startswith("//")
                or lines[j].strip().startswith("#[")
            ):
                j += 1
            if j < n and re.search(r"\b(mod|fn)\b", lines[j]):
                k = j
                while k < n and "{" not in lines[k]:
                    k += 1
                if k < n:
                    depth = 0
                    for t in range(k, n):
                        depth += lines[t].count("{") - lines[t].count("}")
                        if depth <= 0 and t >= k:
                            start = offsets[i]
                            end = offsets[t] + len(lines[t])
                            ranges.append(
                                (start, end, f"cfg(test) L{i + 1}-L{t + 1}")
                            )
                            i = t
                            break
        i += 1
    return ranges


def in_assert_macro(text: str, pos: int) -> Optional[str]:
    """Return a short label if `pos` sits inside assert!/debug_assert!/assert_eq!/assert_ne! args.

    Heuristic: scan back ≤2k chars for a macro name, then check the following
    parenthesis is still open at `pos` (string-aware enough for simple cases).
    """
    window_start = max(0, pos - 2000)
    window = text[window_start:pos]
    macros = list(
        re.finditer(r"\b(debug_assert|assert_eq|assert_ne|assert)\s*!", window)
    )
    for m in reversed(macros):
        after = window[m.end() :]
        paren = after.find("(")
        if paren < 0:
            continue
        abs_open = window_start + m.end() + paren
        depth = 0
        in_str = False
        escape = False
        for i in range(abs_open, pos):
            c = text[i]
            if in_str:
                if escape:
                    escape = False
                elif c == "\\":
                    escape = True
                elif c == '"':
                    in_str = False
                continue
            if c == '"':
                in_str = True
                continue
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0:
                    break
        else:
            if depth > 0:
                name = m.group(1)
                return f"{name}! (open at byte {abs_open})"
    return None


# ---------------------------------------------------------------------------
# Spec order (adjacent_spec) — verified against source
# ---------------------------------------------------------------------------


def parse_megaspec_variants(root: Path) -> List[str]:
    """Parse `enum MegaSpecId` variant names from spec.rs in declaration order."""
    path = root / SPEC_RS
    if not path.is_file():
        raise SystemExit(f"missing MegaSpecId source: {path}")
    text = path.read_text(encoding="utf-8")
    m = re.search(r"pub\s+enum\s+MegaSpecId\s*\{", text)
    if not m:
        raise SystemExit(f"could not find `pub enum MegaSpecId` in {SPEC_RS}")
    # Brace-match the enum body.
    i = m.end() - 1  # position of '{'
    depth = 0
    end = None
    for j in range(i, len(text)):
        c = text[j]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                end = j
                break
    if end is None:
        raise SystemExit(f"unclosed MegaSpecId enum in {SPEC_RS}")
    body = text[i + 1 : end]
    # Variant lines: optional docs/attrs then IDENT, or IDENT {
    variants: List[str] = []
    for line in body.splitlines():
        s = line.strip()
        if not s or s.startswith("//") or s.startswith("///") or s.startswith("#["):
            continue
        # Strip trailing comma and any inline comment.
        s = s.split("//", 1)[0].strip().rstrip(",").strip()
        if not s:
            continue
        # Skip nested items if any; take leading identifier.
        vm = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)\b", s)
        if vm:
            variants.append(vm.group(1))
    return variants


def verify_spec_order(root: Path) -> None:
    """Hard-fail if SPEC_ORDER does not exactly match MegaSpecId in source."""
    found = parse_megaspec_variants(root)
    if found != list(SPEC_ORDER):
        raise SystemExit(
            "MegaSpecId order mismatch between harness SPEC_ORDER and "
            f"{SPEC_RS}:\n"
            f"  harness: {list(SPEC_ORDER)}\n"
            f"  source:  {found}\n"
            "Update SPEC_ORDER (and adjacent_spec) before enumerating."
        )


def adjacent_specs(name: str) -> Tuple[Optional[str], Optional[str]]:
    """Return (predecessor, successor) for a MegaSpecId name, or None at ends."""
    try:
        idx = SPEC_ORDER.index(name)
    except ValueError as e:
        raise SystemExit(
            f"unknown MegaSpecId variant in product gate: {name!r}. "
            f"Known: {list(SPEC_ORDER)}"
        ) from e
    pred = SPEC_ORDER[idx - 1] if idx > 0 else None
    succ = SPEC_ORDER[idx + 1] if idx + 1 < len(SPEC_ORDER) else None
    return pred, succ


# ---------------------------------------------------------------------------
# Enumeration: shared is_enabled site inventory (spec_gate + adjacent_spec)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SpecGateSite:
    """One product `.is_enabled(MegaSpecId::X)` site (shared by two operators)."""

    file: str
    line: int
    spec_name: str
    full_path: str  # MegaSpecId::X or crate::MegaSpecId::X
    path_original: str  # exact bytes of full_path at the call
    path_occurrence: int
    expr_original: str  # full receiver.is_enabled(...)
    expr_occurrence: int
    site: str
    note: str


def find_spec_gate_sites(root: Path) -> Tuple[List[SpecGateSite], List[str]]:
    """Walk product sources for is_enabled(MegaSpecId::X) sites.

    Non-product sites are excluded (same rules for spec_gate and adjacent_spec):
    - `#[cfg(test)]` modules / items
    - `is_enabled(...)` as an argument to assert!/debug_assert!/assert_eq!/assert_ne!
    - comment lines and string literals
    """
    sites: List[SpecGateSite] = []
    exclusions: List[str] = []
    src = root / SRC_ROOT
    if not src.is_dir():
        raise SystemExit(f"missing source root: {src}")

    files = sorted(src.rglob("*.rs"))
    site_counter: Dict[str, int] = {}

    for fpath in files:
        rel = str(fpath.relative_to(root))
        text = fpath.read_text(encoding="utf-8")
        lines = text.splitlines(keepends=True)
        test_ranges = find_cfg_test_ranges(text)

        offsets: List[int] = []
        acc = 0
        for ln in lines:
            offsets.append(acc)
            acc += len(ln)

        for m in IS_ENABLED_CALL.finditer(text):
            # Match is on `.is_enabled(...)`; receiver is to the left of the dot.
            call_start = m.start()  # position of '.'
            call_end = m.end()
            spec_name = m.group(2)
            full_path = m.group(1)  # MegaSpecId::X or crate::MegaSpecId::X

            line_idx = line_number_at(text, call_start) - 1
            line_text = lines[line_idx] if 0 <= line_idx < len(lines) else ""

            if line_is_comment(line_text):
                exclusions.append(
                    f"comment-line: {rel}:{line_idx + 1}: {line_text.rstrip()[:120]}"
                )
                continue

            col_in_line = call_start - offsets[line_idx]
            if in_string_literal(line_text, col_in_line):
                exclusions.append(
                    f"string-literal: {rel}:{line_idx + 1}: col {col_in_line}"
                )
                continue

            test_hit = next(
                (label for a, b, label in test_ranges if a <= call_start < b),
                None,
            )
            if test_hit is not None:
                exclusions.append(
                    f"cfg-test: {rel}:{line_idx + 1}: {test_hit}: {line_text.rstrip()[:100]}"
                )
                continue

            assert_hit = in_assert_macro(text, call_start)
            if assert_hit is not None:
                exclusions.append(
                    f"assert-macro: {rel}:{line_idx + 1}: {assert_hit}: "
                    f"{line_text.rstrip()[:100]}"
                )
                continue

            recv_start = find_receiver_start(text, call_start)
            expr_original = text[recv_start:call_end]
            # Empty receiver: the walk stopped on the '.' itself because no
            # segment could be parsed to its left (e.g. a `foo()?.` or `arr[i].`
            # receiver). Mutating that span would splice `true`/`false` straight
            # after the unparsed prefix, and the resulting compile error would be
            # scored as a kill. Drop the site instead of emitting a fake mutant.
            if recv_start >= call_start:
                exclusions.append(
                    f"empty-receiver: {rel}:{line_idx + 1}: "
                    f"{line_text.rstrip()[:120]}"
                )
                continue
            if ".is_enabled" not in expr_original:
                exclusions.append(
                    f"receiver-walk-failed: {rel}:{line_idx + 1}: "
                    f"{text[call_start:call_end]!r}"
                )
                continue

            # Exact path span inside the match (group 1).
            path_start = m.start(1)
            path_end = m.end(1)
            path_original = text[path_start:path_end]
            if path_original != full_path:
                # Defensive: regex group should equal slice.
                path_original = full_path

            expr_occ = text[:recv_start].count(expr_original)
            path_occ = text[:path_start].count(path_original)

            base_key = f"spec_gate:{rel}:{spec_name}"
            site_counter[base_key] = site_counter.get(base_key, 0) + 1
            n = site_counter[base_key]
            site = f"{spec_name}@{rel}:{line_idx + 1}"
            if n > 1:
                site = f"{site}#{n}"

            sites.append(
                SpecGateSite(
                    file=rel,
                    line=line_idx + 1,
                    spec_name=spec_name,
                    full_path=full_path,
                    path_original=path_original,
                    path_occurrence=path_occ,
                    expr_original=expr_original,
                    expr_occurrence=expr_occ,
                    site=site,
                    note=f"expr={expr_original!r} path={full_path}",
                )
            )

    return sites, exclusions


def enumerate_spec_gate(root: Path) -> Tuple[List[Mutant], List[str]]:
    """Return (mutants, exclusion_notes) for true/false flips of is_enabled sites."""
    sites, exclusions = find_spec_gate_sites(root)
    mutants: List[Mutant] = []
    for s in sites:
        for body, replacement in (("true", "true"), ("false", "false")):
            mid = f"spec_gate:{s.file}:{s.line}:{s.spec_name}:{body}"
            if any(x.mid == mid for x in mutants):
                mid = f"{mid}:occ{s.expr_occurrence}"
            mutants.append(
                Mutant(
                    mid=mid,
                    operator="spec_gate",
                    file=s.file,
                    line=s.line,
                    site=s.site,
                    body=body,
                    original=s.expr_original,
                    replacement=replacement,
                    occurrence=s.expr_occurrence,
                    note=s.note,
                )
            )
    return mutants, exclusions


def enumerate_adjacent_spec(root: Path) -> Tuple[List[Mutant], List[str]]:
    """Adjacent-spec replacement mutants for the same sites as spec_gate.

    For each `is_enabled(MegaSpecId::X)` product site, emit up to two bodies:
      - pred: replace X with its predecessor (skip if X is first)
      - succ: replace X with its successor (skip if X is last)

    mid: `adjacent_spec:<file>:<line>:<X>:<pred|succ>=<Y>`
    """
    verify_spec_order(root)
    # Site inventory matches spec_gate; shared non-product filters are reported
    # only once via enumerate_spec_gate exclusions.
    sites, _shared_excl = find_spec_gate_sites(root)
    exclusions: List[str] = []
    mutants: List[Mutant] = []
    for s in sites:
        pred, succ = adjacent_specs(s.spec_name)
        # Preserve crate:: prefix when present.
        if s.path_original.startswith("crate::"):
            path_prefix = "crate::MegaSpecId::"
        else:
            path_prefix = "MegaSpecId::"

        for kind, target in (("pred", pred), ("succ", succ)):
            if target is None:
                exclusions.append(
                    f"adjacent_spec-skip-end: {s.file}:{s.line}: "
                    f"{s.spec_name} has no {kind}"
                )
                continue
            body = f"{kind}={target}"
            mid = f"adjacent_spec:{s.file}:{s.line}:{s.spec_name}:{body}"
            if any(x.mid == mid for x in mutants):
                mid = f"{mid}:occ{s.path_occurrence}"
            mutants.append(
                Mutant(
                    mid=mid,
                    operator="adjacent_spec",
                    file=s.file,
                    line=s.line,
                    site=s.site,
                    body=body,
                    original=s.path_original,
                    replacement=f"{path_prefix}{target}",
                    occurrence=s.path_occurrence,
                    note=f"{s.note} → {kind}={target}",
                )
            )
    return mutants, exclusions


# ---------------------------------------------------------------------------
# Enumeration: call_delete (statement-position protocol calls)
# ---------------------------------------------------------------------------


# Callee name + optional turbofish + opening paren.
_CALL_DELETE_OPEN = re.compile(
    r"\b("
    + "|".join(re.escape(c) for c in CALL_DELETE_CALLEES)
    + r")\s*(?:::\s*<[^>]*>\s*)?\("
)


def _match_balanced_parens(text: str, open_paren: int) -> Optional[int]:
    """Return index of matching `)` for `(` at open_paren, or None if unbalanced.

    Skips simple string/char contents; good enough for statement-call args.
    """
    if open_paren < 0 or open_paren >= len(text) or text[open_paren] != "(":
        return None
    depth = 0
    in_str = False
    in_char = False
    escape = False
    for i in range(open_paren, len(text)):
        c = text[i]
        if in_str:
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_str = False
            continue
        if in_char:
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == "'":
                in_char = False
            continue
        if c == '"':
            in_str = True
            continue
        if c == "'":
            in_char = True
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return i
    return None


def _line_start_at(text: str, pos: int) -> int:
    nl = text.rfind("\n", 0, pos)
    return 0 if nl < 0 else nl + 1


def _find_call_expr_start(text: str, callee_start: int) -> int:
    """Start of `recv.callee` / bare `callee` for a call whose name starts at callee_start.

    Walks left over method-chain segments, allowing whitespace/newlines **only**
    between a segment and a preceding `.` so multi-line receivers such as

        let x = ctx
            .foo
            .bar();

    resolve to `ctx` (then rejected as non-statements by the line-prefix check).
    Does **not** cross statement boundaries (`;`, `{`, …).

    Shared with spec-gate: `find_receiver_start` delegates here so both operators
    resolve line-wrapped receivers identically. Returns `callee_start` (bare
    call) or the '.' position when no segment parses to its left.
    """
    # Optional whitespace then '.' before the callee name → method call.
    j = callee_start - 1
    while j >= 0 and text[j] in " \t":
        j -= 1
    if j < 0 or text[j] != ".":
        return callee_start

    # `dot` is the '.' immediately before the current segment name.
    dot = j
    while True:
        # Consume one segment to the left of `dot`: ident, path, or call `ident(...)`.
        k = _skip_ws_left(text, dot - 1)
        if k < 0:
            return dot

        if text[k] == ")":
            # Walk back over balanced `(...)` then the callee name of that call.
            depth = 1
            k -= 1
            in_str = False
            escape = False
            while k >= 0 and depth > 0:
                c = text[k]
                if in_str:
                    if escape:
                        escape = False
                    elif c == "\\":
                        escape = True
                    elif c == '"':
                        in_str = False
                    k -= 1
                    continue
                if c == '"':
                    in_str = True
                    k -= 1
                    continue
                if c == ")":
                    depth += 1
                elif c == "(":
                    depth -= 1
                k -= 1
            # k is now just before '('. Optional turbofish / name before '('.
            while k >= 0 and text[k] in " \t":
                k -= 1
            # Skip turbofish `::<...>` if present (walk back over it).
            if k >= 0 and text[k] == ">":
                depth_a = 1
                k -= 1
                while k >= 0 and depth_a > 0:
                    if text[k] == ">":
                        depth_a += 1
                    elif text[k] == "<":
                        depth_a -= 1
                    k -= 1
                while k >= 0 and text[k] in " \t":
                    k -= 1
                if k >= 1 and text[k - 1 : k + 1] == "::":
                    k -= 2
                while k >= 0 and text[k] in " \t":
                    k -= 1
            # Identifier / path before the call.
            if k >= 0 and (text[k].isalnum() or text[k] in "_$"):
                while k >= 0 and (text[k].isalnum() or text[k] in "_$"):
                    k -= 1
                if k >= 1 and text[k - 1 : k + 1] == "::":
                    # rare: Type::method — keep walking path segments simply
                    while k >= 1 and text[k - 1 : k + 1] == "::":
                        k -= 2
                        while k >= 0 and (text[k].isalnum() or text[k] in "_$"):
                            k -= 1
            seg_start = k + 1
        elif text[k].isalnum() or text[k] in "_$":
            while k >= 0 and (text[k].isalnum() or text[k] in "_$"):
                k -= 1
            # Path prefix `Foo::`
            while k >= 1 and text[k - 1 : k + 1] == "::":
                k -= 2
                while k >= 0 and (text[k].isalnum() or text[k] in "_$"):
                    k -= 1
            seg_start = k + 1
        else:
            # Cannot parse a segment (e.g. hit `;`); start at the '.' we had.
            return dot

        # Can the chain continue further left via another '.'?
        k = _skip_ws_left(text, seg_start - 1)
        if k >= 0 and text[k] == ".":
            # Do not chain into a `//` comment (e.g. `// ... budget.` above a call).
            ls = _line_start_at(text, k)
            line_head = text[ls : k + 1]
            if re.search(r"//", line_head):
                return seg_start
            # Block-comment end on this line is rare in this codebase; keep simple.
            dot = k
            continue
        return seg_start


def enumerate_call_delete(
    root: Path,
) -> Tuple[List[Mutant], List[str], Dict[str, Dict[str, int]]]:
    """Delete statement-position calls to curated protocol methods.

    Only full statements whose value is discarded:
      `recv.callee(...);`  (single-line or multi-line ending in `);`)
    Expression-position uses (assignment, return, condition, `?`, method chain
    continuation) are skipped.

    Returns (mutants, exclusion_notes, per_callee stats:
      {callee: {enumerated, skipped}}).
    mid: `call_delete:<file>:<line>:<callee>`
    """
    mutants: List[Mutant] = []
    exclusions: List[str] = []
    stats: Dict[str, Dict[str, int]] = {
        c: {"enumerated": 0, "skipped": 0} for c in CALL_DELETE_CALLEES
    }

    src = root / SRC_ROOT
    if not src.is_dir():
        raise SystemExit(f"missing source root: {src}")

    files: List[Path] = []
    for sub in CALL_DELETE_SUBDIRS:
        d = src / sub
        if d.is_dir():
            files.extend(sorted(d.rglob("*.rs")))
    files = sorted(set(files))

    for fpath in files:
        rel = str(fpath.relative_to(root))
        text = fpath.read_text(encoding="utf-8")
        lines = text.splitlines(keepends=True)
        test_ranges = find_cfg_test_ranges(text)

        offsets: List[int] = []
        acc = 0
        for ln in lines:
            offsets.append(acc)
            acc += len(ln)

        for m in _CALL_DELETE_OPEN.finditer(text):
            callee = m.group(1)
            callee_start = m.start(1)
            open_paren = m.end() - 1  # the '(' of the call
            line_idx = line_number_at(text, callee_start) - 1
            line_text = lines[line_idx] if 0 <= line_idx < len(lines) else ""

            def skip(reason: str) -> None:
                stats[callee]["skipped"] += 1
                exclusions.append(
                    f"call_delete-skip-{reason}: {rel}:{line_idx + 1}: {callee}: "
                    f"{line_text.rstrip()[:100]}"
                )

            if line_is_comment(line_text):
                skip("comment")
                continue

            col_in_line = callee_start - offsets[line_idx]
            if in_string_literal(line_text, col_in_line):
                skip("string")
                continue

            test_hit = next(
                (label for a, b, label in test_ranges if a <= callee_start < b),
                None,
            )
            if test_hit is not None:
                skip("cfg-test")
                continue

            assert_hit = in_assert_macro(text, callee_start)
            if assert_hit is not None:
                skip("assert-macro")
                continue

            # Skip method / function definitions: `fn callee(` on this line.
            header_start = _line_start_at(text, callee_start)
            header_prefix = text[header_start:callee_start]
            if re.search(r"\bfn\b", header_prefix):
                skip("definition")
                continue

            close_paren = _match_balanced_parens(text, open_paren)
            if close_paren is None:
                skip("unbalanced-parens")
                continue

            # After `)`: only whitespace then `;` then whitespace to EOL.
            # Reject `?`, method chaining `.`, assignment uses already handled by prefix.
            j = close_paren + 1
            while j < len(text) and text[j] in " \t":
                j += 1
            if j >= len(text) or text[j] != ";":
                skip("not-statement")  # `?`, `.`, `,`, etc.
                continue
            semi = j
            j = semi + 1
            while j < len(text) and text[j] in " \t":
                j += 1
            # Must be end of line (or EOF).
            if j < len(text) and text[j] not in "\r\n":
                skip("trailing-junk")
                continue

            expr_start = _find_call_expr_start(text, callee_start)
            stmt_line_start = _line_start_at(text, expr_start)
            # Only whitespace between line start and expression start.
            if text[stmt_line_start:expr_start].strip() != "":
                skip("expr-position")  # e.g. `let x = ...`, `return ...`, `if ...`
                continue

            # Statement span: from line start through `;`, plus following newline if any.
            end = semi + 1
            if end < len(text) and text[end] == "\r":
                end += 1
            if end < len(text) and text[end] == "\n":
                end += 1
            original = text[stmt_line_start:end]
            if not original.strip():
                skip("empty")
                continue

            occ = text[:stmt_line_start].count(original)
            mid = f"call_delete:{rel}:{line_idx + 1}:{callee}"
            if any(x.mid == mid for x in mutants):
                mid = f"{mid}:occ{occ}"

            mutants.append(
                Mutant(
                    mid=mid,
                    operator="call_delete",
                    file=rel,
                    line=line_idx + 1,
                    site=f"{callee}@{rel}:{line_idx + 1}",
                    body=callee,
                    original=original,
                    replacement="",  # delete the whole statement line(s)
                    occurrence=occ,
                    note="statement-delete",
                )
            )
            stats[callee]["enumerated"] += 1

    return mutants, exclusions, stats


# ---------------------------------------------------------------------------
# Enumeration: gas / size constants
# ---------------------------------------------------------------------------


def _strip_trailing_comment(rhs_and_rest: str) -> Tuple[str, str, str]:
    """Split `RHS; // comment` or `RHS;` into (rhs, semicolon_tail, trailing).

    Returns (rhs, ';', trailing_including_newline_or_comment).
    """
    # Multi-line pieces arrive without the leading `const ... =` head.
    s = rhs_and_rest
    # Find the terminating semicolon that is not inside a string (simple scan).
    in_str = False
    escape = False
    for i, c in enumerate(s):
        if in_str:
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_str = False
            continue
        if c == '"':
            in_str = True
            continue
        if c == ";":
            rhs = s[:i].rstrip()
            tail = s[i + 1 :]  # after ';'
            return rhs, ";", tail
    raise ValueError(f"no terminating semicolon in const RHS: {s[:80]!r}")


def enumerate_gas_const(root: Path) -> Tuple[List[Mutant], List[str]]:
    mutants: List[Mutant] = []
    exclusions: List[str] = []
    fpath = root / CONSTANTS_FILE
    if not fpath.is_file():
        raise SystemExit(f"missing constants file: {fpath}")
    rel = str(CONSTANTS_FILE)
    text = fpath.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)

    i = 0
    while i < len(lines):
        line = lines[i]
        if line_is_comment(line):
            i += 1
            continue
        m = CONST_HEAD.match(line.rstrip("\n"))
        if not m:
            i += 1
            continue

        head = m.group(1)  # includes up through `= `
        name = m.group(2)
        rest = m.group(3)
        start_line = i + 1

        # Accumulate until we have a complete `...;`
        chunk_lines = [rest + ("\n" if line.endswith("\n") else "")]
        # If the match line had no newline in rest path — rebuild carefully.
        # CONST_HEAD matched against rstrip('\n'), so restore from original line.
        after_eq = line[len(m.group(0)) - len(m.group(3)) :]
        chunk = after_eq
        j = i
        while ";" not in _code_part(chunk):
            j += 1
            if j >= len(lines):
                exclusions.append(
                    f"unterminated-const: {rel}:{start_line}: {name}"
                )
                break
            chunk += lines[j]
        else:
            # Found semicolon in code part.
            try:
                rhs, _semi, trailing = _strip_trailing_comment(chunk)
            except ValueError as e:
                exclusions.append(f"parse-fail: {rel}:{start_line}: {name}: {e}")
                i += 1
                continue

            # Original full RHS assignment text from after `=` through `;`
            # We replace only the RHS value portion: `= <rhs>;` → `= (<rhs>) ± 1;`
            original_rhs = rhs
            # Skip pure literal 0 for -1 only; still do +1.
            is_zero = original_rhs.strip() == "0"

            # Build the original span as it appears: from start of head's line
            # through the semicolon. Apply replaces the RHS expression only by
            # searching for `= <rhs>` carefully — simpler: replace
            # `NAME ... = <rhs>` occurrence of the full const line span.
            # Use the exact text from file for the original RHS including spaces.
            # Reconstruct original substring:
            # From beginning of const line through end of semicolon line.
            end_line = j
            original_block = "".join(lines[i : end_line + 1])
            # Within the block, locate `= <rhs>` and replace RHS.
            # We apply by replacing the first occurrence of `= {rhs}` after the const name
            # inside the file — using occurrence index among same RHS.

            # Prefer unique marker: full `= {rhs};` as in source (rhs may have newlines).
            # Normalize: original is the RHS text as written (possibly multi-line).
            # Replacement wraps: `(<rhs>) + 1`

            # Count how many times this exact RHS appears before this const in the file.
            prefix = "".join(lines[:i])
            # Find `= <rhs>` pattern — use the chunk's RHS portion exactly.
            # Extract exact RHS from chunk (everything before first code `;`).
            exact_rhs = original_rhs
            # The searchable original is `= {exact_rhs}` with the same internal whitespace
            # as in the file. Recover exact bytes from chunk before `;`.
            code_semi = _find_code_semicolon(chunk)
            exact_rhs_raw = chunk[:code_semi].rstrip()  # may include leading spaces/newlines
            # For multi-line, first line after `=` may start immediately.
            # CONST line: `... = <maybe empty>\n    rest`
            # exact_rhs_raw is what follows `=` on the first line + continuation.

            # What we replace: the RHS only (exact_rhs_raw), wrapping it.
            # But bare RHS may not be unique. Use occurrence among identical RHS strings.
            occ = prefix.count(exact_rhs_raw) if exact_rhs_raw else 0

            # Module path for filter convenience: scan back for `mod foo {`
            mod = _enclosing_mod(lines, i)
            site = f"{mod}::{name}" if mod else name

            bodies: List[Tuple[str, str]] = [
                ("+1", f"({exact_rhs_raw.strip()}) + 1"),
            ]
            if is_zero:
                exclusions.append(
                    f"skip-minus-one-on-zero: {rel}:{start_line}: {name}"
                )
            else:
                bodies.append(("-1", f"({exact_rhs_raw.strip()}) - 1"))

            for body, new_rhs in bodies:
                # Preserve leading whitespace of multi-line RHS (indent of first non-empty).
                # We replace exact_rhs_raw with a single-line parenthesized form for
                # multi-line safety: use stripped form and keep a single space after `=`.
                # Application strategy: replace exact_rhs_raw at occurrence.
                mid = f"gas_const:{name}:{body}"
                if mod:
                    mid = f"gas_const:{mod}::{name}:{body}"
                note = ""
                if end_line != i:
                    note = "multi-line-rhs"
                if is_zero and body == "+1":
                    note = (note + " zero-literal").strip()
                mutants.append(
                    Mutant(
                        mid=mid,
                        operator="gas_const",
                        file=rel,
                        line=start_line,
                        site=site,
                        body=body,
                        original=exact_rhs_raw,
                        replacement=f" {new_rhs}"
                        if exact_rhs_raw[:1].isspace()
                        else new_rhs
                        if not exact_rhs_raw[:1].isspace()
                        else new_rhs,
                        occurrence=occ,
                        note=note,
                    )
                )
                # Fix replacement whitespace: keep one leading space if original had
                # leading newline/spaces after `=`.
                # Re-set last mutant replacement cleanly:
                mutants[-1] = Mutant(
                    mid=mid,
                    operator="gas_const",
                    file=rel,
                    line=start_line,
                    site=site,
                    body=body,
                    original=exact_rhs_raw,
                    replacement=_format_rhs_replacement(exact_rhs_raw, new_rhs),
                    occurrence=occ,
                    note=note,
                )

            i = end_line
        i += 1

    return mutants, exclusions


def _format_rhs_replacement(exact_rhs_raw: str, new_rhs: str) -> str:
    """Preserve a leading newline for multi-line style if original had one."""
    if exact_rhs_raw.startswith("\n"):
        # Keep single-line parenthesized form on the next line with indent of 8 spaces.
        indent = "        "
        # Try to reuse indent from original second line.
        parts = exact_rhs_raw.split("\n", 1)
        if len(parts) == 2:
            rest = parts[1]
            indent = rest[: len(rest) - len(rest.lstrip(" \t"))]
        return "\n" + indent + new_rhs
    # Single-line: if original started with space, keep nothing special — `= ` already
    # in head. exact_rhs_raw typically has no leading space (group 3 after `=\s*`).
    return new_rhs


def _code_part(s: str) -> str:
    """Return string with // comments and strings blanked for `;` search."""
    out = []
    i = 0
    in_str = False
    escape = False
    while i < len(s):
        c = s[i]
        if in_str:
            out.append(" ")
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_str = False
            i += 1
            continue
        if c == '"':
            in_str = True
            out.append(" ")
            i += 1
            continue
        if c == "/" and i + 1 < len(s) and s[i + 1] == "/":
            # blank until EOL
            while i < len(s) and s[i] != "\n":
                out.append(" ")
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def _find_code_semicolon(s: str) -> int:
    code = _code_part(s)
    idx = code.find(";")
    if idx < 0:
        raise ValueError("no semicolon")
    return idx


def _enclosing_mod(lines: List[str], idx: int) -> str:
    """Best-effort: nearest `pub mod name {` or `mod name {` above idx."""
    mod_re = re.compile(r"^\s*(?:pub\s+)?mod\s+([A-Za-z0-9_]+)\s*\{")
    last = ""
    depth_hint = 0
    for k in range(0, idx + 1):
        m = mod_re.match(lines[k])
        if m:
            last = m.group(1)
    # Top-level consts (PRE_REX5_...) have empty mod.
    # Heuristic: if we're before any mod or after mod closed fully, still return last open.
    # Good enough for filter keys.
    # Detect top-level: if const indent is 0 pub const at column 0.
    if lines[idx].startswith("pub const") or lines[idx].startswith("const "):
        return ""
    return last


# ---------------------------------------------------------------------------
# Apply / restore
# ---------------------------------------------------------------------------


def atomic_write_text(path: Path, text: str) -> None:
    """Write `text` to `path` via same-dir temp + flush + os.replace.

    A kill/power-loss mid-write cannot leave a half-written product file whose
    hash matches neither journal original nor mutated (which would block
    compare-and-restore). Orphaned temps match `_ATOMIC_WRITE_TMP_SUFFIX` and
    are whitelisted by cleanliness asserts; they are unlinked on failure.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + f".tmp.{os.getpid()}")
    try:
        with open(tmp, "w", encoding="utf-8") as fh:
            fh.write(text)
            fh.flush()
            os.fsync(fh.fileno())
        # `os.replace` gives the target the temp's mode. Copy the original's mode
        # first so applying/restoring a mutant cannot silently change a product
        # file's permissions — which the porcelain cleanliness check would then
        # report as a dirty tree.
        if path.is_file():
            os.chmod(tmp, path.stat().st_mode & 0o7777)
        os.replace(tmp, path)
    except Exception:
        try:
            if tmp.is_file():
                tmp.unlink()
        except OSError:
            pass
        raise


def apply_mutant(root: Path, mutant: Mutant) -> str:
    """Apply mutant text edit. Returns the mutated file content (for journaling)."""
    path = root / mutant.file
    text = path.read_text(encoding="utf-8")
    # Find the Nth occurrence of original (0-based occurrence).
    start = 0
    found_at = -1
    for n in range(mutant.occurrence + 1):
        pos = text.find(mutant.original, start)
        if pos < 0:
            raise RuntimeError(
                f"could not locate original text for {mutant.mid} "
                f"(occurrence {mutant.occurrence}, found {n})"
            )
        found_at = pos
        start = pos + len(mutant.original)
    new_text = (
        text[:found_at] + mutant.replacement + text[found_at + len(mutant.original) :]
    )
    if new_text == text:
        raise RuntimeError(f"replacement produced no change for {mutant.mid}")
    atomic_write_text(path, new_text)
    return new_text


# ---------------------------------------------------------------------------
# Crash-safe journal (apply / restore)
# ---------------------------------------------------------------------------


def journal_path(root: Path) -> Path:
    return root / DEFAULT_JOURNAL


def write_journal(
    root: Path,
    *,
    mid: str,
    file_rel: str,
    original_text: str,
    mutated_text: str,
) -> None:
    """Atomically write journal BEFORE the mutant is considered in-flight.

    Callers write the journal *before* overwriting the product file, then apply.
    On crash mid-mutant, startup recovery uses original_sha256 / mutated_sha256.
    """
    payload = {
        "mid": mid,
        "file": file_rel,
        "original_sha256": sha256_text(original_text),
        "mutated_sha256": sha256_text(mutated_text),
        # Full original bytes (utf-8) so recovery does not need git.
        "original_text": original_text,
        "written_at": datetime.now(timezone.utc).isoformat(),
    }
    path = journal_path(root)
    atomic_write_text(path, json.dumps(payload, indent=2, sort_keys=True) + "\n")


def clear_journal(root: Path) -> None:
    path = journal_path(root)
    if path.is_file():
        path.unlink()


def compare_and_restore_journal(root: Path) -> Optional[str]:
    """Shared compare-and-restore used by startup recovery and per-mutant restore.

    Hash rules (never uses `git checkout`):
    - current file hash == mutated_sha256 → write back original_text, clear journal
    - current file hash == original_sha256 → clear journal only
    - otherwise (concurrent edit / drift) → **do not touch the file**, keep
      journal, raise SystemExit so the campaign aborts with the scene intact

    Returns a short human message when a journal was present and handled,
    else None when no journal file exists.
    """
    path = journal_path(root)
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        raise SystemExit(
            f"corrupt mutation journal at {path}: {e}. "
            "Inspect manually; refusing to auto-recover. Scene preserved."
        ) from e

    file_rel = data.get("file")
    mid = data.get("mid", "?")
    original_text = data.get("original_text")
    original_sha = data.get("original_sha256")
    mutated_sha = data.get("mutated_sha256")
    if not file_rel or original_text is None or not original_sha or not mutated_sha:
        raise SystemExit(
            f"incomplete mutation journal at {path} (mid={mid}). "
            "Inspect manually; refusing to auto-recover. Scene preserved."
        )

    target = root / file_rel
    if not target.is_file():
        raise SystemExit(
            f"journal references missing file {file_rel} (mid={mid}). "
            "Inspect manually; refusing to auto-recover. Scene preserved."
        )

    current = target.read_text(encoding="utf-8")
    current_sha = sha256_text(current)
    if current_sha == original_sha:
        clear_journal(root)
        return (
            f"journal: mid={mid} file already at original hash; cleared journal"
        )
    if current_sha == mutated_sha:
        # Confirm original payload integrity before writing.
        if sha256_text(original_text) != original_sha:
            raise SystemExit(
                f"journal original_text hash mismatch for mid={mid}; "
                "refusing restore. Scene preserved."
            )
        atomic_write_text(target, original_text)
        clear_journal(root)
        return (
            f"journal: recovered mid={mid} file={file_rel} "
            f"(restored original, cleared journal)"
        )
    raise SystemExit(
        f"journal compare-and-restore REFUSED for mid={mid} file={file_rel}: "
        f"current sha256={current_sha} matches neither original "
        f"({original_sha}) nor mutated ({mutated_sha}). "
        "Possible concurrent edit — file NOT modified, journal KEPT. "
        f"Fix the file manually, then delete {path}."
    )


def recover_journal(root: Path) -> Optional[str]:
    """Startup path: if a leftover journal exists, compare-and-restore it."""
    return compare_and_restore_journal(root)


def apply_mutant_journaled(root: Path, mutant: Mutant) -> None:
    """Journal original content, apply mutant, leave journal until restore."""
    path = root / mutant.file
    original_text = path.read_text(encoding="utf-8")
    # Pre-compute mutated text without writing.
    start = 0
    found_at = -1
    for n in range(mutant.occurrence + 1):
        pos = original_text.find(mutant.original, start)
        if pos < 0:
            raise RuntimeError(
                f"could not locate original text for {mutant.mid} "
                f"(occurrence {mutant.occurrence}, found {n})"
            )
        found_at = pos
        start = pos + len(mutant.original)
    mutated_text = (
        original_text[:found_at]
        + mutant.replacement
        + original_text[found_at + len(mutant.original) :]
    )
    if mutated_text == original_text:
        raise RuntimeError(f"replacement produced no change for {mutant.mid}")

    write_journal(
        root,
        mid=mutant.mid,
        file_rel=mutant.file,
        original_text=original_text,
        mutated_text=mutated_text,
    )
    atomic_write_text(path, mutated_text)


def restore_mutant_journaled(root: Path, mutant: Mutant) -> None:
    """Regular restore path: same compare-and-restore as startup recovery.

    Does **not** use `git checkout`. Concurrent edit → SystemExit (campaign
    abort, scene preserved, journal kept). Missing journal is a no-op when the
    apply never wrote one (e.g. apply failed before journaling).
    """
    path = journal_path(root)
    if path.is_file():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            data = {}
        jmid = data.get("mid")
        if jmid is not None and jmid != mutant.mid:
            raise SystemExit(
                f"journal mid mismatch during restore: journal={jmid!r} "
                f"expected={mutant.mid!r}. Refusing restore; scene preserved."
            )
    msg = compare_and_restore_journal(root)
    if msg is None:
        # No journal: either apply never started, or journal already cleared.
        # Do not invent a restore from git — leave the tree as-is.
        return


def parse_first_failing_test(output: str) -> Optional[str]:
    """Extract first failing test name from cargo test output."""
    # Common patterns:
    #   test foo::bar ... FAILED
    #   constants::test_foo --- FAILED   (lib/integration terse format)
    # failures:
    #     foo::bar
    m = re.search(r"^test\s+(\S+)\s+\.\.\.\s+FAILED", output, re.MULTILINE)
    if m:
        return m.group(1)
    m = re.search(r"^(\S+)\s+---\s+FAILED", output, re.MULTILINE)
    if m:
        return m.group(1)
    # Summary block
    m = re.search(r"^failures:\n\n((?:    .+\n)+)", output, re.MULTILINE)
    if m:
        first = m.group(1).strip().splitlines()[0].strip()
        return first
    # cargo wrapper: "error: test failed, to rerun pass `-p mega-evm --test mutation`"
    m = re.search(
        r"error: test failed, to rerun pass `([^`]+)`", output, re.MULTILINE
    )
    if m:
        return f"<cargo: {m.group(1)}>"
    # Compile error — not a test name
    m = re.search(r"^error(\[E\d+\])?:", output, re.MULTILINE)
    if m:
        for line in output.splitlines():
            if line.startswith("error"):
                return f"<compile: {line[:120]}>"
    return None


# ---------------------------------------------------------------------------
# Oracle execution (per-layer `cargo test` with a wall-clock ceiling)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class TestTimeoutPolicy:
    """Per-layer wall-clock ceilings for one oracle `cargo test` call."""

    l1_s: float
    l2_s: float
    # Human-readable provenance, echoed to stdout / state / report.
    source: str

    def for_layer(self, layer: str) -> float:
        return self.l1_s if layer == "L1" else self.l2_s

    def describe(self) -> str:
        return f"L1={self.l1_s:g}s L2={self.l2_s:g}s ({self.source})"

    def as_dict(self) -> dict:
        return {"L1_s": self.l1_s, "L2_s": self.l2_s, "source": self.source}


def derived_timeout_s(baseline_s: float) -> float:
    """max(floor, ceil(factor x baseline)) — the documented derivation rule."""
    return float(
        max(TEST_TIMEOUT_FLOOR_S, math.ceil(TEST_TIMEOUT_FACTOR * baseline_s))
    )


def _baseline_layer_duration(baseline: Optional[dict], layer: str) -> Optional[float]:
    """Clean-tree wall time for one layer from a passed baseline record."""
    if not isinstance(baseline, dict) or baseline.get("status") != "passed":
        return None
    entry = baseline.get(layer)
    if not isinstance(entry, dict):
        return None
    value = entry.get("duration_s")
    if not isinstance(value, (int, float)) or value <= 0:
        return None
    return float(value)


def resolve_test_timeout(
    explicit_s: Optional[float], baseline: Optional[dict]
) -> TestTimeoutPolicy:
    """Resolve the per-layer test timeout. Explicit always wins.

    1. `--test-timeout N` → both layers use N.
    2. else, a passed baseline → per layer
       `max(TEST_TIMEOUT_FLOOR_S, ceil(TEST_TIMEOUT_FACTOR x layer baseline))`.
    3. else (no baseline wall time, e.g. --skip-baseline) → both layers use the
       absolute default `TEST_TIMEOUT_NO_BASELINE_S`.
    """
    if explicit_s is not None:
        return TestTimeoutPolicy(
            l1_s=explicit_s,
            l2_s=explicit_s,
            source=f"explicit --test-timeout={explicit_s:g}s",
        )
    d1 = _baseline_layer_duration(baseline, "L1")
    d2 = _baseline_layer_duration(baseline, "L2")
    if d1 is None or d2 is None:
        return TestTimeoutPolicy(
            l1_s=float(TEST_TIMEOUT_NO_BASELINE_S),
            l2_s=float(TEST_TIMEOUT_NO_BASELINE_S),
            source=(
                f"no baseline wall time; absolute default "
                f"{TEST_TIMEOUT_NO_BASELINE_S}s"
            ),
        )
    return TestTimeoutPolicy(
        l1_s=derived_timeout_s(d1),
        l2_s=derived_timeout_s(d2),
        source=(
            f"derived from baseline (L1={d1:.1f}s, L2={d2:.1f}s): "
            f"max({TEST_TIMEOUT_FLOOR_S}s, ceil({TEST_TIMEOUT_FACTOR} x baseline))"
        ),
    )


def kill_process_group(proc: "subprocess.Popen", grace_s: float) -> str:
    """Kill a timed-out child **and every process in its group**.

    `cargo` spawns rustc and test binaries; signalling only the direct child
    leaves them running and holding the `target/` lock. The child is started
    with `start_new_session=True`, so its pid is the group leader and the whole
    tree can be signalled at once: SIGTERM, short grace, then SIGKILL.

    Returns a short description of what it took (for the layer log).
    """
    try:
        pgid = os.getpgid(proc.pid)
    except OSError:
        pgid = None

    def signal_group(sig: int) -> None:
        try:
            if pgid is not None:
                os.killpg(pgid, sig)
            else:
                proc.send_signal(sig)
        except (ProcessLookupError, PermissionError, OSError):
            pass

    signal_group(signal.SIGTERM)
    try:
        proc.wait(timeout=grace_s)
        return "process group killed with SIGTERM"
    except subprocess.TimeoutExpired:
        pass
    signal_group(signal.SIGKILL)
    try:
        proc.wait(timeout=grace_s)
        return "process group killed with SIGKILL (SIGTERM grace expired)"
    except subprocess.TimeoutExpired:
        return "process group SIGKILLed but child did not reap within grace"


@dataclass
class OracleLayerOutcome:
    """Result of one oracle `cargo test` call."""

    passed: bool
    # First failing test / compile error / timeout label; None when passed.
    detail: Optional[str]
    duration_s: float
    timed_out: bool = False
    # Ceiling that fired; only meaningful when timed_out.
    timeout_s: Optional[float] = None


def run_oracle_layer(
    root: Path,
    layer: str,
    package: str,
    log_path: Path,
    *,
    timeout_s: float,
) -> OracleLayerOutcome:
    """Run one cargo test layer under a wall-clock ceiling.

    The child runs in its own session (`start_new_session=True`) so a timeout
    can kill cargo together with every rustc / test binary it spawned. A
    timeout is a **failure** of the layer (`passed=False`) flagged with
    `timed_out=True`; callers classify it as a kill of kind `timeout`.
    """
    cmd = ["cargo", "test", "-p", package, "--quiet"]
    t0 = time.monotonic()
    proc = subprocess.Popen(
        cmd,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
        start_new_session=True,
    )
    timed_out = False
    kill_note = ""
    try:
        try:
            stdout, stderr = proc.communicate(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_note = kill_process_group(proc, TEST_TIMEOUT_KILL_GRACE_S)
            try:
                stdout, stderr = proc.communicate(timeout=TEST_TIMEOUT_KILL_GRACE_S)
            except subprocess.TimeoutExpired:
                # Something outside the killed group still holds the write end.
                # Drop our read ends so a long campaign cannot leak descriptors.
                stdout, stderr = "", ""
                for stream in (proc.stdout, proc.stderr):
                    if stream is not None:
                        try:
                            stream.close()
                        except OSError:
                            pass
    except BaseException:
        # Ctrl-C / abort: the child is in its own group and would not receive
        # the terminal signal, so tear the group down before propagating.
        kill_process_group(proc, TEST_TIMEOUT_KILL_GRACE_S)
        raise
    duration = time.monotonic() - t0
    returncode = proc.returncode if proc.returncode is not None else -signal.SIGKILL
    out = (stdout or "") + "\n" + (stderr or "")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    header = (
        f"$ {' '.join(cmd)}\n"
        f"exit={returncode}\n"
        f"duration_s={duration:.3f}\n"
        f"timeout_s={timeout_s:g}\n"
        f"timed_out={'true' if timed_out else 'false'}"
    )
    if timed_out:
        header += f" ({kill_note})"
    log_path.write_text(f"{header}\n\n{out}", encoding="utf-8")
    if timed_out:
        return OracleLayerOutcome(
            passed=False,
            detail=f"<timeout after {timeout_s:g}s>",
            duration_s=duration,
            timed_out=True,
            timeout_s=timeout_s,
        )
    if returncode == 0:
        return OracleLayerOutcome(passed=True, detail=None, duration_s=duration)
    return OracleLayerOutcome(
        passed=False,
        detail=parse_first_failing_test(out) or f"<exit {returncode}>",
        duration_s=duration,
    )


def run_mutant(
    root: Path,
    mutant: Mutant,
    *,
    is_sentinel: bool = False,
    timeouts: TestTimeoutPolicy,
) -> MutantResult:
    """Apply → L1 → (if live) L2 → compare-and-restore (journal hash logic).

    Restore always runs in `finally`, including on the timeout path: a mutant
    that hangs an oracle layer is killed (kind `timeout`) and its file is
    restored through the same journal compare-and-restore as any other
    outcome. Concurrent-edit refusal raises `SystemExit` (propagates out of the
    campaign). Other apply/oracle errors become a MutantResult with
    status=error so the campaign can continue.
    """
    t0 = time.monotonic()
    base_log = LOG_DIR / mutant.mid.replace("/", "_").replace(":", "__")

    def killed(layer: str, outcome: OracleLayerOutcome) -> MutantResult:
        return MutantResult(
            mid=mutant.mid,
            operator=mutant.operator,
            file=mutant.file,
            line=mutant.line,
            site=mutant.site,
            body=mutant.body,
            status="killed",
            kill_layer=layer,
            first_failing_test=outcome.detail,
            duration_s=time.monotonic() - t0,
            is_sentinel=is_sentinel,
            note=mutant.note,
            kill_kind="timeout" if outcome.timed_out else "assertion",
            timed_out=outcome.timed_out,
            timeout_s=outcome.timeout_s,
        )

    try:
        apply_mutant_journaled(root, mutant)
        # L1
        out1 = run_oracle_layer(
            root,
            "L1",
            ORACLE_L1_PACKAGE,
            Path(str(base_log) + ".L1.log"),
            timeout_s=timeouts.for_layer("L1"),
        )
        if not out1.passed:
            return killed("L1", out1)
        # L2
        out2 = run_oracle_layer(
            root,
            "L2",
            ORACLE_L2_PACKAGE,
            Path(str(base_log) + ".L2.log"),
            timeout_s=timeouts.for_layer("L2"),
        )
        if not out2.passed:
            return killed("L2", out2)
        return MutantResult(
            mid=mutant.mid,
            operator=mutant.operator,
            file=mutant.file,
            line=mutant.line,
            site=mutant.site,
            body=mutant.body,
            status="survived",
            kill_layer=None,
            first_failing_test=None,
            duration_s=time.monotonic() - t0,
            is_sentinel=is_sentinel,
            note=mutant.note,
        )
    except Exception as e:  # noqa: BLE001 — harness must not abort the campaign
        return MutantResult(
            mid=mutant.mid,
            operator=mutant.operator,
            file=mutant.file,
            line=mutant.line,
            site=mutant.site,
            body=mutant.body,
            status="error",
            duration_s=time.monotonic() - t0,
            error=str(e),
            is_sentinel=is_sentinel,
            note=mutant.note,
        )
    finally:
        try:
            restore_mutant_journaled(root, mutant)
        except SystemExit as se:
            # Concurrent-edit / journal integrity: keep scene, abort campaign.
            sys.stderr.write(f"RESTORE REFUSED for {mutant.file}: {se}\n")
            raise
        except Exception as rexc:  # noqa: BLE001
            sys.stderr.write(f"RESTORE FAILED for {mutant.file}: {rexc}\n")
            raise


def run_baseline(root: Path, *, timeout_s: float) -> dict:
    """Run unmutated L1+L2. Abort (SystemExit) if either fails or times out.

    The baseline cannot use the derived per-layer ceiling (it *is* the
    measurement that ceiling comes from), so it runs under the explicit
    `--test-timeout` when given and otherwise the absolute default.
    """
    log_base = LOG_DIR / "_baseline"
    print(
        f"[baseline] running clean-tree L1 (mega-evm), timeout={timeout_s:g}s ...",
        flush=True,
    )
    out1 = run_oracle_layer(
        root, "L1", ORACLE_L1_PACKAGE, Path(str(log_base) + ".L1.log"),
        timeout_s=timeout_s,
    )
    if not out1.passed:
        kind = "TIMED OUT" if out1.timed_out else "FAILED"
        raise SystemExit(
            f"BASELINE {kind} at L1 (clean tree must pass before campaign).\n"
            f"  first_fail={out1.detail}\n"
            f"  log={log_base}.L1.log\n"
            f"Fix the tree (or raise --test-timeout), then re-run "
            f"(do not --resume a failed baseline)."
        )
    print(
        f"[baseline] L1 ok ({out1.duration_s:.1f}s); "
        f"running L2 (mega-state-test) ...",
        flush=True,
    )
    out2 = run_oracle_layer(
        root, "L2", ORACLE_L2_PACKAGE, Path(str(log_base) + ".L2.log"),
        timeout_s=timeout_s,
    )
    if not out2.passed:
        kind = "TIMED OUT" if out2.timed_out else "FAILED"
        raise SystemExit(
            f"BASELINE {kind} at L2 (clean tree must pass before campaign).\n"
            f"  first_fail={out2.detail}\n"
            f"  log={log_base}.L2.log\n"
            f"Fix the tree (or raise --test-timeout), then re-run "
            f"(do not --resume a failed baseline)."
        )
    print(f"[baseline] L2 ok ({out2.duration_s:.1f}s)", flush=True)
    return {
        "status": "passed",
        "L1": {"ok": True, "duration_s": out1.duration_s, "first_fail": None},
        "L2": {"ok": True, "duration_s": out2.duration_s, "first_fail": None},
        "timeout_s": timeout_s,
        "completed_at": datetime.now(timezone.utc).isoformat(),
    }


# ---------------------------------------------------------------------------
# State / resume
# ---------------------------------------------------------------------------


def load_state(path: Path) -> dict:
    if not path.is_file():
        return {"completed": {}, "results": []}
    return json.loads(path.read_text(encoding="utf-8"))


def save_state(path: Path, state: dict) -> None:
    """Atomic write: temp file in the same directory + os.replace."""
    payload = json.dumps(state, indent=2, sort_keys=True) + "\n"
    atomic_write_text(path, payload)


def validate_resume_state(
    state: dict,
    *,
    head: str,
    inventory_hash: str,
    oracle_command: Sequence[str],
) -> None:
    """Refuse --resume when state is unbound or mismatches the current tree/oracle."""
    binding = state.get("binding")
    if not isinstance(binding, dict):
        raise SystemExit(
            "RESUME REJECTED: state file has no `binding` block "
            "(HEAD / inventory_hash / oracle_command). "
            "This state was written by a pre-M4 harness or is incomplete. "
            "Re-run without --resume, or pass --fresh to start a new campaign."
        )
    reasons = []
    if binding.get("head") != head:
        reasons.append(
            f"HEAD mismatch: state={binding.get('head')!r} current={head!r}"
        )
    if binding.get("inventory_hash") != inventory_hash:
        reasons.append(
            f"inventory_hash mismatch: state={binding.get('inventory_hash')!r} "
            f"current={inventory_hash!r}"
        )
    state_oracle = binding.get("oracle_command")
    if state_oracle != list(oracle_command):
        reasons.append(
            f"oracle_command mismatch: state={state_oracle!r} "
            f"current={list(oracle_command)!r}"
        )
    if reasons:
        detail = "\n  - ".join(reasons)
        raise SystemExit(
            "RESUME REJECTED: state binding does not match current tree/oracle.\n"
            f"  - {detail}\n"
            "Re-run without --resume, or pass --fresh to discard the state and start over."
        )


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def format_list_table(mutants: Sequence[Mutant]) -> str:
    lines = [
        "| # | mid | operator | file:line | site | body |",
        "|---|-----|----------|-----------|------|------|",
    ]
    for i, m in enumerate(mutants, 1):
        lines.append(
            f"| {i} | `{m.mid}` | {m.operator} | `{m.file}:{m.line}` | `{m.site}` | {m.body} |"
        )
    return "\n".join(lines)


def format_results_table(results: Sequence[MutantResult]) -> str:
    lines = [
        "| # | mid | op | site | body | status | kind | layer | first fail | time (s) | sentinel |",
        "|---|-----|----|-----|------|--------|------|-------|------------|----------|----------|",
    ]
    for i, r in enumerate(results, 1):
        lines.append(
            f"| {i} | `{r.mid}` | {r.operator} | `{r.site}` | {r.body} | **{r.status}** | "
            f"{r.kill_kind or '—'} | "
            f"{r.kill_layer or '—'} | `{r.first_failing_test or '—'}` | {r.duration_s:.1f} | "
            f"{'yes' if r.is_sentinel else ''} |"
        )
    return "\n".join(lines)


def write_report(
    path: Path,
    *,
    title: str,
    mutants_all: Sequence[Mutant],
    results: Sequence[MutantResult],
    exclusions: Sequence[str],
    sentinels: Sequence[str],
    extra_sections: Sequence[Tuple[str, str]] = (),
    git_status_sample: str = "",
    projection: str = "",
    timeout_policy: str = "",
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    killed = sum(1 for r in results if r.status == "killed")
    survived = sum(1 for r in results if r.status == "survived")
    errors = sum(1 for r in results if r.status == "error")
    timeouts = [r for r in results if r.timed_out]
    assertion_kills = killed - len(timeouts)
    total_time = sum(r.duration_s for r in results)

    sg = [m for m in mutants_all if m.operator == "spec_gate"]
    gc = [m for m in mutants_all if m.operator == "gas_const"]
    adj = [m for m in mutants_all if m.operator == "adjacent_spec"]
    cd = [m for m in mutants_all if m.operator == "call_delete"]
    # each spec_gate site has 2 bodies
    sg_points = len(sg) // 2
    gc_names = len({m.site for m in gc})
    adj_sites = len({(m.file, m.line, m.site.split("#")[0]) for m in adj})
    cd_sites = len(cd)

    parts = [
        f"# {title}",
        "",
        f"Generated: {now}",
        "",
        "## Summary",
        "",
        f"- Enumerated mutants (full): **{len(mutants_all)}** "
        f"(spec_gate={len(sg)}, gas_const={len(gc)}, "
        f"adjacent_spec={len(adj)}, call_delete={len(cd)})",
        f"- Spec-gate sites (points × 2 bodies): **{sg_points}** points → {len(sg)} mutants",
        f"- Gas-const sites: **{gc_names}** constants → {len(gc)} mutants "
        f"(−1 skipped on literal 0)",
        f"- Adjacent-spec sites: **{adj_sites}** points → {len(adj)} mutants "
        f"(pred/succ; ends omit one body)",
        f"- Call-delete sites: **{cd_sites}** statement deletions",
        f"- Subset run: **{len(results)}** mutants "
        f"(killed={killed} [assertion={assertion_kills}, "
        f"timeout={len(timeouts)}], survived={survived}, error={errors})",
        f"- Wall time (sum of mutant durations): **{total_time:.1f}s** "
        f"({total_time / 60:.1f} min)",
        "",
        "## Test timeout",
        "",
        f"Policy: {timeout_policy or '_not recorded_'}",
        "",
        f"Timeout kills: **{len(timeouts)}** (assertion kills: {assertion_kills}).",
        "A timeout counts as a kill — the mutant hangs the suite, so it is",
        "detected — but is listed separately: a rising timeout count means the",
        "threshold may be too tight rather than the tests being strong.",
        "",
    ]
    if timeouts:
        for r in timeouts:
            parts.append(
                f"- `{r.mid}`: layer={r.kill_layer or '—'}, "
                f"ceiling={r.timeout_s if r.timeout_s is not None else '—'}s, "
                f"observed={r.duration_s:.1f}s"
            )
    else:
        parts.append("_No mutant hit the timeout in this run._")
    parts += [
        "",
        "## Sentinel verification",
        "",
    ]
    if not sentinels:
        parts.append("_No sentinels marked in this run._")
    else:
        parts.append("Sentinels are mutants pre-judged as *must-kill*. Survival = harness bug.")
        parts.append("")
        for sid in sentinels:
            matches = [r for r in results if r.mid == sid or sid in r.mid]
            if not matches:
                parts.append(f"- `{sid}`: **NOT RUN**")
                continue
            r = matches[0]
            ok = r.status == "killed"
            parts.append(
                f"- `{r.mid}`: **{'KILLED (ok)' if ok else 'SURVIVED / ERROR (harness bug?)'}** "
                f"— kind={r.kill_kind or '—'}, layer={r.kill_layer or '—'}, "
                f"fail=`{r.first_failing_test or r.error or '—'}`, "
                f"t={r.duration_s:.1f}s"
            )
    parts += [
        "",
        "## Per-mutant results",
        "",
        format_results_table(results),
        "",
        "## Git cleanliness check",
        "",
        "After baseline and after each mutant restore the harness runs",
        "`git status --porcelain` on the **full** worktree.",
        "Whitelist is harness **runtime artifacts** only:",
        "`tools/mutation/reports/`, `tools/mutation/logs/`,",
        "`tools/mutation/state*.json`, `tools/mutation/.mutate-journal.json`",
        "(+tmp). Harness sources (`mutate.py`, `README.md`, …) are **not**",
        "whitelisted. Rename/copy porcelain lines check **both** paths.",
        "Product-file restore uses journal compare-and-restore (hash vs",
        "original/mutated); it does **not** use `git checkout`.",
        "Unexpected dirty paths abort the campaign and preserve the scene.",
        "End-of-run status sample:",
        "",
        "```",
        git_status_sample.rstrip() or "(clean)",
        "```",
        "",
        "## Exclusions / notes",
        "",
    ]
    if exclusions:
        for e in exclusions:
            parts.append(f"- {e}")
    else:
        parts.append("_None._")

    parts += [
        "",
        "## Full inventory (`--list`)",
        "",
        f"Total bodies: {len(mutants_all)}",
        "",
        format_list_table(mutants_all),
        "",
        "## Full-run time projection",
        "",
        projection or "_Not computed._",
        "",
    ]
    for heading, body in extra_sections:
        parts += [f"## {heading}", "", body, ""]

    path.write_text("\n".join(parts) + "\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def pick_sentinels(mutants: Sequence[Mutant]) -> List[Mutant]:
    """Select ≥2 sentinel mutants that we expect tests to kill.

    Notes on sentinel choice (M4):
    - `STORAGE_CALL_STIPEND ±1` is often an *equivalent* mutant: REX4 burn +
      rescue-exclusion cancel a ±1 change in unused stipend. Do not use as sentinel.
    - Prefer constants pinned by `tests/mutation/constants.rs` (exact
      `assert_eq!` on the evaluated value).
    - Spec-gate sentinels must be **product-code** gates (cfg(test) / assert!
      sites are excluded from enumeration). Prefer `host.rs` REX6
      `load_account_delegated` (killed by M3 unit tests) or execution-path REX6.
    """
    chosen: List[Mutant] = []

    # 1) Gas: mini_rex::BLOCK_DATA_LIMIT +1 — pinned to 13_107_200.
    for m in mutants:
        if (
            m.operator == "gas_const"
            and m.site.endswith("BLOCK_DATA_LIMIT")
            and "mini_rex" in m.site
            and m.body == "+1"
        ):
            chosen.append(m)
            break

    # 2) Gas: rex::TX_DATA_LIMIT -1 — pinned to 13_107_200.
    for m in mutants:
        if (
            m.operator == "gas_const"
            and m.site == "rex::TX_DATA_LIMIT"
            and m.body == "-1"
        ):
            chosen.append(m)
            break

    # 3) Product gate: host.rs load_account_delegated REX6 → false
    #    (M3 unit test pins both polarities; false is the stronger "feature off" kill).
    for m in mutants:
        if (
            m.operator == "spec_gate"
            and m.file.endswith("evm/host.rs")
            and m.line == 252
            and "REX6" in m.site
            and m.body == "false"
        ):
            chosen.append(m)
            break

    # 4) Fallback product gate: any host.rs REX6 → false.
    if not any(c.operator == "spec_gate" for c in chosen):
        for m in mutants:
            if (
                m.operator == "spec_gate"
                and m.file.endswith("evm/host.rs")
                and "REX6" in m.site
                and m.body == "false"
            ):
                chosen.append(m)
                break

    # 5) Fallback: REX6 → false on execution path (instructions.rs product code).
    if not any(c.operator == "spec_gate" for c in chosen):
        for m in mutants:
            if (
                m.operator == "spec_gate"
                and m.file.endswith("evm/instructions.rs")
                and "REX6" in m.site
                and m.body == "false"
            ):
                chosen.append(m)
                break

    # De-dupe preserving order
    seen = set()
    out = []
    for m in chosen:
        if m.mid not in seen:
            seen.add(m.mid)
            out.append(m)
    return out


def select_subset(
    mutants: Sequence[Mutant],
    *,
    limit: int,
    filter_substr: Optional[str],
    prefer_sentinels: Sequence[Mutant],
) -> List[Mutant]:
    pool = list(mutants)
    if filter_substr:
        pool = [m for m in pool if m.matches_filter(filter_substr)]

    selected: List[Mutant] = []
    seen = set()

    def add(m: Mutant) -> None:
        if m.mid not in seen and (not filter_substr or m.matches_filter(filter_substr)):
            # If filter active, only add matching sentinels.
            selected.append(m)
            seen.add(m.mid)

    for m in prefer_sentinels:
        if filter_substr and not m.matches_filter(filter_substr):
            continue
        add(m)

    # Mix operators for diversity (legacy + new).
    buckets: Dict[str, List[Mutant]] = {
        "spec_gate": [m for m in pool if m.operator == "spec_gate" and m.mid not in seen],
        "gas_const": [m for m in pool if m.operator == "gas_const" and m.mid not in seen],
        "adjacent_spec": [
            m for m in pool if m.operator == "adjacent_spec" and m.mid not in seen
        ],
        "call_delete": [
            m for m in pool if m.operator == "call_delete" and m.mid not in seen
        ],
    }
    # Prefer REX6/REX5 gates, STORAGE/SSTORE-ish constants, check_limit deletions.
    buckets["spec_gate"].sort(
        key=lambda m: (
            0 if "REX6" in m.site else 1 if "REX5" in m.site else 2,
            m.mid,
        )
    )
    buckets["gas_const"].sort(
        key=lambda m: (
            0
            if "STIPEND" in m.site or "SSTORE" in m.site or "GAS" in m.site
            else 1,
            m.mid,
        )
    )
    buckets["adjacent_spec"].sort(
        key=lambda m: (
            0 if "REX6" in m.site else 1 if "REX5" in m.site else 2,
            m.mid,
        )
    )
    buckets["call_delete"].sort(
        key=lambda m: (
            0 if m.body == "check_limit" else 1 if "frame" in m.body else 2,
            m.mid,
        )
    )

    order = ("spec_gate", "gas_const", "adjacent_spec", "call_delete")
    idxs = {k: 0 for k in order}
    while len(selected) < limit and any(
        idxs[k] < len(buckets[k]) for k in order
    ):
        for k in order:
            if len(selected) >= limit:
                break
            i = idxs[k]
            if i < len(buckets[k]):
                add(buckets[k][i])
                idxs[k] = i + 1

    # Fill remainder in inventory order
    if len(selected) < limit:
        for m in pool:
            if len(selected) >= limit:
                break
            add(m)

    return selected[:limit]


def positive_seconds(raw: str) -> float:
    """argparse type for `--test-timeout`: a strictly positive number of seconds."""
    try:
        value = float(raw)
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"not a number of seconds: {raw!r}") from e
    if not math.isfinite(value) or value <= 0:
        raise argparse.ArgumentTypeError(
            f"timeout must be a finite value > 0 seconds, got {raw!r}"
        )
    return value


def main(argv: Optional[Sequence[str]] = None) -> int:
    ap = argparse.ArgumentParser(
        description=(
            "MegaETH domain mutator v0 "
            "(spec_gate, gas_const, adjacent_spec, call_delete)"
        )
    )
    ap.add_argument("--list", action="store_true", help="Enumerate mutants and exit")
    ap.add_argument("--filter", type=str, default=None, help="Substring filter (file/const/spec)")
    ap.add_argument("--limit", type=int, default=None, help="Max mutants to run")
    ap.add_argument("--resume", action="store_true", help="Resume from state JSON")
    ap.add_argument(
        "--fresh",
        action="store_true",
        help="Ignore any existing state file and start a new campaign "
        "(incompatible with --resume)",
    )
    ap.add_argument(
        "--state",
        type=Path,
        default=DEFAULT_STATE,
        help=f"State JSON path (default: {DEFAULT_STATE})",
    )
    ap.add_argument(
        "--report",
        type=Path,
        default=None,
        help=f"Write markdown report (default: {DEFAULT_REPORT} when running)",
    )
    ap.add_argument(
        "--title",
        type=str,
        default="Domain mutator run",
        help="Report title",
    )
    ap.add_argument(
        "--no-sentinel-priority",
        action="store_true",
        help="Do not force sentinels to the front of the run queue",
    )
    ap.add_argument(
        "--skip-clean-check",
        action="store_true",
        help="Skip initial worktree cleanliness assertion (debug only)",
    )
    ap.add_argument(
        "--skip-baseline",
        action="store_true",
        help="Skip clean-tree L1+L2 baseline (debug only; not for real campaigns)",
    )
    ap.add_argument(
        "--test-timeout",
        type=positive_seconds,
        default=None,
        metavar="SECONDS",
        help=(
            "Wall-clock ceiling for each oracle `cargo test` call. "
            "Default per layer: "
            f"max({TEST_TIMEOUT_FLOOR_S}s, ceil({TEST_TIMEOUT_FACTOR} x that "
            "layer's baseline wall time)); with --skip-baseline (no baseline "
            f"wall time) the absolute default {TEST_TIMEOUT_NO_BASELINE_S}s is "
            "used. A layer that exceeds its ceiling has its whole process "
            "group killed and the mutant is recorded as killed/timeout."
        ),
    )
    args = ap.parse_args(argv)

    if args.resume and args.fresh:
        raise SystemExit("incompatible flags: --resume and --fresh")

    root = repo_root()
    os.chdir(root)

    # Crash-safe recovery before any enumeration/apply (may rewrite product files).
    recovery_msg = recover_journal(root)
    if recovery_msg:
        print(f"[recover] {recovery_msg}", flush=True)

    sg_mutants, sg_excl = enumerate_spec_gate(root)
    gc_mutants, gc_excl = enumerate_gas_const(root)
    adj_mutants, adj_excl = enumerate_adjacent_spec(root)
    cd_mutants, cd_excl, cd_stats = enumerate_call_delete(root)
    all_mutants = sg_mutants + gc_mutants + adj_mutants + cd_mutants
    exclusions = sg_excl + gc_excl + adj_excl + cd_excl
    inv_hash = inventory_fingerprint(all_mutants)
    head = git_head(root)

    if args.filter:
        filtered = [m for m in all_mutants if m.matches_filter(args.filter)]
    else:
        filtered = list(all_mutants)

    if args.list:
        print("# Domain mutator inventory")
        print(f"spec_gate: {len(sg_mutants)} bodies ({len(sg_mutants)//2} sites × 2)")
        print(f"gas_const: {len(gc_mutants)} bodies")
        # adjacent_spec: up to 2 bodies per site; ends omit one.
        adj_points = len({(m.file, m.line) for m in adj_mutants})
        print(
            f"adjacent_spec: {len(adj_mutants)} bodies "
            f"({adj_points} sites, pred/succ; ends omit one body)"
        )
        print(f"call_delete: {len(cd_mutants)} bodies (1 per statement site)")
        print(f"total: {len(all_mutants)}")
        print(f"inventory_hash: {inv_hash}")
        print(f"HEAD: {head}")
        if args.filter:
            print(f"filter={args.filter!r} → {len(filtered)}")
        # Per-operator file counts (top files).
        print("\n# Per-operator body counts by file (top 10)")
        for op_name, op_list in (
            ("spec_gate", sg_mutants),
            ("gas_const", gc_mutants),
            ("adjacent_spec", adj_mutants),
            ("call_delete", cd_mutants),
        ):
            by_file: Dict[str, int] = {}
            for m in op_list:
                by_file[m.file] = by_file.get(m.file, 0) + 1
            top = sorted(by_file.items(), key=lambda kv: (-kv[1], kv[0]))[:10]
            print(f"## {op_name} ({len(op_list)} total)")
            for f, n in top:
                print(f"  {n:4d}  {f}")
        print("\n# call_delete per-callee (enumerated / skipped)")
        for c in CALL_DELETE_CALLEES:
            st = cd_stats[c]
            print(f"  {c}: enumerated={st['enumerated']} skipped={st['skipped']}")
        print()
        print(format_list_table(filtered if args.filter else all_mutants))
        if exclusions:
            print("\n# Exclusions")
            for e in exclusions:
                print(f"- {e}")
        if args.report:
            write_report(
                root / args.report if not args.report.is_absolute() else args.report,
                title=args.title + " (list only)",
                mutants_all=all_mutants,
                results=[],
                exclusions=exclusions,
                sentinels=[],
                projection="List-only mode; no timing.",
            )
        return 0

    # Run mode
    if not args.skip_clean_check:
        assert_tree_clean(root, allow_harness=True)

    sentinels = pick_sentinels(all_mutants)
    sentinel_ids = {m.mid for m in sentinels}
    by_mid = {m.mid: m for m in all_mutants}

    state_path = root / args.state if not args.state.is_absolute() else args.state

    if args.resume:
        if not state_path.is_file():
            raise SystemExit(f"--resume requested but state file missing: {state_path}")
        state = load_state(state_path)
        validate_resume_state(
            state,
            head=head,
            inventory_hash=inv_hash,
            oracle_command=ORACLE_COMMAND,
        )
        # Use the saved queue (required when present); rebuild Mutant objects from inventory.
        saved_queue = state.get("queue")
        if not isinstance(saved_queue, list) or not saved_queue:
            raise SystemExit(
                "RESUME REJECTED: state has no usable `queue` list. "
                "Re-run with --fresh to start a new campaign."
            )
        queue = []
        for mid in saved_queue:
            m = by_mid.get(mid)
            if m is None:
                raise SystemExit(
                    f"RESUME REJECTED: queue mid not in current inventory: {mid!r}. "
                    "Inventory binding should have caught this; re-run with --fresh."
                )
            queue.append(m)
        print(
            f"[resume] bound HEAD={head[:12]} inventory_hash={inv_hash[:12]}… "
            f"queue={len(queue)} completed={len(state.get('completed', {}))}",
            flush=True,
        )
    else:
        limit = args.limit if args.limit is not None else len(filtered)
        if args.no_sentinel_priority:
            queue = filtered[:limit]
        else:
            queue = select_subset(
                all_mutants if not args.filter else filtered,
                limit=limit,
                filter_substr=args.filter,
                prefer_sentinels=sentinels,
            )
        if args.fresh and state_path.is_file():
            state_path.unlink()
            print(f"[fresh] removed prior state {state_path}", flush=True)
        state = {
            "completed": {},
            "results": [],
            "started_at": datetime.now(timezone.utc).isoformat(),
            "queue": [m.mid for m in queue],
            "binding": {
                "head": head,
                "inventory_hash": inv_hash,
                "oracle_command": list(ORACLE_COMMAND),
            },
        }

    # Unmutated baseline: required once per campaign (recorded in state).
    # The baseline itself cannot use the derived ceiling (it is the measurement
    # the ceiling derives from): explicit --test-timeout, else absolute default.
    baseline_timeout = (
        args.test_timeout
        if args.test_timeout is not None
        else float(TEST_TIMEOUT_NO_BASELINE_S)
    )
    if not args.skip_baseline:
        if state.get("baseline", {}).get("status") == "passed":
            print("[baseline] already recorded in state; skipping re-run", flush=True)
        else:
            state["baseline"] = run_baseline(root, timeout_s=baseline_timeout)
            # Full-tree clean after baseline (oracle must not leave product dirt).
            if not args.skip_clean_check:
                assert_tree_clean(
                    root, allow_harness=True, context="after baseline"
                )
            state["updated_at"] = datetime.now(timezone.utc).isoformat()
            save_state(state_path, state)
    else:
        print("[baseline] SKIPPED (--skip-baseline)", flush=True)
        state.setdefault(
            "baseline",
            {"status": "skipped", "note": "--skip-baseline"},
        )

    # Per-layer oracle ceilings: explicit flag > baseline-derived > absolute
    # default. Recorded in state so a report/auditor can see what was in force.
    timeouts = resolve_test_timeout(args.test_timeout, state.get("baseline"))
    state["test_timeout"] = timeouts.as_dict()
    print(f"[timeout] {timeouts.describe()}", flush=True)

    completed: Dict[str, dict] = state.get("completed", {})
    results: List[MutantResult] = []
    known_result_fields = {f.name for f in dc_fields(MutantResult)}
    for rd in state.get("results", []):
        # Tolerate extra keys from older state by filtering to known fields.
        results.append(
            MutantResult(**{k: v for k, v in rd.items() if k in known_result_fields})
        )

    # Persist binding + queue at campaign start (and after baseline) so a crash mid-queue
    # still has a resume-safe state file.
    state["queue"] = [m.mid for m in queue]
    state["binding"] = {
        "head": head,
        "inventory_hash": inv_hash,
        "oracle_command": list(ORACLE_COMMAND),
    }
    state["updated_at"] = datetime.now(timezone.utc).isoformat()
    save_state(state_path, state)

    print(
        f"queue={len(queue)} completed={len(completed)} "
        f"sentinels={[m.mid for m in sentinels]}",
        flush=True,
    )

    for m in queue:
        if m.mid in completed:
            print(f"[skip-resume] {m.mid}", flush=True)
            continue
        print(f"[run] {m.mid} ...", flush=True)
        # run_mutant always compare-and-restores in finally; concurrent-edit
        # refusal raises SystemExit and aborts the campaign with scene preserved.
        res = run_mutant(
            root, m, is_sentinel=m.mid in sentinel_ids, timeouts=timeouts
        )

        # Full-tree clean after each restore (catches collateral writes during
        # oracle). Unexpected dirt → abort; do not scrub the tree.
        if not args.skip_clean_check:
            assert_tree_clean(
                root,
                allow_harness=True,
                context=f"after mutant {m.mid}",
            )

        results.append(res)
        completed[m.mid] = asdict(res)
        state["completed"] = completed
        state["results"] = [asdict(r) for r in results]
        state["updated_at"] = datetime.now(timezone.utc).isoformat()
        # Queue remains the campaign plan (used on --resume); do not drop it mid-run.
        save_state(state_path, state)

        flag = "SENTINEL " if res.is_sentinel else ""
        print(
            f"[done] {flag}{res.status} kind={res.kill_kind} "
            f"layer={res.kill_layer} "
            f"fail={res.first_failing_test} t={res.duration_s:.1f}s",
            flush=True,
        )

    # Campaign finished: drop queue so a leftover state is not half-stale plan data.
    # Completed results + binding remain for report / audit.
    if "queue" in state:
        del state["queue"]
        state["queue_cleared_at"] = datetime.now(timezone.utc).isoformat()
        save_state(state_path, state)

    # Projection for full run
    if results:
        avg = sum(r.duration_s for r in results) / len(results)
        full_n = len(all_mutants)
        proj = (
            f"Based on {len(results)} completed mutants, mean wall time ≈ **{avg:.1f}s** / mutant.\n"
            f"Full inventory size = **{full_n}** → projected total ≈ "
            f"**{avg * full_n / 3600:.1f} hours** ({avg * full_n / 60:.0f} min).\n"
            f"Use `--resume` to continue a partial campaign (binding must match).\n"
            f"\nBreakdown of this run: "
            f"killed={sum(1 for r in results if r.status=='killed')} "
            f"(assertion={sum(1 for r in results if r.status=='killed' and not r.timed_out)}, "
            f"timeout={sum(1 for r in results if r.timed_out)}), "
            f"survived={sum(1 for r in results if r.status=='survived')}, "
            f"error={sum(1 for r in results if r.status=='error')}."
        )
    else:
        proj = "No results; cannot project."

    baseline_section = json.dumps(state.get("baseline", {}), indent=2, sort_keys=True)
    binding_section = json.dumps(state.get("binding", {}), indent=2, sort_keys=True)

    status_sample = git_status_short(root)
    report_path = args.report or DEFAULT_REPORT
    report_path = root / report_path if not report_path.is_absolute() else report_path
    write_report(
        report_path,
        title=args.title,
        mutants_all=all_mutants,
        results=results,
        exclusions=exclusions,
        sentinels=[m.mid for m in sentinels],
        git_status_sample=status_sample,
        projection=proj,
        timeout_policy=timeouts.describe(),
        extra_sections=[
            (
                "Campaign binding",
                "```json\n" + binding_section + "\n```",
            ),
            (
                "Baseline (unmutated L1+L2)",
                "```json\n" + baseline_section + "\n```",
            ),
            (
                "Judge-calls / blockers",
                "\n".join(
                    [
                        "- Product code is not modified by the harness "
                        "(mutants are restored after each body).",
                        "- Oracle is layered: L1 `cargo test -p mega-evm --quiet`, then L2 "
                        "`cargo test -p mega-state-test --quiet` if L1 survives.",
                        "- Spec-gate / adjacent-spec sites in `#[cfg(test)]` modules and "
                        "inside `assert!`/`debug_assert!` args are **excluded** "
                        "(non-product).",
                        "- Spec-gate sites that use `is_enabled(SomeAssociatedConst)` "
                        "(not a `MegaSpecId::` path) remain out of scope for v0.",
                        "- call_delete only removes statement-position calls; expression "
                        "position (assign/return/condition/`?`) is skipped.",
                        "- State resume binds HEAD + inventory_hash + oracle_command; "
                        "mismatch → refuse with --fresh hint.",
                        "- Each oracle layer runs under a wall-clock ceiling "
                        "(`--test-timeout`, else derived from the baseline). On "
                        "expiry the child's whole process group is killed "
                        "(SIGTERM → grace → SIGKILL), the mutant is restored "
                        "through the normal journal compare-and-restore, and it "
                        "is recorded as killed with kill_kind=timeout — counted "
                        "as a kill but reported apart from assertion kills.",
                        "- Crash journal + regular restore share compare-and-restore "
                        "(hash vs original/mutated); concurrent-edit refuses and keeps "
                        "journal. Full-tree `git status --porcelain` after baseline and "
                        "each mutant; only runtime artifacts (reports/logs/state/journal) "
                        "are whitelisted — harness sources dirty → abort, scene preserved.",
                        f"- Enumerated spec-gate points: {len(sg_mutants)//2} "
                        f"({len(sg_mutants)} bodies).",
                        f"- Enumerated gas constants: {len({m.site for m in gc_mutants})} "
                        f"({len(gc_mutants)} bodies).",
                        f"- Enumerated adjacent-spec bodies: {len(adj_mutants)}.",
                        f"- Enumerated call-delete bodies: {len(cd_mutants)} "
                        f"(skipped sites logged in exclusions).",
                        f"- Total bodies: {len(all_mutants)}.",
                    ]
                ),
            ),
        ],
    )
    print(f"report: {report_path}", flush=True)

    # Fail the process if a sentinel survived (harness bug signal).
    sentinel_fails = [
        r
        for r in results
        if r.is_sentinel and r.status != "killed"
    ]
    if sentinel_fails:
        print("ERROR: sentinel mutant(s) were not killed:", file=sys.stderr)
        for r in sentinel_fails:
            print(f"  {r.mid} status={r.status} err={r.error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
