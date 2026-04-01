---
name: doc-freshness
description: Detects recent code changes in mega-evm that are not yet reflected in docs/. Use when checking for documentation gaps, auditing doc coverage after a release, looking for undocumented changes, or running a periodic doc sweep.
---

# Documentation Freshness Check

Check for undocumented code changes in this repository: $ARGUMENTS

Parse the arguments to determine the time window.
Accepted inputs:

- A duration (e.g., `7d`, `14d`, `30d`) — check merged PRs in that window.
- A date (e.g., `2025-03-01`) — check merged PRs since that date.
- A git ref or tag (e.g., `v0.5.0`) — check merged PRs since that ref.

Default (no arguments): last 14 days.

## What Is Doc-Worthy

A change is doc-worthy if it affects the behavior specified in `docs/`.
The documentation in `docs/` is the formal MegaETH specification covering EVM execution, gas accounting, resource limits, system contracts, and upgrade history.

**Always doc-worthy** (auto-include):

- New or modified gas constants or limits
- New or modified system contract (address, interface, behavior)
- New spec or hardfork introduction
- Opcode behavior changes
- Resource limit changes (compute gas, data size, KV updates, state growth)
- Gas detention rule changes
- Precompile behavior changes
- SELFDESTRUCT semantics changes

**Usually doc-worthy** (include if impact is significant):

- New external environment dependencies
- Changes to block execution logic
- Changes to gas forwarding or call frame semantics

**Internal-only** (exclude):

- Pure refactoring with no behavioral change
- Test-only changes
- CI/CD pipeline changes
- Dependency bumps (unless they change behavior)
- Build system changes
- CLI tool (`mega-evme`) changes (not part of the spec)
- Benchmark changes

When uncertain, include the PR as "Possibly doc-worthy" with a note on why it's ambiguous.

## Code-to-Doc Mapping

| Code path | Affected doc pages |
|-----------|-------------------|
| `crates/mega-evm/src/constants.rs` | `docs/evm/dual-gas-model.md`, `docs/evm/resource-limits.md`, `docs/evm/gas-detention.md`, `docs/evm/gas-forwarding.md`, `docs/evm/contract-limits.md` |
| `crates/mega-evm/src/evm/spec.rs` | `docs/hardfork-spec.md`, `docs/upgrades/overview.md`, `docs/evm/overview.md` |
| `crates/mega-evm/src/evm/instructions.rs` | `docs/evm/dual-gas-model.md`, `docs/evm/gas-detention.md` |
| `crates/mega-evm/src/evm/host.rs` | `docs/evm/gas-detention.md` |
| `crates/mega-evm/src/evm/precompiles.rs` | `docs/evm/precompiles.md` |
| `crates/mega-evm/src/block/` | `docs/hardfork-spec.md`, `docs/evm/overview.md` |
| `crates/mega-evm/src/limit/` | `docs/evm/resource-limits.md`, `docs/evm/resource-accounting.md` |
| `crates/mega-evm/src/access/` | `docs/evm/gas-detention.md` |
| `crates/mega-evm/src/system/` | `docs/system-contracts/*.md` |
| `crates/mega-evm/src/external/` | `docs/evm/dual-gas-model.md`, `docs/system-contracts/oracle.md` |
| `crates/system-contracts/contracts/` | `docs/system-contracts/*.md` |

### Agent and Skill Files

Code changes can also make agent instruction files stale.
These files contain code paths, constant names, system contract tables, and spec references that must stay in sync with the implementation.

