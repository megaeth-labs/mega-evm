# Base Spec Style Guide

This document defines the format for **base specs** — specs that define the complete MegaETH EVM behavior from a parent layer (e.g., MiniRex builds on Optimism Isthmus).
Base specs introduce new feature areas rather than modifying existing MegaETH behavior.

Currently, MiniRex is the only base spec.

For **patch specs** (Rex, Rex1, Rex2, Rex3, Rex4, and future patches), see [STYLE-GUIDE.md](STYLE-GUIDE.md).

## How it differs from patch specs

| Aspect | Base spec | Patch spec |
| --- | --- | --- |
| Top-level section | `## Specifications` | `## Changes` |
| Subsection header | `#### Rationale` | `#### Motivation` |
| Semantics style | Define behavior directly | Previous/New behavior diff |
| Lineage section | `## Base Layer` | `## Inheritance` |
| Rationale length | MAY be longer (design justification) | 1–3 sentences |

A patch spec describes what changed and why.
A base spec describes what exists and why it was designed that way.

## Document Hierarchy

Same as patch specs: each base spec produces three files (normative spec, Behavior-Details, Implementation-References).
See [STYLE-GUIDE.md § Document Hierarchy](STYLE-GUIDE.md#document-hierarchy) for file locations, roles, and disclaimer templates.

## Normative Spec Structure

```markdown
# {Spec} Specification

## Abstract

## Base Layer

## Specifications

### N. {Feature title}
#### Rationale
#### Semantics

## Invariants

## References
```

### Abstract

- One paragraph.
- State what this spec is and what parent layer it builds on.
- Summarize the feature areas introduced.

### Base Layer

- Declare the parent layer, e.g.:
  `MiniRex builds on Optimism Isthmus (Ethereum Prague).`
- State that all parent layer semantics are inherited unless explicitly overridden below.

### Specifications

- Each feature area is a numbered subsection: `### 1. {Title}`.
- Each feature area contains two sub-subsections:
  - `#### Rationale` — Why this feature exists. MAY be longer than a patch spec's Motivation, since base specs often need to explain design decisions and trade-offs from first principles.
  - `#### Semantics` — What the EVM does. Define behavior directly using MUST/MUST NOT language. No "Previous behavior / New behavior" pattern.
- Tables with concrete values belong in Semantics.

### Invariants

- Same format as patch specs: numbered `I-1`, `I-2`, etc.
- Each invariant is a single sentence using MUST/MUST NOT.

### References

- Links to successor specs (first patch spec in the lineage).
- Links to companion impl docs.
- Links to related docs and external standards.

## Writing Conventions

All writing conventions from [STYLE-GUIDE.md](STYLE-GUIDE.md) apply (normative language, ABI-observable interfaces, tables, prose style).

## Companion Doc Conventions

Same structure as patch specs, with one addition for Behavior-Details:

- **Migration impact**: Base specs introduce entirely new behavior that affects existing contracts and applications. The Behavior-Details companion doc SHOULD include a migration impact section covering how existing contracts, gas estimation, and application workflows are affected.

For Implementation-References, use `## Specification Mapping` (instead of `## Change Mapping`) with subsections matching the normative spec's numbering.
