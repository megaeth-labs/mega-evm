---
name: bump-version-release
description: Start a mega-evm release. The bump, tag and GitHub Release are made by the shared release workflows; this skill only dispatches them and explains the steps. Use when releasing a new version, bumping version, creating a release, or tagging a release.
user-invocable: true
---

# Bump Version & Release

Releases are made by the org release pipeline (megaeth-labs/.github). Two decisions are yours: merging the candidate PR, which cuts the release branch, and merging the settle PR, which publishes.

1. **Candidate** — `gh workflow run release-candidate.yml --ref main -f version=X.Y.Z`.
   This opens a `chore/release-candidate-X.Y.Z` PR that bumps `Cargo.toml` (workspace version, the three path dependencies, `Cargo.lock`) and drafts the `CHANGELOG.md` entry.
   Merging it pushes to `main`, the route decides `cut`, and `release-vX.Y.Z` is created from the merge commit.
   Fixes for the release go to the release branch by PR, and every such fix must also land on `main` (cherry-pick it in a separate PR): the next candidate is cut from `main`, so a fix that exists only on the release branch is lost in the next release.
2. **Settle** — nothing to dispatch: the push to `release-vX.Y.Z` routes to `refresh`, and the `release-settle` action opens or updates `chore/release-settle-vX.Y.Z` onto the release branch. It writes the generated release notes under the candidate's undated `## vX.Y.Z` heading and refuses to settle anything that is not the branch tip.
   To regenerate it after a changelog edit, `gh workflow run release-settle.yml --ref main -f version=X.Y.Z` — the workflow's only input is `version`; the commit, release branch and version file are computed by the route and the action, so pass no `commit`.
   Merging the settle PR is the settlement decision.
3. **Publish** — merging the settle PR pushes to `release-vX.Y.Z`, which routes to `publish`. That job runs in the `release` environment and waits for its reviewer, then creates the annotated tag `vX.Y.Z` on the merge commit and publishes the GitHub Release with the settle PR's notes. The dispatch is not the approval; the merge plus the environment reviewer is.
4. **Targets** — automatic on the Release: `on-release.yml` publishes the four crates to crates.io and uploads `mega-evme` to the MegaETH artifact registry and the Release page, in the `release` environment.
   Rehearse it first on an existing tag, dispatching on that tag's ref: `gh workflow run on-release.yml --ref vX.Y.Z -f dry_run=true`.
   The tag being released is always the run's own ref; there is no separate tag input.
   The publish credentials — `CARGO_REGISTRY_TOKEN` and `GCP_AUTH_KEY` — live in the `release` environment, whose deployment policy admits `main`, `release-v*` branches and `v*` tags.

Do not create tags or Releases by hand; the tag ruleset rejects it.
