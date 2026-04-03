---
name: doc-readability
description: Evaluates documentation readability, structure, and compliance with docs/spec/AGENTS.md conventions. Use when reviewing doc quality, auditing formatting compliance, checking a page before publishing, or running a readability audit across the specification.
---

# Documentation Readability Evaluation

Evaluate readability and spec-layer compliance for: $ARGUMENTS

Parse the arguments to determine scope.
Accepted inputs:

- A single page path (e.g., `docs/spec/evm/dual-gas-model.md`) — evaluate that page.
- A directory (e.g., `docs/spec/evm/`) — evaluate all pages in that directory.
- `all` — evaluate every page listed in `docs/spec/SUMMARY.md`.

Default (no arguments): evaluate all pages.

## Setup

Before evaluating any page:

1. Read `docs/spec/AGENTS.md` — the authoritative writing rules for this specification.
2. Read `docs/spec/SUMMARY.md` — the full page inventory.

## Evaluation Checklist

### 1. Structural Lint

- [ ] **Frontmatter**: YAML frontmatter present with a `spec` field (concept pages) or `description` field (upgrade pages).
- [ ] **Heading hierarchy**: Exactly one H1 (`#`). Sections use H2 (`##`), subsections H3 (`###`). No heading level skips.
- [ ] **One sentence per line**: Each sentence on its own line for diff readability. Flag paragraphs where multiple sentences share a line.
- [ ] **Page in SUMMARY.md**: The page appears in `docs/spec/SUMMARY.md`. Flag orphaned pages.

### 2. Page Structure Compliance

Concept pages MUST follow this section order (per `docs/spec/AGENTS.md`):

```
# Page Title
Abstract
## Motivation
## Specification
## Constants
## Rationale
## Security Considerations
## Spec History
```

- [ ] **Required sections present**: For pages defining behavioral rules, check that Specification and Constants sections exist.
- [ ] **Section order**: Sections appear in the prescribed order.
- [ ] **Upgrade page structure**: Upgrade pages under `upgrades/` follow the required structure (Summary, What Changed with Previous/New behavior, Developer Impact, Safety and Compatibility, References).

### 3. Normative Language

- [ ] **MUST/SHALL/SHOULD/MAY usage**: Behavioral rules in Specification sections use uppercase normative keywords per RFC 2119.
- [ ] **No normative keywords outside Specification/Security**: Motivation, Rationale, and Spec History use plain English only.
- [ ] **No implementation details**: Specification sections do not reference code patterns, function names, or implementation strategies.
- [ ] **No developer guidance**: No "tips", "best practices", "how to use", or "you should" language.
- [ ] **No user-facing language**: No second-person ("you") in Specification sections.

### 4. Constants and Formulas

- [ ] **Named constants**: Every numeric value in the Specification is defined as a named constant in the Constants table.
- [ ] **No magic numbers**: Formulas reference constant names, not embedded numeric values.
- [ ] **Constant rows complete**: Each constant row has name, value, and one-line description.

### 5. Spec Versioning

- [ ] **Latest spec implicit**: Main content does not mention the latest spec name in Specification, Constants, or Motivation sections.
- [ ] **Unstable content in `<details>`**: Unstable (not-yet-activated) spec content is wrapped in `<details>` blocks.
- [ ] **Spec History format**: Overview pages use simple lists; concept pages may use tables. Only list specs that changed relevant behavior.

### 6. Cross-Linking

- [ ] **Glossary first-use linking**: First mention of MegaETH-specific terms links to glossary entry. No over-linking.
- [ ] **Spec names link to upgrade pages**: When a spec is mentioned by name, it links to `upgrades/{spec}.md`.
- [ ] **Anchor targets are headings**: Any `#fragment` link target is a markdown heading, not bold text.
- [ ] **No external user/dev links**: Spec pages do not link to external user docs or developer docs.

### 7. Formatting

- [ ] **Tables for structured data**: Gas costs, opcode lists, resource limits, constants use tables.
- [ ] **Unambiguous table values**: "Unlimited", "No limit", or "N/A" instead of bare dashes.
- [ ] **Hint blocks**: `{% hint style="info" %}` used sparingly for non-normative notes. No `{% hint style="success" %}`. `{% hint style="danger" %}` only for deprecation notices.

## Output Format

```markdown
## Readability

**Scope**: {what was evaluated}
**Pages evaluated**: {count}

### Summary

| Result | Count |
|--------|-------|
| Pass | N |
| Fail | N |
| Total findings | N |

### Per-Category Results

For each page, report pass/fail per checklist category.
Categories with no findings still appear as "Pass" — do not silently omit them.

| Category | Result | Findings |
|----------|--------|----------|
| Structural Lint | Pass / Fail | N |
| Page Structure Compliance | Pass / Fail | N |
| Normative Language | Pass / Fail | N |
| Constants and Formulas | Pass / Fail | N |
| Spec Versioning | Pass / Fail | N |
| Cross-Linking | Pass / Fail | N |
| Formatting | Pass / Fail | N |

### Findings

#### R-001: {short description}

- **Severity**: Blocker | Major | Minor
- **Page**: {path}
- **Rule**: {which check failed}
- **Evidence**: {excerpt or description}
- **Suggested fix**: {concrete edit}

#### R-002: ...
```

**Severity definitions**:

- **Blocker**: Content violates core spec conventions — implementation details in Specification section, normative keywords in Rationale, spec linking to external user/dev docs.
- **Major**: Missing frontmatter, heading hierarchy violations, wrong hint block style, missing required sections.
- **Minor**: Multi-sentence lines, missing glossary links, suboptimal block usage, minor formatting drift.

## Rules

- Evaluate against the `docs/spec/AGENTS.md` rules as written. Do not invent additional style rules.
- When a page has zero findings, still count it as "Pass" in the summary.
- Sort findings by severity (Blocker first), then by page.
- Do NOT fix the issues yourself unless the user explicitly asks. This skill produces a report, not edits.
- If a finding is ambiguous (could be intentional), note it as Minor with "Review: may be intentional".
