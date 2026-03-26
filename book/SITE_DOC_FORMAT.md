# Book Documentation Guide

This file defines conventions and rules for writing and editing the public-facing book (`book/src/`), hosted via GitBook.
The book is the specification for the MegaETH blockchain's execution layer — covering MegaEVM, system contracts, oracle services, resource metering, and the upgrade history.

## Audience

The book serves two audiences:

1. **App builders** — need to understand current MegaEVM behavior to develop and optimize dApps.
2. **Node builders** — need unambiguous behavioral details and the full history of changes across specs.

### Audience routing by section

| Section | Primary audience | Purpose |
|---|---|---|
| `Overview.md` | Both | Landing page — motivation, audience routing, spec progression |
| `evm/README.md` | App builders | Quick reference for current MegaEVM behavior (latest stable spec) |
| `evm/*.md` (concept pages) | Node builders | Detailed mechanics of specific MegaEVM features |
| `hardfork-spec.md` | Node builders | Hardfork vs spec definitions, full progression, links to upgrade pages |
| `system-contracts/` | App builders | How to use system contracts (interfaces, addresses, examples) |
| `oracle-services/` | App builders | How to use oracle-backed services (interfaces, code examples) |
| `upgrades/` | Node builders | What changed per spec, previous vs new behavior, compatibility boundaries |
| `glossary.md` | Both | Definitions of MegaETH-specific terms |

## Writing rules

Soundness and completeness of the specification come first.
Readability is important but must never compromise accuracy.

### Accuracy

- **Verify documentation against implementation, but never expose implementation details.**
  The doc writer should always check that documentation matches the actual implementation.
  However, the public page must never reference implementation details — no code paths, function names, struct names, internal dispatch flow, or test file mappings.
- **Verify constants against source code.**
  Every numeric value (gas limits, storage gas bases, detention caps, contract addresses) must match `crates/mega-evm/src/constants.rs` and related source files.
  Do not copy values from other docs without verification.
- **Attribute behavior to the correct spec.**
  If a behavior was introduced in Rex3, say "Rex3", not "Rex4" — even if Rex4 inherits it.
  Each spec's contribution must be precisely scoped.
- **Do not invent guarantees.**
  The documentation must not introduce guarantees or constraints that are not present in the repository spec.

### Spec versioning

- **Main content describes the latest stable spec only.**
  Pages outside of `upgrades/` and `hardfork-spec.md` must present only the behavior of the latest stable (activated) spec in their main content.
  Unstable spec behavior should be placed in a GitBook info hint box (e.g., `{% hint style="info" %}**Rex4 (unstable): ...**{% endhint %}`), not in the main prose or tables.
  Previous stable spec behavior (e.g., MiniRex values superseded by Rex) should also not appear in the main content — it belongs in the corresponding upgrade page under `upgrades/`.
  Each upgrade page must include "Previous behavior" for every changed behavior, so readers can deduce the full history by reading upgrade pages in sequence.
- **State backward compatibility explicitly.**
  The guarantee that stable specs are frozen must appear in the Overview, the Hardforks and Specs page, and each upgrade page.
- **Mark the unstable spec explicitly everywhere it appears.**
  The unstable spec must be labeled in the spec progression diagram, the spec summary list, the spec's heading, and with a GitBook warning hint.
  When a new spec is introduced or the unstable spec is stabilized, update all these locations.

### Terminology and naming

- **Use `MegaEVM` as the official name.**
  `MegaEVM` is the canonical name for the EVM implementation.
  `MegaETH EVM` is an acceptable synonym but `MegaEVM` is preferred.
  Use `MegaEVM` consistently in headings, first mentions, and formal contexts.
- **Every MegaETH-specific term must be in the glossary.**
  If a concept is unique to MegaETH (not standard EVM/Ethereum), it belongs in `glossary.md`.
  Include links to external repositories where appropriate (e.g., SALT → the salt repo).

### Linking

- **Link glossary terms on first use.**
  The first mention of a MegaETH-specific term on a page should link to its glossary entry.
  Do not over-link — subsequent uses on the same page should not be linked.
- **Link common concepts to the glossary.**
  References to common concepts (e.g., call frame, compute gas, storage gas, volatile data, SALT bucket, multiplier, beneficiary, detained limit) should link to their glossary entry on first use per page.
- **Link spec names to hardfork-spec.md.**
  Whenever a spec is mentioned by name (e.g., MiniRex, Rex, Rex3), link it to the corresponding section heading in `hardfork-spec.md`.
  Each spec section in `hardfork-spec.md` should in turn link to its upgrade page under `upgrades/`.
- **Anchor targets must be headings.**
  Any content that is the target of a `#fragment` link must be a markdown heading (`##`, `###`, etc.), not bold text or other inline formatting.
  GitBook only generates anchors from headings.

### Style

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
- Use unambiguous table values.
  Write "Unlimited" or "No limit", not "—".
  Dashes are ambiguous (could mean "not applicable", "unknown", or "unlimited").
- One sentence, one line.

## Page templates

### Overview pages (`Overview.md` and `*/README.md`)

Overview pages orient the reader before diving into details.

**Required elements:**

