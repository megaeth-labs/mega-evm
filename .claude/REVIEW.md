# PR #202 Review: MegaEVM Book Documentation

## Overall Assessment

Strong documentation PR.
The book provides a well-structured, informative companion to the normative specs.
Writing is clear, consistent, and follows SITE_DOC_FORMAT.md faithfully.
Constants and formulas are overwhelmingly accurate against source code (`crates/mega-evm/src/constants.rs`).

~~Two structural weaknesses: the top-level Overview is too thin to orient readers, and the EVM overview page tries to be both an introduction and an exhaustive reference card, serving neither audience optimally.~~

**Update**: Overview.md has been expanded with motivation, audience routing, and spec progression summaries.
EVM README.md now describes Rex3 (latest stable) behavior with Rex4 in hint boxes.

Findings below are organized by severity, considering both audiences:
1. **App builders** — need to understand current behavior to develop and optimize dApps.
2. **Node builders** — need unambiguous details and full change history.

---

## 🔴 Spec Soundness Issues

### S1. Contract creation double-charge not stated in storage gas table

**Status**: ✅ **INVALID** — The book was correct; the normative spec was wrong.

PR #207 clarified that contract creation storage gas (32K×(m−1)) **subsumes** account creation — they are not additive.
Account creation storage gas (25K×(m−1)) applies only to CALL-initiated account creation (value transfer to empty account).
The book's storage gas table correctly lists them as separate line items with distinct triggers.

### S2. Gas detention evolution annotation conflates Rex3 values with Rex4 semantics

**Status**: ✅ **RESOLVED**

The evolution table was removed from main content.
Gas detention page now describes only Rex3 (latest stable) absolute cap behavior.
Rex4 relative cap is in a hint box.
History section points to upgrade pages.

### S3. Resource limits table: compute gas block limit shown as "—" instead of "Unlimited"

**Status**: ✅ **RESOLVED**

Changed to "Unlimited".
MiniRex table removed entirely (latest-stable-only rule).

### S4. MiniRex storage gas table implies a contract-creation vs account-creation distinction that doesn't exist in MiniRex

**Status**: ✅ **INVALID** — Non-issue.

Both rows show the same cost (2M × multiplier), which is correct.
Listing them separately is a presentational choice that aids readability — readers can look up their operation directly.
Whether the implementation uses one constant or two is an internal detail, not a spec concern.

---

## 🟡 Completeness Issues

### C1. Missing: High-Precision Timestamp system contract page

**Status**: ✅ **RESOLVED**

Created `book/src/oracle-services/timestamp.md` with interface, usage guidance, storage layout, guarantees, and gas detention impact.
Added to `SUMMARY.md` under a new "Oracle Services" section.
EVM README and system-contracts README link to it.

### C2. Missing: Dedicated pages for MegaAccessControl and MegaLimitControl

**Status**: ⚠️ **DEFERRED** — Rex4 is unstable.

These are Rex4 system contracts.
They are documented in the Rex4 hint boxes and the `upgrades/rex4.md` page.
Dedicated pages under `system-contracts/` can be added when Rex4 is stabilized.

### C3. Missing: EQUIVALENCE baseline description

**Status**: ✅ **INVALID** — Adequately covered.

EQUIVALENCE is described in both `spec-system.md` and `Overview.md`.
It doesn't change EVM behavior (just adds internal access tracking on top of Optimism Isthmus), so there's no "Previous → New" delta for an upgrade page.
A dedicated page would be empty padding.

### C4. Calldata storage gas derivation is implicit

**Status**: ✅ **RESOLVED**

Table notes in both `evm/README.md` and `evm/dual-gas-model.md` now show the derivation: "10 × standard EVM zero-byte cost (4)", "10 × EIP-7623 floor cost for non-zero bytes (40)", etc.

### C5. LOG revert behavior note is placed in the wrong context

**Status**: ❌ **OPEN** (in `book/src/evm/README.md`)

The LOG rows in the storage gas table still include:
"Storage gas is permanent regardless of revert; data size usage is rolled back on revert."

This mixes two resource dimensions in a storage gas table.

**Fix**: Simplify the table note to "Permanent regardless of revert" and state the full revert semantics once below the table or defer to the Resource Accounting page.

### C6. "Post-Execution" phase heading could mislead

**Status**: ✅ **RESOLVED**

Renamed to "Phase 2: Runtime Enforcement (Precise)".

---

## 🟢 Minor / Polish Issues

### M1. Alloy EVM URL is wrong

**Status**: ✅ **RESOLVED**

Changed to `https://github.com/alloy-rs/evm` in `Overview.md`.

### M2. `spec-system.md` calls EQUIVALENCE "the default spec"

**Status**: ✅ **RESOLVED**

Changed to "The baseline spec."

### M3. Glossary missing Rex4 ABI-observable terms

**Status**: ⚠️ **PARTIALLY RESOLVED**

`MegaLimitExceeded` is now mentioned in the `Call-frame-local exceed` glossary entry.
No dedicated glossary entries for `MegaAccessControl` or `MegaLimitControl` — deferred since Rex4 is unstable.

### M4. `oracle.md` — `sendHint` declared `view` but has sequencer-level side effects

**Status**: ✅ **RESOLVED**

Added `@dev` comment explaining: "Declared `view` because it does not mutate on-chain state; the hint is processed by the sequencer outside the EVM."

### M5. Beneficiary not defined inline on the EVM overview page

**Status**: ✅ **RESOLVED**

"beneficiary" is now linked to the glossary entry which defines it as "the block coinbase address".

### M6. Upgrade page references to normative specs — verify GitBook link resolution

**Status**: ⚠️ **CANNOT VERIFY** — requires GitBook deployment to test.

Book-internal relative links use `../evm/dual-gas-model.md` style.
These should resolve in GitBook but need runtime verification.

---

## 📐 Structural Suggestion: Overview Pages

**Status**: ✅ **RESOLVED**

- `Overview.md` expanded with "Why a Modified EVM?" motivation, "How to Use This Book" audience routing, expanded spec progression with per-spec summaries, backward compatibility guarantee, and unstable spec warning.
- `evm/README.md` now describes Rex3 (latest stable) in main content, Rex4 in hint boxes.
- Spec names link to `spec-system.md` sections; each spec section links to its upgrade page.

---

## ✅ What's Done Well

- All numeric constants verified correct against source code — no discrepancies in core values.
- Spec-to-book traceability is excellent — every normative spec change maps to a book upgrade page.
- The "What Changed" / "Previous behavior" / "New behavior" structure is consistent across all upgrade pages.
- Rex4 unstable disclaimer is properly flagged everywhere.
- Resource Accounting page is an excellent node-builder reference.
- ~~Gas Detention evolution table provides clear per-spec history.~~ Replaced with pointer to upgrade pages (latest-stable-only rule).
- Style guide and SITE_DOC_FORMAT are well-written meta-documents ensuring future consistency.
- System contract registry with addresses, hardfork provenance, and interceptor pattern notes.
- New Oracle Services section with High-Precision Timestamp page.
- Comprehensive glossary with SALT, call frame, and all MegaEVM-specific terms.
- Consistent use of "call frame" terminology (not bare "frame").
- "MegaEVM" used as canonical name throughout.

---

## Summary of Open Items

| ID | Issue | Severity |
|----|-------|----------|
| C4 | Calldata storage gas derivation (10× EVM cost) not stated | 🟡 |
| C5 | LOG revert note mixes resource dimensions in storage gas table | 🟡 |
| C6 | "Post-Execution" phase heading is misleading | 🟡 |
| M4 | `sendHint` view modifier not explained | 🟢 |
