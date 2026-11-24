---
name: bump-version-release
description: Start a mega-evm release. The bump, tag and GitHub Release are made by the shared release workflows; this skill only dispatches them and explains the steps. Use when releasing a new version, bumping version, creating a release, or tagging a release.
user-invocable: true
---

# Bump Version & Release

Releases are made by the org release pipeline (megaeth-labs/.github).
Three decisions are yours: merging the candidate PR, which cuts the release branch; merging a settle PR when one opens; and approving the pending `release` deployment, which tags and publishes.
Candidate and settle PRs need one each of the `spec:`, `comp:` and `api:` labels.

1. **Candidate** — `gh workflow run release-candidate.yml --ref main -f version=X.Y.Z`.
   This opens a `chore/release-candidate-X.Y.Z` PR that bumps `Cargo.toml` (workspace version, the three path dependencies, `Cargo.lock`) and drafts the `CHANGELOG.md` entry.
   Merging it pushes to `main`, the route decides `cut`, and `release-vX.Y.Z` is created from the merge commit.
   Fixes for the release go to the release branch by PR, and every such fix must also land on `main` (cherry-pick it in a separate PR): the next candidate is cut from `main`, so a fix that exists only on the release branch is lost in the next release.
2. **Settle** — after the candidate lands, check the settle run.
   If it opens or refreshes `chore/release-settle-vX.Y.Z`, review and merge that PR.
   It writes the generated release notes under the candidate's undated `## vX.Y.Z` heading and refuses to settle anything that is not the branch tip.
   If the branch is clean at the candidate merge, there is no settle PR.
   Then publish with `gh workflow run release-settle.yml --ref main -f version=X.Y.Z`; that dispatch also resumes a partial publication.
   A blank version only refreshes every eligible release branch.
   The workflow's only input is `version`; the commit, release branch and version file are computed by the route and the action, so pass no `commit`.
3. **Publish**, on either path — `mega-evme` is built and its version checked first.
   Then approve the pending `release` deployment: it creates the annotated tag `vX.Y.Z` and publishes the GitHub Release with `mega-evme-x86_64-unknown-linux-gnu` and `SHA256SUMS` attached.
   If the build fails, publish does not run and no tag is created.
   The dispatch or the merge is not the approval.
4. **Targets** — automatic on the published Release: `on-release.yml` uploads `mega-evme` to the MegaETH artifact registry, in the `release` environment.
   The registry job builds its own `mega-evme` from the tag, independently of the Release download.
   The GitHub Release is written by the release app; the registry upload credential, `GCP_AUTH_KEY`, lives in the `release` environment, whose deployment policy admits `main`, `release-v*` branches and `v*` tags.
   crates.io publishing is paused while the OP crates are git-pinned (see the root `Cargo.toml`), so no crates are published.
   Rehearse `on-release.yml` first on an existing tag, dispatching on that tag's ref: `gh workflow run on-release.yml --ref vX.Y.Z -f dry_run=true`.
   The tag being released is always the run's own ref; there is no separate tag input.
5. **Recover** a partial publication:
   - Tag present but no published Release: if the tag is on the settle PR merge, rerun the original push-triggered settle run; if it is on the candidate merge, redispatch settle with the same version on `main`.
   - A rerun rebuilds `mega-evme` before publish resumes.
   - A finished Release cannot be backfilled.
   - Only the registry failed: after fixing the cause, `gh workflow run on-release.yml --ref vX.Y.Z -f dry_run=false`.
   - A broken target workflow in the tagged tree needs a new release.
6. **Hotfix** — `gh workflow run release-candidate.yml --ref main -f version=X.Y.Z -f base_commit=vA.B.C`, where `vA.B.C` is the release line to fix and the new version is above the newest tag.
   Then follow the same steps on that release branch.
   The fix must also land on `main`.

Do not create tags or Releases by hand; the tag ruleset rejects it.