1. **Orientation paragraph** — what this section covers and why it matters.
2. **Audience routing or content summary** — where to start based on your role (top-level), or a table/list of what the section contains (section-level).
3. **Summary table or feature list** — registry table, history table, or key feature list as appropriate.

**Top-level `Overview.md` additionally includes:**

- Motivation (why MegaETH differs from standard Ethereum)
- Reference implementation version table
- Spec progression with backward-compatibility notice and unstable spec warning

**Do not** use an overview page as a pure reference card — provide a brief orientation before tables and details.

### Topic pages (concepts, system contracts, oracle services)

Topic pages cover a single concept or component in detail.
This template applies to `evm/*.md` concept pages, `system-contracts/*.md` pages, and `oracle-services/*.md` pages.

**Required elements:**

1. **Title** — `# <Topic Name>`.
2. **Opening paragraph** — what it is, why it matters, in 1–3 sentences.
3. **Core content** — tables, mechanics, rules, or formulas as appropriate for the topic.

**Optional elements (include when applicable):**

- **Contract address and spec** — for system contracts: the address and "Available since: \<SpecName\>".
- **Solidity interface** — with NatSpec, for system contracts and oracle services.
- **Code examples** — usage snippets for developer-facing pages.
- **Trust assumptions or warnings** — use `{% hint style="warning" %}` for trust boundaries or migration risks.
- **Unstable spec behavior** — use `{% hint style="info" %}` for behavior in the unstable spec, never in main prose or tables.
- **Cross-links** — to related topic pages, upgrade pages, or glossary entries.

**Each system contract must have its own dedicated page** under `system-contracts/` with the contract interface, usage guidance, and deployment history.

### Hardforks and Specs page (`hardfork-spec.md`)

This page is the central reference for spec and hardfork definitions.

**Required elements:**

1. **Spec vs hardfork distinction** — define both concepts and their relationship.
2. **Spec progression** — ordered list or diagram of all specs, with the unstable spec clearly marked via `{% hint style="warning" %}`.
3. **Per-spec sections** — one section per spec, each containing:
   - Brief summary of what the spec introduces.
   - Link to the corresponding upgrade page under `upgrades/`.
4. **Backward compatibility statement** — that stable specs are frozen.

### Upgrade pages (`upgrades/*.md`)

Upgrade pages are informative summaries of repository specs.
Treat the repository spec (`specs/*.md`) as the normative source of truth.
If the page conflicts with the repository spec, the repository spec wins.

**Required structure:**

1. **Frontmatter** (YAML) — every upgrade page starts with:

   ```yaml
   ---
   description: One-sentence summary of what this spec upgrade changes, for search engines and link previews.
   ---
   ```

2. **Title** — use the format `<SpecName> Network Upgrade` (e.g., `Rex4 Network Upgrade`).

   Immediately after the title, add an informative notice:

   ```md
   This page is an informative summary of the <SpecName> specification.
   For the full normative definition, see the <SpecName> spec in the mega-evm repository.
   ```

   For the unstable spec, add `{% hint style="warning" %}` after the notice.

3. **Summary** — two to four short paragraphs.
   Explain what changed, what problem it solves, and the most important impact for developers.
   Include the high-level motivation for the upgrade here — why this change was needed, in user-facing terms.
   Do not include implementation details.

4. **What Changed** — one subsection per spec change, using exactly this shape:

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

5. **Developer Impact** — what contract authors, integrators, and tooling authors need to care about.
   Focus on observable behavior and design implications.
   Address the developer directly — use "you" and "your".

6. **Safety and Compatibility** — state backward-compatibility boundaries clearly.
   State failure-mode differences such as revert versus halt when relevant.
   State whether pre-upgrade behavior remains unchanged for older specs.

7. **References** — link only high-value supporting documents.
   Do not use relative paths to files inside the source repository — the public page lives in a separate docs repo.
   Link to the repository itself (e.g., `https://github.com/megaeth-labs/mega-evm`) and mention the spec path.

**Allowed content in upgrade pages:**

- Contract addresses that are part of the developer-facing interface.
- Solidity interface definitions (with NatSpec) for new system contracts introduced in the spec.
- Error signatures and revert reasons that developers may encounter.
- Formulas and constants that define observable behavior (e.g., budget forwarding ratios, gas caps).

The page should be self-contained — do not link users to external sources for essential reading.

### Glossary (`glossary.md`)

The glossary is a flat reference of MegaETH-specific terms.

**Format for each entry:**

```md
**Term** — Definition in 1–4 sentences.
```

**Rules:**

- No subsections or tables — flat list only.
- Link to concept pages and external repos where relevant.
- Mark unstable-spec terms inline: `*(Rex4, unstable)*`.
- Every MegaETH-specific concept that appears in the book must have a glossary entry.

## Checklist

Before finalizing any page:

- [ ] The repository spec remains the semantic source of truth.
- [ ] All required elements for this page type are present.
- [ ] Constants and values match source code.
- [ ] Behavior is attributed to the correct spec.
- [ ] Main content describes the latest stable spec only (unstable in hint boxes).
- [ ] Backward compatibility is stated where required.
- [ ] Glossary terms and spec names are linked on first use.
- [ ] No implementation details are exposed.
- [ ] One sentence, one line.
