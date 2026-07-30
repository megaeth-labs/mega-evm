#!/usr/bin/env python3
"""Generate the comby rule file for the `spec-gate` operator pack.

The rules are derived from the MegaSpecId progression so that they stay in sync
with the spec system: edit `ALL_SPECS` / `FROZEN_SPECS` / `INSTRUCTION_MODULES`
here rather than hand-editing `rules.comby`.

Two operators are emitted (both pure literal comby templates, no holes, so they
match structurally with no receiver over-match risk):

  * SPEC_BOUNDARY_SHIFT  — shift an `is_enabled(MegaSpecId::X)` gate to an
    adjacent spec. Probes whether the *exact* activation fork is pinned by a
    test (the classic "gated one fork too early/late" backward-compat bug).
  * SPEC_MATCH_MISROUTE  — reroute a per-spec `rexN::instruction_table` dispatch
    arm to a neighbour's table. Probes whether opcode-level behavior is pinned
    per spec.

A third operator, SPEC_GATE_NEGATE (per-site polarity flip), is opt-in via
`--with-negate`. It uses a comby hole for the receiver, which can over-match a
compound receiver, so it is left off by default until validated on real matches.

Backward-compatibility scoping: `ALL_SPECS` lists the whole progression and fixes
adjacency; `FROZEN_SPECS` is the subset whose gates may be mutated. When a new
spec is introduced, append it to `ALL_SPECS`, exclude it from `FROZEN_SPECS`
while it is unstable, and regenerate — its own behavior is allowed to change, so
mutating its gates would only produce expected (equivalent) survivors, but it
remains a valid destination for a shift off the spec below it.
"""
from __future__ import annotations

import argparse
from pathlib import Path

# The full MegaSpecId progression, oldest -> newest, frozen and unstable alike
# (see crates/mega-evm/src/evm/spec.rs). Keep in spec order. This is the
# *adjacency* universe: it decides which spec sits next to which.
ALL_SPECS = [
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
]

# The specs whose gates may be mutated. Currently everything through REX6 (see
# CLAUDE.md for the current unstable spec).
#
# This is a *subset* of ALL_SPECS, not a replacement for it, and the distinction
# matters in both directions:
#
#   * A gate on a frozen spec is the mutation SOURCE — its activation fork is
#     fixed, so shifting it is a backward-compatibility bug a test must catch.
#   * The spec shifted TO is only a destination. It may be the unstable spec:
#     `is_enabled(REX6) ==> is_enabled(REX7)` is the classic "gated one fork too
#     late" regression — it silently disables frozen REX6 behavior on a REX6
#     chain — and is exactly what freezing REX6 promises to catch.
#
# Conflating the two lists drops every mutant whose destination is the unstable
# spec, leaving the newest frozen spec's gates probed from one side only.
FROZEN_SPECS = [s for s in ALL_SPECS if s != "REX7"]

# Per-spec instruction-table modules wired in evm/instructions.rs, in spec order.
#
# This is NOT simply the frozen progression. A module belongs here only if its
# table actually differs from its neighbour's, because SPEC_MATCH_MISROUTE works
# by swapping one module's table for an adjacent one: swapping in an identical
# table produces an equivalent mutant, i.e. a guaranteed survivor that says
# nothing about test coverage.
#
# `rex6` is excluded on exactly that ground: `rex6::instruction_table` returns
# `rex5::instruction_table` verbatim. Rex6 changed no table entry — every Rex6
# behavior difference is internal `is_enabled(MegaSpecId::REX6)` dispatch inside
# the shared handlers, and SPEC_BOUNDARY_SHIFT (driven by the spec lists above)
# is what probes those gates. Add `rex6` here only if Rex6 ever gains a table
# entry of its own.
INSTRUCTION_MODULES = ["mini_rex", "rex", "rex2", "rex3", "rex4", "rex5"]


def boundary_shift_rules() -> list[str]:
    """`is_enabled(MegaSpecId::X)` -> each adjacent spec, for every frozen X.

    The source is restricted to frozen specs; the destination is any neighbour in
    the full progression, including the unstable spec.
    """
    out = []
    for i, spec in enumerate(ALL_SPECS):
        if spec not in FROZEN_SPECS:
            continue
        for j in (i - 1, i + 1):
            if 0 <= j < len(ALL_SPECS):
                nbr = ALL_SPECS[j]
                out.append(
                    f"is_enabled(MegaSpecId::{spec}) ==> is_enabled(MegaSpecId::{nbr})"
                )
    return out


def match_misroute_rules() -> list[str]:
    """`rexN::instruction_table` -> each adjacent module's table.

    The `:[pre~[^_A-Za-z0-9]]` hole pins a non-identifier char immediately before
    the module name (and reproduces it) so a short module name does not match
    inside a longer one — e.g. `rex` must not match the tail of `mini_rex`.
    """
    out = []
    for i, mod in enumerate(INSTRUCTION_MODULES):
        for j in (i - 1, i + 1):
            if 0 <= j < len(INSTRUCTION_MODULES):
                nbr = INSTRUCTION_MODULES[j]
                out.append(
                    f":[pre~[^_A-Za-z0-9]]{mod}::instruction_table "
                    f"==> :[pre]{nbr}::instruction_table"
                )
    return out


def negate_rules() -> list[str]:
    """Per-site polarity flip. Opt-in: the `:[recv]` hole can over-match."""
    out = []
    for spec in FROZEN_SPECS:
        out.append(
            f":[recv].is_enabled(MegaSpecId::{spec}) "
            f"==> !(:[recv].is_enabled(MegaSpecId::{spec}))"
        )
    return out


def render(with_negate: bool) -> str:
    lines = [
        "# GENERATED by mutants/operators/spec-gate/generate.py — do not edit by hand.",
        "# Regenerate after changing the spec progression or instruction modules.",
        "# comby rule format: PATTERN ==> REPLACEMENT  ('#' lines are comments).",
        "",
        "# --- SPEC_BOUNDARY_SHIFT: shift a gate to an adjacent frozen spec ---",
        *boundary_shift_rules(),
        "",
        "# --- SPEC_MATCH_MISROUTE: reroute a per-spec instruction-table arm ---",
        *match_misroute_rules(),
    ]
    if with_negate:
        lines += [
            "",
            "# --- SPEC_GATE_NEGATE (opt-in): per-site polarity flip ---",
            *negate_rules(),
        ]
    return "\n".join(lines) + "\n"


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--with-negate",
        action="store_true",
        help="also emit the opt-in SPEC_GATE_NEGATE rules (validate matches first)",
    )
    p.add_argument(
        "--output",
        default=str(Path(__file__).parent / "spec-gate.rules"),
        # universalmutator requires ".rules" in the filename, else the path is
        # interpreted as a language name rather than a rule file.
        help="where to write the rules (default: this pack's spec-gate.rules)",
    )
    args = p.parse_args()
    Path(args.output).write_text(render(args.with_negate))
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
