# External Site Doc Format

Use this format when generating public site documentation from repository specs.
Treat the repository spec as the normative source of truth.
Treat the public site page as informative and developer-facing.

## Source hierarchy

Use inputs in this order:

1. `specs/*.md`
2. `specs/impl/*-Behavior-Details.md`
3. `specs/impl/*-Implementation-References.md`

Use implementation references only for validation.
Do not surface code-path mappings or implementation-reference links in the public page unless the user explicitly asks.

## Page role

- The site page is informative, not normative.
- The page MUST preserve repository spec semantics.
- If the page conflicts with the repository spec, the repository spec wins.
- The page SHOULD help developers understand behavior, impact, and compatibility.

## Required page structure

1. Frontmatter (YAML)
2. `Title`
3. `Summary`  — includes upgrade motivation
4. `What Changed`
5. `Developer Impact`
6. `Safety and Compatibility`
7. `References`

## Section rules

### 0. Frontmatter

Every page starts with YAML frontmatter for GitBook SEO and indexing:

```yaml
---
description: One-sentence summary of what this spec upgrade changes, for search engines and link previews.
---
```

### 1. Title

Use the format `<SpecName> Network Upgrade` (e.g., `Rex4 Network Upgrade`), following OP Stack convention.

Immediately after the title, add a self-contained informative notice:

```md
This page is an informative summary of the <SpecName> specification.
For the full normative definition, see the <SpecName> spec in the mega-evm repository.
```

Do not link users to external sources for essential reading — the page should be self-contained.

### 2. Summary

Write two to four short paragraphs.
Explain what changed, what problem it solves, and the most important impact for developers.
Include the high-level motivation for the upgrade here — why this change was needed, in user-facing terms.
Do not include implementation details.

### 3. What Changed

Create one subsection per spec change.
For each change, use exactly this shape:

```md
## What Changed

### <Change Name>

#### Previous behavior
- ...

#### New behavior
- ...
```

Keep every `Previous behavior` and `New behavior` distinction from the repository spec.
Do not merge changes so aggressively that the mapping back to the normative spec becomes unclear.

### 4. Developer Impact

Explain what contract authors, integrators, and tooling authors need to care about.
Focus on observable behavior and design implications.
Address the developer directly — use "you" and "your".

### 5. Safety and Compatibility

State backward-compatibility boundaries clearly.
State failure-mode differences such as revert versus halt when relevant.
State whether pre-upgrade behavior remains unchanged for older specs.

### 6. References

Link only high-value supporting documents.
Do not use relative paths to files inside the source repository — the public page lives in a separate docs repo.
Link to the repository itself (e.g., `https://github.com/megaeth-labs/mega-evm`) and mention the spec path.
Do not dump implementation-path indexes into the public page.

## Allowed and prohibited content

### Allowed in the main public page

- Contract addresses that are part of the developer-facing interface.
- Solidity interface definitions (with NatSpec) for new system contracts introduced in the spec.
- Error signatures and revert reasons that developers may encounter.
- Formulas and constants that define observable behavior (e.g., budget forwarding ratios, gas caps).

### Prohibited in the main public page

- Code paths, function names, or struct names from the implementation.
- Test file mappings.
- Selector-level details unless explicitly requested.
- Internal dispatch or interception flow details unless necessary for understanding.
- New guarantees not present in the repository spec.

## Writing style

- Write for developers, not auditors.
- Use active voice.
  Address the developer as "you" and "your".
- Explain behavior before mechanism.
- Keep paragraphs short.
- Keep each bullet to one concrete point.
- Preserve semantic accuracy over smoothness.
- Prefer developer-facing language over implementation-facing language.
- Preserve MUST/MUST NOT semantics from the spec, but prefer natural language where meaning is unchanged.
  Example: "MUST revert the frame" → "reverts the frame (does not halt the transaction)".
  Use explicit MUST only when the obligation is non-obvious or safety-critical.
- Prefer "allows", "enables", "can" over passive constructions.
  Write documentation that reads like a guide, not a translated spec.
- One sentence, one line.

## Generation checklist

- Confirm the repository spec remains the semantic source of truth.
- Confirm all required sections are present and heading levels follow the hierarchy above.
- Keep all spec changes represented in the public page.
- Keep all previous-versus-new comparisons.
- Keep compatibility boundaries.
- Keep invariants either explicitly or as clearly equivalent compatibility/safety statements.
- Use behavior-details docs only to clarify, not to invent.
- Exclude implementation-reference links from the main narrative.
