# Specification — Writing Rules

This layer is the **formal MegaETH specification** — the complete, normative definition of MegaETH's verifiable behavior.
It is not limited to the EVM; it covers the entire set of behaviors that define a correct MegaETH node: transaction processing, resource metering, system contracts, oracle services, and protocol state management.
It targets protocol implementers, auditors, and anyone who needs to verify or reproduce MegaETH's exact behavior.

**Scope principle**: If a behavior affects whether a node produces correct outputs given the same inputs, it belongs in this spec.
This includes but is not limited to EVM execution — gas accounting, resource limits, system contract semantics, oracle data lifecycle, and upgrade activation rules are all in scope.

## Page Structure

Every spec page MUST include YAML frontmatter with a `spec` field — the latest stable spec that this page's main content describes (e.g., `Rex3`).
This field is the single source of truth for which spec version the page reflects.
When the latest stable spec changes, update this field and the page content together.

Every spec page MUST follow this section order:

```
# Page Title
Abstract: 1-2 sentence summary of what this page specifies.

## Motivation
The problem statement: what problem exists that this spec solves.
Describes the concrete failure modes or limitations that necessitate this behavior.
This section explains WHY the spec exists, not how it works.

## Specification
The normative behavioral definition.
Subsections organized by logical component.
All behavioral rules use MUST/MUST NOT/SHALL/SHOULD/MAY per RFC 2119.

## Constants
Table of all named constants with values and descriptions.
Every constant referenced in the Specification section MUST appear here.
Placed after the Specification so readers encounter the behavioral rules first
and can reference constants as needed.

## Rationale
Design decisions: why this specific solution over alternatives.
Each decision is a named paragraph explaining the trade-off.
This section explains WHY specific choices were made.

## Security Considerations
What could go wrong if this spec is implemented incorrectly or incompletely.
Concrete attack vectors, economic risks, or safety invariants that implementers must preserve.

## Spec History
Links to upgrade pages showing how this behavior evolved across specs.
```

**Sections may be omitted** when they genuinely don't apply (e.g., a glossary page has no Constants or Motivation).
But for any page that defines behavioral rules, the full structure SHOULD be followed.
Security Considerations SHOULD be included on any page that defines gas accounting, resource limits, state mutation, or economic rules.
It MAY be omitted for pages that are purely structural (e.g., glossary, overview indexes).

### Spec History Format

For **overview pages**, the Spec History section SHOULD be a simple list of specs linking to their upgrade pages.
Each item SHOULD include one short sentence summarizing the main change introduced by that spec.
Do not use tables or detailed changelog breakdowns there.

Example:

```markdown
## Spec History

- [MiniRex](../upgrades/minirex.md) — Introduced the initial MegaEVM execution model.
- [Rex](../upgrades/rex.md) — Revised gas forwarding and storage-gas semantics.
- [Rex1](../upgrades/rex1.md) — Fixed detained compute-gas reset behavior.
- [Rex2](../upgrades/rex2.md) — Added KeylessDeploy and enabled SELFDESTRUCT with EIP-6780 semantics.
- [Rex3](../upgrades/rex3.md) — Revised oracle detention to use SLOAD-based triggering.
- [Rex4](../upgrades/rex4.md) *(unstable)* — Introduces per-call-frame limits and relative detention.
```

For **concept pages**, the Spec History section MAY summarize what changed at each spec if that helps explain the evolution of the behavior.

Rules for the Spec History table:
- Only list specs that changed behavior relevant to this page. Do not list specs that inherited behavior unchanged (unless noting "No changes" is clarifying).
- The "Change" column is a short summary (one sentence). Detailed previous/new behavior belongs in the linked upgrade page.
- The "Key values" column shows the concrete values introduced at that spec — constants, limits, formulas. Use "N/A" if the change is purely behavioral with no new numeric values.
- Link each spec name to its upgrade page under `upgrades/`.

**Overview-page exception**: overview/index pages that summarize and organize links to authoritative subpages SHOULD omit Motivation and Rationale.
For those pages, use a lighter structure such as:

```
# Page Title
Abstract: 1-2 sentence summary.

## Stable Scope
What stable behavior the overview summarizes.

## Specifications
Grouped summaries of the relevant concept pages.

## Spec History
Optional, if it helps orient readers.
```

## Accuracy

- **Verify constants against source code.**
  Every numeric value (gas limits, storage gas bases, detention caps, contract addresses) MUST be verified against the implementation source (`mega-evm`, `mega-reth`, etc.) before writing or updating.
  Do not copy values from other documentation pages without cross-checking.
- **Attribute behavior to the correct spec.**
  If a behavior was introduced in Rex3, reference Rex3 — not Rex4, even if Rex4 inherits it.
  Each spec's contribution must be precisely scoped.
