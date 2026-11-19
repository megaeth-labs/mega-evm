#!/usr/bin/env bash
# Decides whether a pull request needs the heavy workflows (the byte-price grid, the long fuzz).
#
# Usage: scripts/pull_request_needs_ci.sh [BASE [HEAD]]     (defaults: HEAD^1 and HEAD, the
#                                                           merge commit of a pull request)
#
# Prints `true` unless every changed file is on the allowlist, and then `false`:
#
# - anything under docs/, and
# - a Markdown file (*.md) outside crates/ and bin/.
#
# Any other change runs the workflows, and so does a change with no files. Markdown under crates/
# and bin/ is not on the list because tests read it: the generated documents there (the pricing
# table, the deviation registry, the mega-evme differences) are rendered by a test and compared to
# the file, and a crate's README is its documentation.
#
# Renames are listed as a deletion and an addition, so a file moved out of the allowlist counts at
# both paths.

set -euo pipefail

base="${1:-HEAD^1}"
head="${2:-HEAD}"

changed=0
while IFS= read -r -d '' file; do
    changed=1
    case "$file" in
        crates/* | bin/*)
            echo true
            exit 0
            ;;
        docs/* | *.md) ;;
        *)
            echo true
            exit 0
            ;;
    esac
done < <(git diff -z --name-only --no-renames "$base" "$head")

if [ "$changed" -eq 0 ]; then
    echo true
else
    echo false
fi