| Code path | Affected agent files |
|-----------|---------------------|
| `crates/mega-evm/src/evm/spec.rs` | `AGENTS.md` (spec progression list, unstable spec marker), `CLAUDE.md` |
| `crates/mega-evm/src/block/hardfork.rs` | `AGENTS.md` (hardfork-to-spec mapping) |
| `crates/mega-evm/src/` (new/renamed modules) | `AGENTS.md` (Core Source Layout section) |
| `crates/system-contracts/contracts/` (new contract) | `AGENTS.md` (System Contracts table) |
| `crates/mega-evm/src/constants.rs` | `AGENTS.md` (Key Concepts sections referencing constant names) |
| `crates/mega-evm/src/system/` | `AGENTS.md`, `.claude/skills/doc-impact-check/SKILL.md`, `.claude/skills/doc-freshness/SKILL.md` (code-to-doc mapping tables) |
| `crates/mega-evm/src/limit/` (new tracker) | `AGENTS.md` (Multidimensional Resource Limits section) |
| `docs/` (new pages added to SUMMARY.md) | `.claude/skills/doc-impact-check/SKILL.md`, `.claude/skills/doc-freshness/SKILL.md` (mapping tables need new entries) |

## Workflow

### Phase 1: Collect Recent Changes

Collect merged PRs in the time window:

```bash
gh pr list --repo megaeth-labs/mega-evm --state merged --search "merged:>={since_date}" --json number,title,url,mergedAt,labels,body --limit 100
```

If `gh` is not available, fall back to local git log:

```bash
git log --oneline --since="{since_date}" --merges -- .
```

For each PR, record:

- PR number and title
- Merge date
- Labels (if any)
- A one-line summary of what changed

### Phase 2: Triage

Classify each PR as **doc-worthy** or **internal-only** using the criteria above.
Check PR titles AND bodies — some PRs have uninformative titles but detailed bodies.
When a PR touches both doc-worthy and internal code, classify based on the doc-worthy parts.

### Phase 3: Coverage Search

For each doc-worthy change:

1. Use the code-to-doc mapping to identify target pages, including agent/skill files.
2. Search `docs/` and agent files (`AGENTS.md`, `CLAUDE.md`, `.claude/skills/`) for existing coverage of the feature/constant/behavior.
3. Read the target page(s) and agent file(s) to check if the change is reflected.

Classify coverage:

- **Covered**: The change is already documented accurately.
- **Partially covered**: The feature is mentioned but the specific change is not reflected (e.g., old values, missing new parameters).
- **Not covered**: No mention found in any documentation page.

For each gap, also check whether agent files (`AGENTS.md`, `CLAUDE.md`, `.claude/skills/*/SKILL.md`) reference the affected area and need updating.
Record this in the "Agent file impact" field of the gap finding.

### Phase 4: Prioritize Gaps

| Priority | Criteria |
|----------|----------|
| **P0** | Incorrect values live in docs, security-relevant, or new spec with no upgrade page |
| **P1** | New behavioral change that implementers need to know about |
| **P2** | Minor parameter change, non-breaking addition, or enhancement to existing feature |

### Phase 5: Report

```markdown
## Freshness

**Time window**: {since} to {now}
**PRs reviewed**: {total count}
**Doc-worthy changes**: {count}
**Coverage gaps found**: {count}

### Gaps (ordered by priority)

#### F-001: {short description}

- **Priority**: P0 | P1 | P2
- **Source**: PR #{number} — {title} ({url})
- **Merged**: {date}
- **What changed**: {description of the behavioral change}
- **Coverage**: Not covered | Partially covered
- **Target page**: {doc page path that needs updating}
- **Agent file impact**: {agent files affected, e.g., "AGENTS.md (system contract table)" or "None"}
- **What needs updating**: {specific content that needs to change}

#### F-002: ...

### Covered Changes

<details>
<summary>N changes already documented</summary>

| # | PR | Change | Documented in |
|---|-----|--------|---------------|
| 1 | #{number} | {summary} | `{doc page}` |

</details>

### Internal-Only Changes

<details>
<summary>N internal changes excluded</summary>

| # | PR | Why excluded |
|---|-----|-------------|
| 1 | #{number} — {title} | {reason} |

</details>
```

## Rules

- Always check PR titles AND bodies when triaging.
- If the time window returns more than 100 PRs, note this and suggest narrowing the window.
- Do NOT create or edit documentation pages. This skill produces a gap report only.
- When suggesting target pages, prefer updating existing pages over creating new ones.
- Include the PR URL for every finding so the reader can review the actual change.