- **Do not invent guarantees.**
  The specification MUST NOT introduce constraints or behavioral rules that are not verified against the implementation.
  If uncertain, surface the ambiguity to the user rather than guessing.

## Tone & Language

- **Normative and precise.** Use MUST, MUST NOT, SHALL, SHOULD, MAY per RFC 2119 when defining behavior. Every behavioral rule in the Specification section MUST use normative language.
- **Exhaustive.** Cover every corner case. Readability may be sacrificed for completeness.
- **No developer guidance.** Do not include "tips", "best practices", "how to use", or recommendations like "Use `eth_estimateGas`" or "Use transient storage". That belongs in `docs/dev/`.
- **No user-facing language.** Do not address the reader as "you" in the Specification section. Do not explain "what this means for you". That belongs in `docs/user/` or `docs/dev/`.
- **Self-contained.** The spec never links to user docs or developer docs. It may link to external references (EIPs, Ethereum Yellow Paper, OP Stack specs) and to other spec pages.
- **No implementation details.** Do not reference specific code patterns, function names, or implementation strategies (e.g., `spec.is_enabled(MINI_REX)`). Describe the required behavior, not how to implement it.

## Specification Section Rules

### Normative Language

- **Uppercase only.**
  Per RFC 8174, only UPPERCASE keywords (MUST, SHOULD, MAY, etc.) carry normative weight.
  Lowercase "must", "should", "may" are plain English with no special meaning.
  The formal BCP 14 boilerplate lives on the [Specification overview page](overview.md) — individual spec pages do not repeat it.
- Use "A node MUST..." for required behavior.
- Use "A node MUST NOT..." for prohibited behavior.
- Use "SHOULD" only when non-compliance is acceptable in defined circumstances.
- Do not use normative keywords outside the Specification and Security Considerations sections.
  Motivation, Rationale, and Spec History use plain English.
- Descriptive prose (background, context) does not require normative keywords.

### Constants

- Every numeric value used in the Specification MUST be defined as a named constant in the Constants table.
- Do not embed magic numbers in formulas — reference the constant name.
- Each constant row MUST include: name, value, and a one-line description.

### Formulas

- Express formulas as inline code blocks: `` `total_gas = compute_gas + storage_gas` ``.
- Define every variable immediately after the formula.
- For complex logic, use pseudocode in fenced code blocks.

### Edge Cases

- State edge cases explicitly as normative rules (e.g., "For state that does not yet exist, the node MUST...").
- Do not leave behavior undefined — if the spec doesn't say what happens, implementers will guess differently.

### Charging Lifecycle

- For any cost or fee, specify WHEN it is charged: before execution, at the opcode, or post-execution.
- Specify what happens on failure: is the cost consumed, refunded, or rolled back?

### Spec Versioning

- Main content in concept pages MUST describe the latest stable spec's behavior only.
  Previous spec behavior belongs in the Spec History table or upgrade pages.
  Unstable spec behavior MUST be placed in `<details>` blocks, never in main prose or tables.
- **The latest spec is implicit.**
  Do not mention the latest spec name in the Specification, Constants, or Motivation sections.
  The `spec` frontmatter field declares which spec the page describes.
  Readers should be able to read the main content without encountering "as of Rex3" or "in Rex3" qualifiers.
  The Rationale section MAY reference spec names when explaining historical design decisions.
- Wrap unstable (not-yet-activated) spec content in `<details>` blocks with a clear label (e.g., "Rex4 (unstable): ...").
- Unstable content MUST still use normative language within the `<details>` block.

## Motivation and Rationale Section Rules

### Motivation

- Describe the concrete problem: what breaks, what is underpriced, what attack becomes possible.
- Use specific numbers where possible (e.g., "base fee of 0.001 gwei", "up to 10 billion gas per block").
- Do NOT describe the solution — that is the Specification section's job.

### Rationale

- Each design decision is a **named paragraph** starting with bold text (e.g., "**Why `base × (multiplier − 1)` instead of `base × multiplier`?**").
- Explain the trade-off: what was considered, what was rejected, and why.
- Reference historical changes where applicable (e.g., "MiniRex used X, Rex changed to Y because...").

## Security Considerations Section Rules

This section exists because EIP-1 makes Security Considerations a blocking requirement for any Ethereum specification, and RFC 2119 §7 warns that "the effects on security of not implementing a MUST or SHOULD may be very subtle."

- Describe **concrete** risks, not generic warnings.
  State what goes wrong: underpriced operations, denial-of-service vectors, state corruption, economic exploits.
- Frame each risk as a consequence of incorrect implementation.
  "If a node fails to charge storage gas on contract creation, an attacker can exhaust state storage at compute-gas cost only."
- Do not repeat the Specification section.
  The Specification says what MUST happen; Security Considerations says what breaks if it does not.
