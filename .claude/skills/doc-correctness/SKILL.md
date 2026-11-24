---
name: doc-correctness
description: Verifies documentation claims in docs/ against implementation source code in this repository. Use when cross-checking doc accuracy, auditing constants and addresses, verifying spec correctness, or validating behavioral rules against source.
---

# Documentation Correctness Verification

Verify factual claims in the documentation against implementation source: $ARGUMENTS

Parse the arguments to determine scope.
Accepted inputs:

- A single page path (e.g., `docs/spec/evm/dual-gas-model.md`) — verify claims on that page.
- A directory (e.g., `docs/spec/evm/`) — verify all pages in that directory.
- A claim family (e.g., `gas`, `system-contracts`, `upgrades`, `agent-files`) — verify all claims of that type across all pages.
- `all` — verify every page listed in `docs/spec/SUMMARY.md` and `docs/mega-evme/SUMMARY.md`, plus all agent files (`AGENTS.md`, `CLAUDE.md`, `docs/spec/AGENTS.md`, `bin/mega-evme/AGENTS.md`, `REVIEW.md`, `.claude/skills/*/SKILL.md`).

Default (no arguments): verify all pages.

## Claim Families

Every verifiable claim in the docs falls into one of these families.

| Family | What to verify | Key source locations |
|--------|---------------|---------------------|
| Gas | Gas costs, limits, compute gas caps, storage gas bases, detention caps, refund rules, multipliers | `crates/mega-evm/src/constants.rs`, `crates/mega-evm/src/evm/instructions.rs`, `crates/mega-evm/src/external/gas.rs` |
| System Contracts | Addresses, Solidity interfaces, execution semantics, deployment specs, interception behavior | `crates/system-contracts/contracts/`, `crates/mega-evm/src/system/`, address constants in `constants.rs` |
| Resource Limits | Per-transaction limits (compute gas, data size, KV updates, state growth), per-frame limits, forwarding rules | `crates/mega-evm/src/constants.rs`, `crates/mega-evm/src/limit/` |
| Detention | Gas detention caps, volatile data categories, detention trigger conditions | `crates/mega-evm/src/constants.rs`, `crates/mega-evm/src/access/`, `crates/mega-evm/src/evm/host.rs` |
| Upgrades | Spec progression, per-upgrade behavioral deltas, activation order, backward compatibility | `crates/mega-evm/src/evm/spec.rs`, `crates/mega-evm/src/block/hardfork.rs` |
| Precompiles | Precompile addresses, behavior, gas costs | `crates/mega-evm/src/evm/precompiles.rs` |
| CLI Commands | Command names, flags, subcommands, default values in mega-evme docs | `bin/mega-evme/src/cmd.rs`, `bin/mega-evme/src/run/`, `bin/mega-evme/src/tx/`, `bin/mega-evme/src/replay/`, `bin/mega-evme/src/common/` |
| Agent Files | Spec progression lists, system contract tables, source layout descriptions, code path references, unstable spec markers in `AGENTS.md`, `CLAUDE.md`, `docs/spec/AGENTS.md`, `bin/mega-evme/AGENTS.md`, `REVIEW.md`, and `.claude/skills/*/SKILL.md` | Same sources as the claim's primary family (spec progression → `spec.rs`, system contracts → `constants.rs` + `crates/system-contracts/`, source layout → actual directory structure, mega-evme structure → `bin/mega-evme/src/`) |

### Agent File Verification

Agent instruction files (`AGENTS.md`, `CLAUDE.md`, `docs/spec/AGENTS.md`, `bin/mega-evme/AGENTS.md`, `REVIEW.md`, `.claude/skills/*/SKILL.md`) contain code-related claims that can go stale.
Treat them as additional pages to audit.
Key claims to verify:

- **Spec progression list** in `AGENTS.md`: Does it match the actual `MegaSpecId` enum in `spec.rs`?
- **Unstable spec marker** in `AGENTS.md`: Is the marked unstable spec still the latest?
- **System Contracts table** in `AGENTS.md`: Do the contracts, addresses, and purposes match the source?
- **Core Source Layout** in `AGENTS.md`: Do the listed modules and descriptions match the actual directory structure?
- **Code-to-doc mapping tables** in skill files: Are all doc pages and code paths still valid?
- **Hardfork-to-spec mapping** in `AGENTS.md`: Does it match `hardfork.rs`?
- **Test organization** in `AGENTS.md`: Does it match actual test directory structure?
- **mega-evme STRUCTURE** in `bin/mega-evme/AGENTS.md`: Do the listed modules match the actual `bin/mega-evme/src/` directory layout?

