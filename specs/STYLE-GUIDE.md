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
| **Implementation references** | `specs/impl/{Spec}-Implementation-References.md` | Informative. Maps spec clauses and invariants to source files. |

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

There are two spec categories: **patch specs** and **base specs**.
A patch spec describes behavioral deltas from an existing MegaETH spec (e.g., Rex3 patches Rex2).
A base spec defines complete EVM behavior from a parent layer (e.g., MiniRex builds on Optimism Isthmus).

### Patch specs

Patch specs (Rex, Rex1, Rex2, Rex3, Rex4, and future patches) MUST use the following structure:

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

**Abstract**: One paragraph. State what the spec is, summarize changes, state inheritance baseline.

**Changes**: Each change is `### N. {Title}` with:
- `#### Motivation` — Why this change is needed. 1–3 sentences.
- `#### Semantics` — Use the "Previous behavior / New behavior" pattern:
  ```
  Previous behavior:
  - ...

  New behavior:
  - ...
  ```

**Invariants**: Numbered `I-1`, `I-2`, etc. Each a single MUST/MUST NOT sentence.

**Inheritance**: One sentence + full lineage, e.g.:
`Semantic lineage: Rex3 -> Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague.`

**References**: Links to predecessor specs (and successor if they exist), companion docs, related docs, external standards.

### Base specs

Base specs (currently only MiniRex) MUST use the following structure:

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

**Abstract**: One paragraph. State what the spec is and what parent layer it builds on.

**Base Layer**: Declare the parent layer and state that all parent semantics are inherited unless overridden.

**Specifications**: Each feature is `### N. {Title}` with:
- `#### Rationale` — Why this feature exists. MAY be longer than a patch spec's Motivation.
- `#### Semantics` — Define behavior directly using MUST/MUST NOT. No Previous/New pattern.

**Invariants**: Same format as patch specs.

**References**: Links to successor specs, companion docs, related docs, external standards.

### Key differences

| Aspect | Base spec | Patch spec |
| --- | --- | --- |
| Top-level section | `## Specifications` | `## Changes` |
| Subsection header | `#### Rationale` | `#### Motivation` |
| Semantics style | Define behavior directly | Previous/New behavior diff |
| Lineage section | `## Base Layer` | `## Inheritance` |
| Rationale length | MAY be longer (design justification) | 1–3 sentences |

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
- Keep the Abstract and Motivation/Rationale sections brief (Rationale in base specs MAY be longer).

## Companion Doc Conventions

### Behavior-Details

Content that belongs here:
- Concrete addresses, selector values, error signatures.
- Edge case explanations.
- Background context and rationale that is too long for Motivation/Rationale.
- Solidity interfaces.
- Usage examples where they help developers understand the semantics.
- For base specs: migration impact and developer guidance (e.g., how existing contracts are affected).

Structure: use the same numbering as the normative spec (e.g., `## N. {Feature/Change title} details`).

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
- Mapping from each spec feature/change to source files.
- Maintenance notes.

Structure:
- `## Change Mapping` (patch specs) or `## Specification Mapping` (base specs), with subsections matching the normative spec's numbering.
  Each subsection MUST contain two parts:
  - **Spec clauses** — enumerate the key normative requirements.
  - **Implementation** — list the source files that implement those clauses.
- `## Invariant Mapping` listing each invariant with its implementation coverage.
- `## Maintenance Notes` with update instructions.
