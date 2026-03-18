---
name: bump-version-release
description: Use when releasing a new version, bumping version, or creating a GitHub release.
user-invocable: true
---

# Bump Version & Release

Bump the project version and create a GitHub release: $ARGUMENTS

## Overview

This skill handles the full release workflow:

1. Check if a version bump is already pending/merged on main
2. Bump the workspace version in Cargo.toml and Cargo.lock
3. Create a PR for the version bump
4. Wait for CI to pass
5. User merges the PR
6. Create a git tag and push it
7. Verify the tag on remote matches expectations
8. Create a GitHub release with auto-generated notes

## Step 0: Detect existing version bump progress

Before starting a new version bump, check if one is already in progress.
Run these checks in order and take the first matching action.

### 0a. Fetch latest remote state

```bash
git fetch origin main
```

### 0b. Check if we're on a version bump branch

Check the current branch name:

```bash
git branch --show-current
```

If the current branch matches a version bump pattern (e.g., `*/chore/bump-v*`), extract the version from the branch name.
Then check whether a PR already exists for this branch:

```bash
gh pr list --head "$(git branch --show-current)" --state all --json number,state,url,title
```

- If **an open PR exists**: inform the user, then **skip to Step 3** (wait for CI / merge).
- If **a merged PR exists**: extract the version and **skip to Step 5** (tag & release).
- If **no PR exists but the branch has commits ahead of main**: **skip to Step 2e** (push and create PR).
- If **no PR exists and no commits ahead**: the branch is empty — continue to Step 1 to determine the version, then proceed to Step 2b (skip branch creation since we're already on it).

### 0c. Check for open version bump PRs on any branch

```bash
gh pr list --search "bump version in:title" --state open --json number,title,url,headRefName
```

If an open version bump PR exists:

- Inform the user about the existing PR (show title and URL).
- Ask the user whether to continue with that PR or start a new one.
- If continuing: check out that branch and **skip to Step 3**.

### 0d. Check if main already has a version bump as the latest commit

```bash
git log origin/main -1 --oneline
```

If the latest commit message on `main` matches a version bump pattern (e.g., starts with `chore: bump version`), extract the version from that commit and **skip to Step 5**.

### 0e. No existing progress found

Proceed to Step 1.

## Step 1: Determine version bump type

Ask the user which version component to bump:

- **Patch** (e.g., 1.4.0 → 1.4.1) — bug fixes, minor changes
- **Minor** (e.g., 1.4.1 → 1.5.0) — new features, backward-compatible
- **Major** (e.g., 1.4.1 → 2.0.0) — breaking changes

Read the current version from `Cargo.toml` (`workspace.package.version`) and compute the new version.

## Step 2: Create version bump PR

### 2a. Create a branch

```bash
git checkout origin/main -b {username}/chore/bump-v{new_version}
```

Use `git config user.name` to determine the username (lowercase first name).

### 2b. Update Cargo.toml

Edit the `version` field under `[workspace.package]` in the root `Cargo.toml`.
Only change the version line — do not modify anything else.

### 2c. Update Cargo.lock

Run `cargo check` to regenerate `Cargo.lock` with the new version.

### 2d. Commit changes

Stage only `Cargo.toml` and `Cargo.lock`:

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to {new_version}"
```

Do NOT include any AI attribution in the commit message.

### 2e. Push and create PR

```bash
git push -u origin {branch-name}
gh pr create --base main --title "chore: bump version to {new_version}" --body "$(cat <<'EOF'
## Summary
- Bump workspace version from {old_version} to {new_version}
EOF
)"
```

## Step 3: Wait for CI

Use `gh pr checks` with `--watch` to monitor CI status:

```bash
gh pr checks --watch --fail-fast
```

If CI fails:

1. Inspect the failure with `gh pr checks`
2. Attempt to fix the issue
3. Commit the fix and push
4. Re-check CI

## Step 4: Ask user to merge

Tell the user the PR is ready and ask them to merge it on GitHub.
Provide the PR URL.
Wait for the user to confirm the merge before proceeding.

After merge confirmation, update local main:

```bash
git fetch origin main
```

## Step 5: Check if tag exists

Determine the version from the merge commit on main (or from the version computed in Step 1).

```bash
git tag -l "v{version}"
```

If the tag already exists, **skip to Step 7** (verify it still).

## Step 6: Create and push tag

Create a lightweight tag on the merge commit:

```bash
git tag "v{version}" origin/main
git push origin "v{version}"
```

## Step 7: Verify tag on remote

After pushing the tag (or if the tag already existed), verify it points to the expected commit on remote:

```bash
git ls-remote origin "refs/tags/v{version}"
```

Compare the commit SHA from the tag with the expected merge commit on `origin/main`:

```bash
git rev-parse origin/main
```

If the SHAs do not match, warn the user and stop — do not proceed with the release.
If they match, continue.

## Step 8: Create GitHub release

Use `gh release create` with auto-generated release notes:

```bash
gh release create "v{version}" --generate-notes --title "v{version}"
```

This uses GitHub's Release Notes API to automatically generate notes from merged PRs since the last release.

Print the release URL when done.
