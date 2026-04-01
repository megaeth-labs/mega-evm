---
name: doc-impact-check
description: Analyzes a PR diff to determine if documentation under docs/ needs updating. Use when a PR modifies EVM behavior, gas constants, system contracts, opcode semantics, resource limits, or spec definitions in mega-evm source code.
---

# PR Documentation Impact Check

Analyze the current PR diff to determine whether documentation under `docs/` needs updating.

## Context

This skill runs in CI on pull requests that modify source code under `crates/mega-evm/src/` or `crates/system-contracts/`.
It checks whether the code changes affect documented behavior and whether the PR already includes corresponding doc updates.

The documentation in `docs/` is the formal MegaETH specification.
Read `docs/AGENTS.md` for the writing rules and conventions.

## Code-to-Doc Mapping

Use this mapping to identify which doc pages are potentially affected by code changes.

| Code path | What changes here | Affected doc pages |
|-----------|-------------------|--------------------|
| `crates/mega-evm/src/constants.rs` | Gas limits, resource limits, detention caps, multipliers | `docs/evm/dual-gas-model.md`, `docs/evm/resource-limits.md`, `docs/evm/gas-detention.md`, `docs/evm/gas-forwarding.md`, `docs/evm/contract-limits.md` |
| `crates/mega-evm/src/evm/spec.rs` | Spec definitions, spec progression | `docs/hardfork-spec.md`, `docs/upgrades/overview.md`, `docs/evm/overview.md` |
| `crates/mega-evm/src/evm/instructions.rs` | Opcode behavior, compute gas wrapping, gas detention enforcement | `docs/evm/dual-gas-model.md`, `docs/evm/gas-detention.md` |
| `crates/mega-evm/src/evm/host.rs` | Host hooks, volatile data access tracking | `docs/evm/gas-detention.md` |
| `crates/mega-evm/src/evm/precompiles.rs` | Precompile behavior | `docs/evm/precompiles.md` |
| `crates/mega-evm/src/block/` | Block execution, hardfork mapping, executor | `docs/hardfork-spec.md`, `docs/evm/overview.md` |
| `crates/mega-evm/src/limit/` | Resource limit tracking (compute gas, data size, KV updates, state growth) | `docs/evm/resource-limits.md`, `docs/evm/resource-accounting.md` |
| `crates/mega-evm/src/limit/storage_call_stipend.rs` | Storage gas stipend lifecycle | `docs/evm/gas-forwarding.md` |
| `crates/mega-evm/src/access/` | Block env access tracking, volatile data detection | `docs/evm/gas-detention.md` |
| `crates/mega-evm/src/system/` | System contract integration, call interception | `docs/system-contracts/*.md` |
| `crates/mega-evm/src/external/` | SALT environment, oracle environment, dynamic gas cost | `docs/evm/dual-gas-model.md`, `docs/system-contracts/oracle.md` |
| `crates/system-contracts/contracts/` | Solidity system contract sources | `docs/system-contracts/*.md` |

### Agent and Skill Files

Code changes can also make agent instruction files stale.
These files contain code paths, constant names, system contract tables, and spec references that must stay in sync with the implementation.

| Code path | Affected agent files |
|-----------|---------------------|
| `crates/mega-evm/src/evm/spec.rs` | `AGENTS.md` (spec progression list, unstable spec marker), `CLAUDE.md` (same content) |
| `crates/mega-evm/src/block/hardfork.rs` | `AGENTS.md` (hardfork-to-spec mapping) |
| `crates/mega-evm/src/` (new/renamed modules) | `AGENTS.md` (Core Source Layout section) |
| `crates/system-contracts/contracts/` (new contract) | `AGENTS.md` (System Contracts table) |
| `crates/mega-evm/src/constants.rs` | `AGENTS.md` (Key Concepts sections referencing constant names) |
| `crates/mega-evm/src/system/` | `AGENTS.md` (System Contracts section), `.claude/skills/doc-impact-check/SKILL.md` and `.claude/skills/doc-freshness/SKILL.md` (code-to-doc mapping tables) |
| `crates/mega-evm/src/limit/` (new tracker) | `AGENTS.md` (Multidimensional Resource Limits section) |
| `docs/` (new pages added to SUMMARY.md) | `.claude/skills/doc-impact-check/SKILL.md` and `.claude/skills/doc-freshness/SKILL.md` (code-to-doc mapping tables need new entries) |

## Workflow

### Phase 1: Read the Diff

```bash
gh pr diff $PR_NUMBER
```

Identify all changed files and classify each as:
- **Behavioral code change**: Modifies EVM semantics, gas costs, resource limits, system contract logic, spec definitions.
- **Test-only change**: Only adds/modifies tests. No doc impact.
- **Refactoring**: Restructures code without changing behavior. No doc impact.
- **Doc change**: Already modifies files under `docs/`. Note which pages are updated.

Focus on behavioral code changes only.

### Phase 2: Map Changes to Doc Pages

For each behavioral code change:

1. Use the code-to-doc mapping table above to identify potentially affected pages.
2. Also check the agent file mapping table — code changes may affect `AGENTS.md`, `CLAUDE.md`, or skill files.
3. Read the affected source code to understand *what* changed (new constant value? new spec gate? new opcode behavior?).
4. Read the potentially affected doc and agent files to check if the current content matches the new behavior.

### Phase 3: Check for Existing Doc Updates

Check if the PR already includes changes to the affected doc pages:
- If the PR updates the relevant doc pages, verify the updates are consistent with the code changes.
- If the PR does NOT update the relevant doc pages, flag them.

### Phase 4: Report

Post a single PR comment with findings.

**If doc or agent file updates are needed:**

```markdown
## Documentation Impact

This PR modifies EVM behavior that is documented in `docs/` or referenced in agent files. The following files may need updating:

### Spec Documentation

| Doc page | Reason |
|----------|--------|
| `docs/evm/dual-gas-model.md` | {what changed and why the page is affected} |
| `docs/evm/resource-limits.md` | {what changed and why the page is affected} |

{If the PR introduces a new spec}: A new upgrade page under `docs/upgrades/` is also needed.

### Agent / Skill Files

| File | Reason |
|------|--------|
| `AGENTS.md` | {e.g., spec progression list needs new spec, system contract table needs new entry} |

These updates can be included in this PR or in a follow-up.
```

**If no doc updates are needed:**

Do NOT post a comment.
Only comment when there is an actionable finding.

**If the PR already includes correct doc updates:**

Do NOT post a comment.
The PR review job handles general review.

## Rules

- Only flag genuine behavioral changes that affect documented behavior.
  Do NOT flag refactorings, test additions, or internal restructuring.
- Be specific about *what* in the docs needs updating.
  "This page may need updating" is not actionable.
  "`COMPUTE_GAS_LIMIT` changed from 1,000,000,000 to 2,000,000,000 — update the Constants table in `docs/evm/resource-limits.md`" is actionable.
- Respect the spec's backward compatibility rule: if the change introduces a new spec, note that a new upgrade page is needed under `docs/upgrades/`.
- Do NOT edit documentation yourself. This skill produces a comment, not edits.
- Do NOT duplicate the work of the `pr-review` job. Focus exclusively on doc impact.
- If uncertain whether a change is behavioral, err on the side of flagging it — a false positive is better than a missed doc gap.
