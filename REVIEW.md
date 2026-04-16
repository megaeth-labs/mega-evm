# Code Review Guidelines

## Purpose

The goal of code review is to **improve overall code health over time** while allowing developers to make progress.
A PR that improves the codebase should be approved even if it is not perfect.
There is no "perfect" code — only "better" code.

Beyond defect detection, code review serves as **knowledge transfer** — spreading understanding of the codebase, design decisions, and conventions across the team.

## Principles

### Good taste

"Sometimes you can look at a problem from a different angle and rewrite it so that the special cases disappear and it becomes the normal case." — Linus Torvalds

- Eliminating edge cases through better design is always preferable to adding conditionals
- If an implementation needs more than 3 levels of indentation, it needs restructuring
- Functions should be short, do one thing, and do it well
- Good programmers worry about data structures and relationships, not just the code around them
- Use the type system to express semantics — prefer newtypes over primitive aliases.
  Make invalid states unrepresentable.
- Prefer existing types and structs over inventing new ones.
  Only introduce a new type when existing ones carry wrong semantics.
- For common logic and data structures, prefer well-maintained crates over hand-rolling — even if they are not yet in our dependencies — as long as the dependency does not add disproportionate complexity
- When code implicitly embodies a known pattern, make it explicit.
  Named patterns are easier to review because the reader already has the mental model.
  But only when it simplifies; don't force-fit.

### Pragmatism

- Solve real problems, not hypothetical threats.
  Reject "theoretically perfect" but practically complex designs.
- Does this problem truly occur in production?
  Does the solution's complexity match the severity?
- Backward compatibility matters — changes that break existing spec behavior are bugs, regardless of theoretical correctness

### Review priorities

Focus human attention in this order.
Automate everything below the line.

1. **Data structures** — Are the core data structures right? Bad code around good data structures is fixable; good code around bad data structures is doomed
2. **Design** — Is the overall approach right? Does it belong in this crate/module? Can special cases be eliminated through better design rather than more conditionals?
3. **Correctness** — Does the code do what the author intended? Are there edge cases, off-by-ones, or consensus-critical mistakes?
4. **Complexity** — Can a future developer understand and modify this easily? Could it be simpler? Can the number of concepts be cut in half?
5. **Breakage** — What existing functions could be impacted? Which downstream consumers (mega-reth, test-client) will break?
6. **Tests** — Are they correct, meaningful, and covering the important cases?
7. **Naming and comments** — Are names clear? Do comments explain _why_, not _what_?
8. ~~Style~~ — Defer to `cargo fmt` and `cargo clippy`.
   Never spend human review time on formatting.

## What to look for

### PR description and title

- PR title must follow Conventional Commits format (`feat:`, `fix:`, `chore:`, etc.) and accurately reflect the main change
- PR description must summarize what changed and why — not just list files touched
- Review the description on every update: if new commits shift the scope, the title and description should be updated to match
- If the title or description is misleading or stale, flag it in the review

### Correctness and safety

- New or modified logic has accompanying tests
- Changes to consensus-critical code (opcode behavior, gas computation, state transitions, resource limits) require extra scrutiny
- All execution logic must be **deterministic and architecture-independent** — no `mem::transmute`, no native-endian byte conversions, no platform-dependent operations in consensus paths
- `unsafe` blocks must have a `// SAFETY:` comment explaining why the invariant holds
- Results and errors must always be checked.
  If intentionally ignored, an inline comment must explain why.

### Spec backward compatibility

This is the single most important correctness concern in mega-evm.

- **Existing stable specs must never change behavior.**
  Check `CLAUDE.md` for which spec is currently unstable — all others are frozen.
  New EVM behavior, gas cost changes, or opcode modifications must introduce a new spec and be gated with `spec.is_enabled(MegaSpecId::NEW_SPEC)`.
- System contract changes (Solidity sources or Rust integration) require a new spec
- Modified constants must be gated per-spec — verify that old spec paths still use the old values
- If a PR claims to "fix" behavior for an existing spec, scrutinize whether this changes consensus.
  A true bug fix in an existing spec is rare and must be justified.

### Design and architecture