- Name the invariants that the spec preserves.
  "This spec preserves the invariant that total detained gas never exceeds the block gas limit."
- If no security considerations apply, state so explicitly: "This page has no security considerations."
  Do not silently omit the section — that is ambiguous (forgot vs. none).

## Upgrade Page Rules

Upgrade pages under `upgrades/` are the authoritative record of what changed at each spec.
They complement concept pages: concept pages describe the current behavior, upgrade pages describe the delta.

### Required Structure

Every upgrade page MUST follow this structure:

```markdown
---
spec: <SpecName>
---

# <SpecName> Network Upgrade

This page is an informative summary of the <SpecName> specification.

## Summary
2-4 paragraphs: what changed, what problem it solves, developer impact.

## What Changed

### <Change Name>

#### Previous behavior
- Precise description of behavior before this spec.
- Include concrete values, formulas, or rules that were in effect.

#### New behavior
- Precise description of behavior introduced by this spec.
- Include the new values, formulas, or rules.

### <Next Change Name>
...

## Developer Impact
What contract authors, integrators, and tooling authors need to know.

## Safety and Compatibility
Backward-compatibility boundaries, failure-mode differences.

## References
Links to the implementation repo, related EIPs, or other specs.
```

### Precision Requirements

- Every "Previous behavior" section MUST state the exact prior values, formulas, or rules — not just "it was different."
  A reader should be able to implement the previous spec from the "Previous behavior" text alone.
- Every "New behavior" section MUST state the exact new values, formulas, or rules.
- Do not merge changes so aggressively that the previous/new mapping becomes unclear.
  Each behavioral change gets its own subsection under "What Changed."
- If a change fixes a bug in a prior spec, state what the buggy behavior was and what the corrected behavior is.

## What Belongs Here

- EVM behavioral definitions (gas costs, opcode semantics, resource limits)
- System contract specifications (addresses, interfaces, execution semantics)
- Oracle service specifications (storage layout, timing guarantees, gas detention)
- Hardfork/spec progression and per-upgrade behavioral deltas
- Glossary of protocol terms

## What Does NOT Belong Here

- Developer tips or best practices → `docs/dev/`
- Code examples showing how to use a feature → `docs/dev/`
- User-facing explanations → `docs/user/`
- Integration configuration → `docs/dev/tooling.md`
- "Why should I care about this?" framing → `docs/dev/`
- Implementation-specific code patterns or API references → `docs/dev/`
- Recommendations ("Use X instead of Y") → `docs/dev/`

## Source of Truth

The user is the ultimate source of truth.
This documentation is the canonical written specification of MegaETH's verifiable behavior, but user instructions override both the docs and the implementation when there is ambiguity or conflict.

The implementation repositories are side channels for verification only.
Agents MUST inspect the relevant implementation in `mega-evm`, `mega-reth`, and any other repository that materially affects the documented behavior to confirm whether the specification matches the current implementation, but implementation code is not the final authority.

If an agent finds or suspects any discrepancy between user intent, this specification, and the implementation behavior across `mega-evm`, `mega-reth`, or other relevant repositories, the agent MUST NOT silently resolve it.
Instead, the agent MUST surface the discrepancy clearly and ask the user to confirm the intended behavior before changing the spec text.

Do not describe this spec as "mirrored from mega-evm/docs".
The old `mega-evm/docs` content is transitional and may be removed.

## Linking Rules

- **Link glossary terms on first use.**
  The first mention of a MegaETH-specific term on a page SHOULD link to its glossary entry.
  Do not over-link — subsequent uses on the same page need not be linked.
- **Link spec names to their upgrade pages.**
  When a spec is mentioned by name (e.g., MiniRex, Rex, Rex3), link it to the corresponding upgrade page under `upgrades/`.
- **Anchor targets must be headings.**
  Any content that is the target of a `#fragment` link MUST be a markdown heading (`##`, `###`, etc.), not bold text or other inline formatting.
  GitBook generates anchors only from headings.

## Formatting Preferences

- Use tables for structured data (gas costs, opcode lists, resource limits, constants).
- Use unambiguous table values. Write "Unlimited", "No limit", or "N/A" — never use bare dashes ("—") which are ambiguous (not applicable? unknown? unlimited?).
- Use `<details>` blocks for unstable (Rex4) features.
- Use `{% hint style="info" %}` sparingly — only for non-normative notes that help implementers understand design intent. Never for developer tips.
- Use `{% hint style="warning" %}` for unstable spec warnings.
- Do NOT use `{% hint style="success" %}` in spec pages — it implies developer guidance.
- Do NOT use `{% hint style="danger" %}` for normative rules — normative rules belong in plain prose with MUST/MUST NOT. Reserve `{% hint style="danger" %}` only for deprecation notices.
