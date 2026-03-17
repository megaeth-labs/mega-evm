# Rex ~ Rex3 Spec Rewrite Plan

Based on the [Spec Document Style Guide](STYLE-GUIDE.md), this document describes the concrete changes needed to migrate Rex.md, Rex1.md, Rex2.md, and Rex3.md to the standard format.

## Scope

- **In scope**: Rex.md, Rex1.md, Rex2.md, Rex3.md and their companion impl docs.
- **Out of scope**: MiniRex.md (much larger, separate effort), Rex4.md (already in target format).

## Per-Spec Plan

### Rex.md

**Current state**: Numbered sections (1–5), inline implementation references, descriptive prose.

**Normative spec rewrite**:
- Abstract: Rex is the second spec of MegaETH EVM. Summarize the 4 change areas.
- Changes (4 items):
  1. Transaction intrinsic storage gas — motivation: MiniRex has no intrinsic storage gas component; semantics: 39,000 storage gas added.
  2. Storage gas economics — motivation: MiniRex formula `base × multiplier` overcharges at multiplier=1; semantics: new `base × (multiplier - 1)` formulas. Keep the formula comparison tables and the summary table.
  3. Consistent behavior among CALL-like opcodes — motivation: DELEGATECALL/STATICCALL/CALLCODE bypass gas forwarding cap and oracle detection; semantics: 98/100 forwarding for all, oracle detection for STATICCALL. Keep the opcode behavior table.
  4. Transaction and block limits — motivation: adjust resource limits for production workloads; semantics: data size 4x increase, KV update 4x increase, compute gas 5x decrease, state growth new limit. Keep the summary table.
- Invariants: extract from semantics (e.g., storage gas with multiplier=1 MUST be zero, state growth limit MUST apply at both tx and block level).
- Inheritance: Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague.
- References: remove inline impl refs, link to companion docs and predecessor spec.

**Companion docs to create**:
- `impl/Rex-Implementation-References.md` — move content from current section 4.
- `impl/Rex-Behavior-Details.md` — move the detailed "What Counts as State Growth", "Impact" notes, enforcement details.

### Rex1.md

**Current state**: Problem/fix narrative style, inline implementation references.

**Normative spec rewrite**:
- Abstract: Rex1 is the first patch to the Rex hardfork. It fixes a compute gas limit persistence bug.
- Changes (1 item):
  1. Compute gas limit reset between transactions — motivation: lowered limit from volatile access in one tx affects subsequent txs; semantics: compute gas limit MUST reset to configured value at the start of each transaction.
- Invariants: e.g., volatile data access gas detention MUST NOT affect subsequent transactions.
- Inheritance: Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague.

**Companion docs to create**:
- `impl/Rex1-Behavior-Details.md` — move the bug background narrative (section 2), "What Gets Reset" / "What Remains Unchanged" details.
- `impl/Rex1-Implementation-References.md` — move implementation references (section 5), with spec clauses / implementation / tests structure.

### Rex2.md

**Current state**: 2 changes, some implementation references, Solidity interface inline.

**Normative spec rewrite**:
- Abstract: Rex2 is the second patch to the Rex hardfork. It re-enables SELFDESTRUCT and introduces KeylessDeploy.
- Changes (2 items):
  1. SELFDESTRUCT re-enabled (EIP-6780) — motivation: complete disable was too restrictive; semantics: EIP-6780 rules (same-tx creation allows full destruct, otherwise balance transfer only).
  2. KeylessDeploy system contract — motivation: MegaETH gas model causes keyless deploy txs to fail; semantics: system contract MUST provide keyless deploy with gas limit override, MUST only be callable at depth 0, unknown selectors MUST fall through.
- Invariants: e.g., SELFDESTRUCT of non-same-tx contract MUST NOT delete code or storage.
- Inheritance: Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague.

**Companion docs to create**:
- `impl/Rex2-Behavior-Details.md` — move Solidity interface, usage notes, depth-0 restriction rationale.
- `impl/Rex2-Implementation-References.md` — move current implementation references.

### Rex3.md

**Current state**: 3 changes, inline implementation references.

**Normative spec rewrite**:
- Abstract: Rex3 is the third patch to the Rex hardfork. Summarize 3 changes.
- Changes (3 items):
  1. Oracle access compute gas limit increase — motivation: 1M cap too restrictive for legitimate oracle usage; semantics: oracle access cap MUST be 20M (previously 1M).
  2. Oracle gas detention triggers on SLOAD — motivation: CALL-based trigger is imprecise; semantics: detention MUST be triggered by SLOAD from oracle storage, not by CALL to oracle address. DELEGATECALL MUST NOT trigger. MEGA_SYSTEM_ADDRESS exemption checks tx sender.
  3. Keyless deploy compute gas tracking — motivation: sandbox overhead gas not counted toward compute limit; semantics: 100K overhead MUST be recorded as compute gas.
- Invariants: e.g., DELEGATECALL to oracle MUST NOT trigger detention, MEGA_SYSTEM_ADDRESS tx MUST be exempted from oracle detention.
- Inheritance: Rex3 -> Rex2 -> Rex1 -> Rex -> MiniRex -> Optimism Isthmus -> Ethereum Prague.

**Companion docs to create**:
- `impl/Rex3-Behavior-Details.md` — move detailed SLOAD semantics, MEGA_SYSTEM_ADDRESS exemption details, call chain examples.
- `impl/Rex3-Implementation-References.md` — move current implementation references.

## Execution Order

1. Rex (largest, establishes the pattern)
2. Rex1 (smallest, quick)
3. Rex2
4. Rex3
5. Final review: verify cross-references between all specs are correct.

## Open Questions

1. **MiniRex**: Should we plan a follow-up rewrite for MiniRex.md? It is ~20KB and would be a separate significant effort.
2. **Rex4 references update**: After rewrite, Rex4 references to Rex3 etc. should still be valid (file names unchanged). No action needed unless we rename files.