## Workflow

### Phase 1: Claim Extraction

Read the target page(s) and extract every verifiable claim.
A claim is any statement that can be checked against source code:

- **Numeric values**: gas costs, limits, caps, multipliers, addresses, bucket sizes.
- **Behavioral rules**: "X always/never happens", "Y reverts when Z", "A is charged before B".
- **Interface definitions**: function signatures, event signatures, error selectors, parameter names.
- **Relationships**: "Spec X introduced feature Y", "Contract Z is available since Rex2".
- **Security claims**: Attack vectors, invariants, and risk consequences stated in Security Considerations sections.

For each claim, record:

- The exact text from the doc.
- The page path and section.
- The claim family.

### Phase 2: Source Resolution

For each claim, locate the authoritative source:

1. **Match the claim family** to the source locations in the table above.
2. **Find the specific source**: search for the constant name, function signature, address, or behavioral code path.
   - For constants: grep for the constant name or numeric value in `crates/mega-evm/src/constants.rs` and related files.
   - For interfaces: read the Solidity contract source in `crates/system-contracts/contracts/`.
   - For upgrade claims: check `spec.rs` and `hardfork.rs` for the spec gate.
   - For behavioral rules: trace the code path that implements the rule.

**Budget**: Do not spend more than ~5 targeted file reads per claim.
If the source cannot be found after 5 reads, mark the claim as Ambiguous.

### Phase 3: Verification

For each claim, compare the doc text against the source and assign a disposition:

| Disposition | Meaning |
|-------------|---------|
| **Verified** | Doc matches source exactly. Record: file path and line number. |
| **Incorrect** | Doc contradicts source. Record: what the doc says, what the source says, and the source location. |
| **Stale** | Doc was correct for a previous spec but is outdated. Record: which spec introduced the change. |
| **Ambiguous** | Source is unclear or claim cannot be verified. Record: what was searched and why. |
| **Version-dependent** | Claim is correct for some specs but not others, and the doc doesn't specify which. |

### Phase 4: Report

```markdown
## Correctness

**Scope**: {what was verified}
**Claims checked**: {count}

### Summary

| Disposition | Count |
|-------------|-------|
| Verified | N |
| Incorrect | N |
| Stale | N |
| Ambiguous | N |
| Version-dependent | N |

### Findings (Incorrect / Stale / Ambiguous / Version-dependent only)

#### C-001: {short description}

- **Severity**: Blocker | Major | Minor
- **Disposition**: {disposition}
- **Claim family**: {family}
- **Page**: {path}
- **Section**: {heading}
- **Doc says**: "{exact text from doc}"
- **Source says**: "{what the source actually shows}"
- **Source location**: `{file}:{line}`
- **Suggested correction**: {concrete edit to the doc text}

#### C-002: ...

### Verified Claims

<details>
<summary>N claims verified</summary>

| # | Page | Claim | Source | Line |
|---|------|-------|--------|------|
| 1 | {path} | {claim summary} | `{file}` | {line} |

</details>
```

**Severity definitions**:

- **Blocker**: Incorrect numeric values (gas costs, limits, addresses), wrong spec attribution, wrong interface signature — would cause implementation bugs.
- **Major**: Stale values from a previous spec, missing version qualification, misleading behavioral description.
- **Minor**: Ambiguous claim that could be misread, minor wording imprecision that doesn't change meaning.

## Rules

- Verify against actual source code, not against other documentation pages.
- If a claim references a specific spec (e.g., "Rex3 introduced X"), verify it was indeed that spec, not an earlier or later one.
- Do NOT fix the issues yourself unless the user explicitly asks. This skill produces a report, not edits.
- When verifying gas constants, check both the constant definition AND its usage context (per-transaction? per-opcode? per-block?).
- When verifying Security Considerations claims, confirm that the stated invariant is real — check that the code actually enforces it.
- Every numeric value MUST be verified against `crates/mega-evm/src/constants.rs` or the relevant source file. Do not trust other documentation pages as a source of truth.