- Respect revm's design patterns — mega-evm customizes revm through its trait hooks, not by replacing its abstractions
- Changes to public APIs or traits must have a clear reason documented in an inline comment or the PR description
- `no_std` compatibility must be maintained in the `mega-evm` crate — no direct `std::` usage.
  Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;`.
- New workspace dependencies should use `default-features = false` — features are opted-in explicitly
- Per-frame gas mechanisms (stipends, adjustments) must handle all frame termination paths: system contract interception, gas rescue on limit exceed, and frame return.
  Missing any path causes gas leakage.

### Observability

mega-evm is primarily a library crate, so observability is minimal.
When logging or tracing is present:

- Use `tracing` macros, never `println!` or `eprintln!`
- Use structured key-value fields, not string interpolation
- Log levels based on frequency: `error!` for unrecoverable, `warn!` for recoverable anomalies, `info!` for infrequent lifecycle events, `debug!` for investigation, `trace!` for high-frequency paths

### Tests

**Structure:**

- Test names must use the `test_` prefix and state the key object being tested — the function, struct, or behavior under test must be obvious from the name
- Enforce determinism: no `sleep`-based assertions, no wall-clock dependence, no flaky tests
- Cover corner cases — each reachable branch should have a test
- If a change affects cross-component behavior that cannot be covered by unit tests, suggest e2e tests in the review comment (these may live in the test-client repo)

**Assertion quality:**

- Assert the exact expected value, not just `is_ok()` or `is_some()`.
  Use `assert_eq!` / `assert_ne!` over `assert!` for better failure diagnostics.
- For error paths, assert the specific error variant: `assert!(matches!(result, Err(MyError::Specific(..))))`, not just `is_err()`
- Use `#[should_panic(expected = "specific message")]` rather than bare `#[should_panic]`
- Test both the output AND relevant side effects (state mutations, gas consumption, resource limit tracking).
  A test that only checks the return value may miss silent corruption.
- Assert absence of unintended changes too — if a function returns a struct, assert fields that should NOT change as well

**Test oracle:**

- When exact output is hard to specify, assert invariants: round-trip (encode/decode), idempotency, monotonicity
- Use a simplified reference implementation as oracle when available — compare the optimized version's output against it
- For stateful systems (resource limit trackers, gas stipend lifecycle), assert state-machine invariants after each transition, not just at the end
- Mentally ask "what mutation to the code would this test NOT catch?" — if the answer is "many", the assertion is too weak

**Rust-specific:**

- Derive or implement `Debug` and `PartialEq` on types under test so assertion failures produce readable diffs
- Ban bare `unwrap()` as the only "check" and tests with no assertions — these prove nothing

## How to review

### Giving feedback

- **Label severity**: prefix optional suggestions with `nit:` so the author knows what blocks approval vs what is advisory.
- **Critique the code, not the person**: say "this code does X" not "you did X wrong"
- **Explain why**: link to docs or prior incidents, not just "change this to that"
- **One comment per issue, in the most relevant location**: do not repeat the same point in multiple places
- **Be concise**: one line problem, one line fix.
  No preambles or "looks good overall" filler.

### When to approve

- The PR improves the overall health of the codebase
- Design is sound and fits the architecture
- No correctness or safety issues
- Test coverage is adequate for the risk level
- It is OK to approve with nits — the author can address them before merging
- **Approve when the PR improves code health**, even if you'd write it differently.
  Do not let "perfect" block "better".

### Handling previous comments

- Before writing new comments, check all previous review threads on this PR
- If the author or others have replied to previous review comments, read those replies and respond if necessary
- If a previous comment has been addressed by the latest changes, resolve that thread
- Do not repeat feedback that has already been addressed
- To read all threads, replies, and their thread IDs:
  `gh api graphql -f query='{ repository(owner:"OWNER", name:"REPO") { pullRequest(number:NUMBER) { reviewThreads(first:50) { nodes { id isResolved comments(first:20) { nodes { author { login } body path } } } } } } }'`
- To reply to a thread:
  `gh api graphql -f query='mutation { addPullRequestReviewThreadReply(input:{ pullRequestReviewThreadId:"THREAD_ID", body:"Your reply" }) { comment { id } } }'`
- To resolve a thread:
  `gh api graphql -f query='mutation { resolveReviewThread(input:{threadId:"THREAD_ID"}) { thread { id } } }'`

## What NOT to flag

### Skip list

- Formatting-only changes already enforced by `cargo fmt`
- Lint issues already caught by `cargo clippy`
- Dependency ordering already enforced by `cargo sort`
- Redundant guards that aid readability (e.g., explicit `is_some()` check before unwrap even when the branch guarantees it)
- Threshold/constant values that are tuned empirically — comments on these rot
- Issues already addressed in the diff being reviewed — read the FULL diff before commenting

### Reviewer anti-patterns

- **Bikeshedding** — spending disproportionate time on trivial matters (variable names, brace placement) while ignoring architectural concerns
- **Rubber stamping** — approving without reading the diff
- **Nitpick avalanche** — leaving many minor comments without distinguishing severity
- **Gatekeeping** — blocking merges over stylistic preferences or hypothetical future concerns
- **Manufacturing feedback** — if the PR is clean, a single "LGTM" is sufficient.
  Do not comment just to have something to say.
