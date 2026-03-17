# Spec Document Style Guide

This document defines the standard format and writing conventions for MegaETH EVM spec documents under `specs/`.
All new specs MUST follow this guide.
Existing specs SHOULD be migrated to this format.

## Document Hierarchy

Each spec produces three files:

| File | Location | Role |
| --- | --- | --- |
| **Normative spec** | `specs/{Spec}.md` | Defines what the EVM MUST do. This is the source of truth. |
| **Behavior details** | `specs/impl/{Spec}-Behavior-Details.md` | Informative. Elaborates on semantics with concrete values, examples, edge cases, and rationale. |
| **Implementation references** | `specs/impl/{Spec}-Implementation-References.md` | Informative. Maps spec clauses and invariants to source files and tests. |

Each companion doc MUST open with a disclaimer stating that it is informative, that normative semantics are defined in the spec, and that the normative spec wins on conflict.
The exact wording MAY vary, but MUST convey these three points.

Behavior-Details template:

> This document is informative.
> Normative semantics are defined in [{Spec} Specification](../{Spec}.md).
> If this document conflicts with the normative spec text, the normative spec wins.

Implementation-References template:

> This document is informative.
> Normative semantics are defined in [{Spec} Specification](../{Spec}.md).
> If this mapping conflicts with the normative spec text, the normative spec wins.

## Normative Spec Structure

This guide covers **patch specs** (Rex, Rex1, Rex2, Rex3, Rex4, and future patches) — specs that modify an existing MegaETH spec by describing behavioral deltas.

For **base specs** (currently only MiniRex), see [BASE-SPEC-GUIDE.md](BASE-SPEC-GUIDE.md).

The normative spec file MUST use the following top-level sections in order:

```markdown
# {Spec} Specification

## Abstract

## Changes

### N. {Change title}
#### Motivation
#### Semantics

## Invariants

## Inheritance

## References
```

### Abstract

- One paragraph.
- State what this spec is (e.g., "Rex3 is the third patch to the Rex hardfork.").
- Summarize the changes introduced (enumerate briefly).
- State the inheritance baseline (e.g., "All Rex2 semantics are preserved unless explicitly changed below.").

### Changes

- Each change is a numbered subsection: `### 1. {Title}`.
- Each change contains two sub-subsections:
  - `#### Motivation` — Why this change is needed. 1–3 sentences. Focus on the problem being solved, not the implementation.
  - `#### Semantics` — What the EVM does differently. Use the "Previous behavior / New behavior" pattern:
    ```
    Previous behavior:
    - ...

    New behavior:
    - ...
    ```
- Tables with concrete values (gas costs, limits, formulas) belong in Semantics.

### Invariants

- Numbered list: `I-1`, `I-2`, etc.
- Each invariant is a single sentence stating a correctness property using MUST/MUST NOT.
- Invariants capture cross-cutting properties that span multiple changes or that are critical to preserve.

### Inheritance

- One sentence stating the inheritance chain, e.g.:
  `Rex3 inherits Rex2 except for the deltas defined in Changes.`
- Followed by the full lineage:
  `Semantic lineage: Rex3 -> Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague.`

### References

- Links to predecessor specs (and successor specs, if they exist).
- Links to companion impl docs.
- Links to related docs (e.g., `docs/RESOURCE_ACCOUNTING.md`).
- Links to external standards (e.g., EIP links).

## Writing Conventions

### Normative language

Use RFC-2119 keywords for behavioral requirements:

- **MUST** / **MUST NOT** — absolute requirements.
- **SHOULD** / **SHOULD NOT** — strong recommendations with rare exceptions.
- **MAY** — optional behavior.

Example: "A frame-local exceed MUST revert that frame and MUST NOT halt the transaction."

### ABI-observable interfaces vs implementation details

The normative spec MUST NOT contain internal implementation details:
- Source file paths (e.g., `crates/mega-evm/src/...`).
- Internal Rust function, method, struct, or module names (e.g., `AdditionalLimit::reset`).

These belong in the Implementation References companion doc.

However, ABI-observable interface elements ARE part of the external semantics and SHOULD appear in the normative spec where they define required behavior:
- System contract function signatures (e.g., `remainingComputeGas() -> uint64`).
- Revert error signatures (e.g., `MegaLimitExceeded(uint8 kind, uint64 limit)`).
- Event signatures.
- Selector-level dispatch behavior (e.g., "unknown selectors MUST fall through").

Full Solidity interface blocks and detailed ABI encoding examples belong in Behavior-Details.

### Tables

Use tables for concrete numeric values, formulas, and comparisons.
Tables SHOULD include the predecessor spec's values for comparison where relevant.

### Prose style

- One sentence, one line (for diff readability).
- Prefer active voice.
- Avoid narrative or tutorial-style exposition in the normative spec — move that to Behavior-Details.
- Keep the Abstract and Motivation sections brief.

## Companion Doc Conventions

### Behavior-Details

Content that belongs here:
- Concrete addresses, selector values, error signatures.
- Edge case explanations.
- Background context and rationale that is too long for Motivation.
- Solidity interfaces.
- Usage examples where they help developers understand the semantics.

Structure: use the same change numbering as the normative spec (e.g., `## N. {Change title} details`).

#### Usage examples

Examples are optional.
Only include an example when the normative semantics are non-obvious and a concrete scenario genuinely helps developers understand the behavior.
Not every change needs examples — if the spec text is clear on its own, omit them.

These documents will be used to generate an external-facing spec site.
Examples should be written for developers integrating with MegaETH, not as test case summaries.

When an example is included, it SHOULD:
- Describe a realistic developer scenario in plain language.
- Use concrete values only where they clarify the rule (e.g., "a 20M cap" rather than raw constants).
- State the expected outcome and which normative rule it illustrates.
- Where relevant, briefly contrast the new behavior with the previous behavior.

**Critical rule**: All examples MUST be consistent with actual code behavior and test coverage.
No speculated, hypothetical, or imagined scenarios are allowed.
If a scenario cannot be verified against existing code or tests, it MUST NOT be included.

### Implementation-References

Content that belongs here:
- Mapping from each spec change to source files.
- Maintenance notes.

Structure follows the Rex4-Implementation-References.md exemplar:
- `## Change Mapping` with subsections matching the normative spec's change numbers.
  Each subsection MUST contain two parts:
  - **Spec clauses** — enumerate the key normative requirements from the spec change.
  - **Implementation** — list the source files that implement those clauses.
- `## Invariant Mapping` listing each invariant with its implementation coverage.
- `## Maintenance Notes` with update instructions.
