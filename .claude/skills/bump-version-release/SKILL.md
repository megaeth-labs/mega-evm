---
name: bump-version-release
description: Start a mega-evm release. The bump, tag and GitHub Release are now made by the shared release workflows; this skill only dispatches them and explains the steps. Use when releasing a new version, bumping version, creating a release, or tagging a release.
user-invocable: true
---

# Bump Version & Release

Releases are made by the org release pipeline (megaeth-labs/.github).

1. **Candidate** — `gh workflow run release-candidate.yml --ref main -f version=X.Y.Z`.
   This opens a PR that bumps `Cargo.toml` (workspace version, the three path dependencies, `Cargo.lock`) and drafts the `CHANGELOG.md` entry.
   Merging it cuts `release-vX.Y.Z`.
   Fixes for the release go to that branch by PR, and every such fix must also land on `main` (cherry-pick it in a separate PR): the next candidate is cut from `main`, so a fix that exists only on the release branch is lost in the next release.
2. **Settle** — `gh workflow run release-settle.yml --ref main -f version=X.Y.Z -f commit=<tip sha of release-vX.Y.Z>`.
   The dispatch is the release approval: the run waits for the `release` environment's reviewer, and the action checks that the dispatcher is a repository admin.
   The same job then commits the finalised changelog entry to the release branch, creates the annotated tag `vX.Y.Z`, and publishes the GitHub Release with the entry as notes.
   There is no settle PR; `release-publish.yml` only serves the PR mode this repo no longer uses.
   Fix changelog wording before dispatching: in the candidate PR while it is open, or by a PR onto the release branch after it merges.
3. **Targets** — automatic on the Release: `on-release.yml` publishes the four crates to crates.io and uploads `mega-evme` to the MegaETH artifact registry and the Release page.
   Rehearse it first on an existing tag, dispatching on that tag's ref: `gh workflow run on-release.yml --ref vX.Y.Z -f dry_run=true`.
   The tag being released is always the run's own ref; there is no separate tag input.
   The publish credentials live in the `publish` environment, whose deployment policy allows only `v*` tag refs.

Do not create tags or Releases by hand; the tag ruleset rejects it.
