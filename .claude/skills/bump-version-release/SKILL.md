---
name: bump-version-release
description: Start a mega-evm release. The bump, tag and GitHub Release are now made by the shared release workflows; this skill only dispatches them and explains the steps. Use when releasing a new version, bumping version, creating a release, or tagging a release.
user-invocable: true
---

# Bump Version & Release

Releases are made by the org release pipeline (megaeth-labs/.github):

1. **Candidate** — `gh workflow run release-candidate.yml --ref main -f version=X.Y.Z`
   opens a PR that bumps `Cargo.toml` (workspace version, the three path
   dependencies, `Cargo.lock`) and drafts the `CHANGELOG.md` entry. Merging it
   cuts `release-vX.Y.Z`. Fixes for the release go to that branch by PR.
2. **Settle** — `gh workflow run release-settle.yml --ref main -f version=X.Y.Z -f commit=<tip sha of release-vX.Y.Z>`
   opens a PR onto the release branch with the finalised changelog entry.
   Merging it is the release approval.
3. **Publish** — automatic on that merge: annotated tag `vX.Y.Z`, GitHub
   Release with the entry as notes; `publish.yml` then publishes the crates
   to crates.io as before.

Do not create tags or Releases by hand; the tag ruleset rejects it.
