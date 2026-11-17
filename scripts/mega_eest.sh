#!/usr/bin/env bash
# Runs the Mega EEST conformance suite on this checkout of mega-evm, as CI's Mega EEST gate does.
#
# Usage:
#   scripts/mega_eest.sh
#
# Mega EEST is the conformance suite both Satin implementations share: state-transition cases,
# each pinned to rules of the Satin specification, run on an implementation through an adapter.
# The commit this tree gates on is scripts/mega_eest.env's MEGA_EEST_REV. The script:
#
# 1. Takes Mega EEST at that commit: the checkout $MEGA_EEST_DIR names, which must be at the pin
#    with its cases, specs, adapters and format crate as committed (no change, no untracked file,
#    no file git is told to ignore changes to), or else a shallow fetch of the commit into
#    $MEGA_EEST_WORK/src, reused while it is at the pin.
# 2. Checks the pin's provenance: the commit must be on Mega EEST's default branch (a blobless copy
#    of its history in $MEGA_EEST_WORK/history.git). A commit that is not, such as the branch of a
#    suite change still under review, is a draft state: the cases still run, and the script exits 3.
# 3. When MEGA_EEST_BASE_REV names another commit (a pull request's base pin), compares the two
#    slices' case sets: the case ids removed, added and changed are listed (and written to
#    $MEGA_EEST_WORK/case-diff.md), and removed ids must be exactly those scripts/mega_eest.env's
#    MEGA_EEST_REMOVED_CASES declares, or the script exits 3.
# 4. Builds the suite's `mega-fixture` CLI, and its revm adapter against this checkout: the adapter
#    is written out as a workspace of its own ($MEGA_EEST_WORK/adapter-revm, by scripts/mega_eest.py)
#    whose mega-evm is a path dependency on crates/mega-evm here, whose other crates take their
#    sources from this tree's [workspace.dependencies], and whose [patch] and [profile] are this
#    tree's root manifest's, so a change that moves the revm or op-revm fork is tested with its own
#    fork. The executables run are the ones these builds report (cargo's JSON messages), never a
#    path guessed from the target directory.
#    The lockfile: the adapter's own Cargo.lock was resolved for the mega-evm commit it pins and is
#    not used. The workspace's lock is seeded with this tree's Cargo.lock, cargo resolves it once
#    (pruning what the adapter does not link and adding what only the adapter needs), every package
#    mega-evm links must then be at the version and source this tree locks, with one revm, and the
#    build runs --locked on that lock.
#    Cargo runs from this checkout's root, so the toolchain is its rust-toolchain.toml and the
#    configuration its .cargo/config.toml.
# 5. Consumes every gating case (slice/cases) with `mega-fixture consume` in each mode of
#    $MEGA_EEST_MODES, plain and --instrumented by default, and the suite's deliberately wrong
#    expectation (slice/negative).
# 6. Decides (scripts/mega_eest.py check): in each mode the cases consumed are exactly the slice's
#    index and every one passed; plain and instrumented observations are the same bytes. The wrong
#    expectation, which the index must list, is the control that shows the pipeline can report a
#    failure: when every gating case passed it must fail where the index says; when gating cases
#    failed, the failures show that already, and since a change of behaviour can also move where
#    the control first differs, it need only have run and not passed (failed or erred, anywhere).
#    A failure prints the case id, the first differing path and both values, and the rules the
#    case pins.
#
# Everything goes to stderr but one line on stdout, the summary:
#
#   MEGA-EEST rev=<commit> run=<full|partial> cases=<n> <mode>=pass:<n>,fail:<n>,unsupported:<n>,error:<n> ...
#     [parity=<identical|N-differ>] negative=<...> provenance=<default-branch|unmerged|unchecked>
#     caseset=<unmoved|kept|declared|undeclared|unchecked> result=<pass|FAIL|INCOMPLETE|REFUSED>
#
# `run=full` is both modes, which the gate requires; `run=partial` is one, which the mutation gate
# runs (plain). A run that stops before the reports are read prints
# `MEGA-EEST rev=<commit> run=<...> stage=<step> result=INCOMPLETE` instead.
#
# Exit codes:
#   0  pass: every case passed in every mode run, the wrong expectation failed where recorded, and
#      the pin's checks passed or were skipped (result=pass)
#   1  the cases failed: every mode ran to a complete report of exactly the slice's cases, a case
#      failed or erred or plain and instrumented observations differ, and the wrong expectation ran
#      and did not pass, wherever it first differed (result=FAIL). The only exit that is a verdict
#      on this tree.
#   2  no verdict: anything that kept the run from one, a fetch, a build, the resolution, the lock
#      check, a report missing or incomplete, cases that are not the index, a case the adapter does
#      not support, no mode, a wrong expectation not run or passing, or one that failed elsewhere
#      while every gating case passed (result=INCOMPLETE)
#   3  the cases passed, but the pin is refused: not on Mega EEST's default branch, or it removes
#      cases scripts/mega_eest.env does not declare (result=REFUSED)
#
# Environment:
#   MEGA_EEST_DIR         a Mega EEST checkout at the pin to use instead of fetching one
#   MEGA_EEST_WORK        where the checkout, the adapter workspace, logs and reports go
#                         (default target/mega-eest)
#   MEGA_EEST_TARGET_DIR  the cargo target directory of both builds (default $MEGA_EEST_WORK/target)
#   MEGA_EEST_MODES       the modes to consume in (default "plain instrumented"); at least one
#   MEGA_EEST_JOBS        cases run at once (default: the number of CPUs)
#   MEGA_EEST_BASE_REV    the pin to compare the case set with (CI: the pull request base's pin)
#   MEGA_EEST_SKIP_PROVENANCE=1
#                         skips step 2 (provenance=unchecked): for the mutation gate's runs, which
#                         its warm-up already checked, and the coverage report; never the gate
#   MEGA_EEST_ENGINE_RUSTFLAGS
#                         compiler flags for this tree's packages and the adapter's own crate in
#                         the adapter build, for a coverage build (scripts/coverage_mega_evm.sh):
#                         the crates.io and git crates it links are built without them
#
# Requires git, cargo (rustup, for this tree's toolchain), Foundry (mega-evm's system contracts
# build with forge, as for every build of this tree) and Python 3.11 or newer.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PIN_FILE="$ROOT_DIR/scripts/mega_eest.env"
WORK="${MEGA_EEST_WORK:-$ROOT_DIR/target/mega-eest}"
TARGET="${MEGA_EEST_TARGET_DIR:-$WORK/target}"
MODES="${MEGA_EEST_MODES:-plain instrumented}"
# macOS has no nproc. The count then comes from sysctl, then from POSIX getconf.
JOBS="${MEGA_EEST_JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || getconf _NPROCESSORS_ONLN)}"
HELPER="$ROOT_DIR/scripts/mega_eest.py"

log() { echo "mega-eest: $*" >&2; }
started=$SECONDS
elapsed() { echo "$((SECONDS - started))s"; }

# Every way out before the verdict is exit 2 with an INCOMPLETE summary line, a `die` and an
# unexpected failure under `set -e` alike, so no failure of the run can read as exit 1, the verdict
# that cases failed.
REV=""
RUN=""
STAGE=setup
FINISHED=0
# shellcheck disable=SC2329 # the EXIT trap below runs it
on_exit() {
    local rc=$?
    if [ "$FINISHED" != 1 ] && [ "$rc" -ne 0 ]; then
        echo "MEGA-EEST rev=${REV:-unknown} run=${RUN:-unknown} stage=$STAGE result=INCOMPLETE"
        exit 2
    fi
}
trap on_exit EXIT
die() {
    echo "mega-eest: error: $*" >&2
    exit 2
}

python3 -c 'import sys; sys.exit(sys.version_info < (3, 11))' 2> /dev/null \
    || die "needs Python 3.11 or newer as python3 (for tomllib); found $(python3 --version 2>&1 || echo none)"

# The modes, each once; at least one. Both are the gate's full run.
MODE_WORDS=""
for mode in $MODES; do
    case "$mode" in
        plain | instrumented) ;;
        *) die "MEGA_EEST_MODES names '$mode'; the modes are plain and instrumented" ;;
    esac
    case " $MODE_WORDS " in
        *" $mode "*) ;;
        *) MODE_WORDS="${MODE_WORDS:+$MODE_WORDS }$mode" ;;
    esac
done
[ -n "$MODE_WORDS" ] || die "MEGA_EEST_MODES ('$MODES') names no mode; the modes are plain and instrumented"
case " $MODE_WORDS " in
    *" plain "*" instrumented "* | *" instrumented "*" plain "*) RUN=full ;;
    *) RUN=partial ;;
esac

# One `NAME=value` line of the pin file; anything else fails rather than running another commit.
read_pin() {
    local value
    value=$(sed -n "s/^$1=\(.*\)$/\1/p" "$PIN_FILE")
    if [ -z "$value" ] || [ "$(printf '%s\n' "$value" | grep -c .)" -ne 1 ]; then
        die "$PIN_FILE: no single $1 line"
    fi
    printf '%s' "$value"
}
# At most one `NAME=value` line, whose value may be empty.
read_optional() {
    local count
    count=$(grep -c "^$1=" "$PIN_FILE" || true)
    [ "$count" -le 1 ] || die "$PIN_FILE: more than one $1 line"
    sed -n "s/^$1=\(.*\)$/\1/p" "$PIN_FILE"
}
REPO=$(read_pin MEGA_EEST_REPO)
REV=$(read_pin MEGA_EEST_REV)
[[ "$REV" =~ ^[0-9a-f]{40}$ ]] || die "$PIN_FILE: MEGA_EEST_REV is '$REV', not a full commit hash"
DECLARED_REMOVALS=$(read_optional MEGA_EEST_REMOVED_CASES)

# The paths of a checkout that decide what runs: a change to any of them is a different suite.
SUITE_PATHS=(slice specs adapters crates Cargo.toml Cargo.lock)

# Whether the checkout $1 is the commit $2 with those paths as committed; PIN_PROBLEM says why not.
# Each git command's own status is checked, so a git that cannot read the checkout is no clean
# checkout; and a file marked skip-worktree or assume-unchanged, whose changes `git status` does
# not report, is refused. Git takes no optional lock: several runs may read one checkout at once
# (the mutation gate's build directories).
export GIT_OPTIONAL_LOCKS=0
PIN_PROBLEM=""
at_commit() {
    local dir=$1 rev=$2 head status flags hidden
    if ! head=$(git -C "$dir" rev-parse HEAD 2> /dev/null); then
        PIN_PROBLEM="is no git checkout"
        return 1
    fi
    if [ "$head" != "$rev" ]; then
        PIN_PROBLEM="is at $head"
        return 1
    fi
    if ! status=$(git -C "$dir" status --porcelain --untracked-files=all -- "${SUITE_PATHS[@]}"); then
        PIN_PROBLEM="cannot be read by git status"
        return 1
    fi
    if [ -n "$status" ]; then
        PIN_PROBLEM="changes the suite (${status%%$'\n'*})"
        return 1
    fi
    if ! flags=$(git -C "$dir" ls-files -v -- "${SUITE_PATHS[@]}"); then
        PIN_PROBLEM="cannot be listed by git ls-files"
        return 1
    fi
    if [ -z "$flags" ]; then
        PIN_PROBLEM="tracks none of the suite's files"
        return 1
    fi
    hidden=$(printf '%s\n' "$flags" | awk 'NF && substr($0, 1, 2) != "H " { print; exit }')
    if [ -n "$hidden" ]; then
        PIN_PROBLEM="marks a file skip-worktree or assume-unchanged, whose changes git status does not report ($hidden)"
        return 1
    fi
}

# A shallow fetch of commit $2 into a fresh checkout at $1. One retry: a fetch that fails twice is
# a network failure or a commit the repository does not have, and either stops the run.
fetch_commit() {
    local dir=$1 rev=$2
    rm -rf "$dir"
    mkdir -p "$dir"
    git -C "$dir" init -q
    git -C "$dir" remote add origin "$REPO"
    if ! git -C "$dir" fetch -q --depth 1 origin "$rev"; then
        log "the fetch failed; retrying once"
        sleep 5
        git -C "$dir" fetch -q --depth 1 origin "$rev" \
            || die "cannot fetch $rev from $REPO: the repository has no such commit, or the network failed"
    fi
    git -C "$dir" -c advice.detachedHead=false checkout -q FETCH_HEAD
    at_commit "$dir" "$rev" || die "the fetched checkout of $rev $PIN_PROBLEM"
}

mkdir -p "$WORK"
rm -f "$WORK/adapter.path" "$WORK/case-diff.md"
STAGE=fetch
if [ -n "${MEGA_EEST_DIR:-}" ]; then
    SUITE="$(cd "$MEGA_EEST_DIR" && pwd)"
    at_commit "$SUITE" "$REV" \
        || die "MEGA_EEST_DIR=$SUITE $PIN_PROBLEM, and the pin is $REV: check the pin out there," \
            "unchanged, or move the pin in scripts/mega_eest.env"
    log "Mega EEST at $REV in $SUITE"
elif [ -d "$WORK/src/.git" ] && at_commit "$WORK/src" "$REV"; then
    SUITE="$WORK/src"
    log "Mega EEST at $REV in $SUITE (reused)"
else
    SUITE="$WORK/src"
    log "fetching Mega EEST $REV from $REPO"
    fetch_commit "$SUITE" "$REV"
fi

# The pin's provenance: on Mega EEST's default branch, read from a blobless copy of its history.
PROVENANCE=unchecked
if [ "${MEGA_EEST_SKIP_PROVENANCE:-0}" = 1 ]; then
    log "the pin's provenance is not checked (MEGA_EEST_SKIP_PROVENANCE=1)"
else
    STAGE=provenance
    history="$WORK/history.git"
    if [ ! -d "$history" ] || [ "$(git -C "$history" config remote.origin.url 2> /dev/null)" != "$REPO" ]; then
        rm -rf "$history"
        git clone -q --bare --filter=blob:none "$REPO" "$history" \
            || die "cannot copy Mega EEST's history from $REPO to check the pin's provenance"
    fi
    if ! git -C "$history" fetch -q origin HEAD; then
        log "the fetch of Mega EEST's default branch failed; retrying once"
        sleep 5
        git -C "$history" fetch -q origin HEAD \
            || die "cannot fetch Mega EEST's default branch from $REPO to check the pin's provenance"
    fi
    if git -C "$history" merge-base --is-ancestor "$REV" FETCH_HEAD 2> /dev/null; then
        PROVENANCE=default-branch
        log "the pin is on Mega EEST's default branch"
    else
        PROVENANCE=unmerged
        log "error: the pin $REV is not on Mega EEST's default branch ($(git -C "$history" rev-parse FETCH_HEAD))." \
            "A pin on a suite change still under review is a draft state: the cases run, the gate stays" \
            "red, and the pin moves to the merged commit before this change merges."
    fi
fi

# The case set against the base's pin, when one is given.
CASESET=unchecked
if [ -n "${MEGA_EEST_BASE_REV:-}" ]; then
    STAGE=case-diff
    BASE_REV=$MEGA_EEST_BASE_REV
    [[ "$BASE_REV" =~ ^[0-9a-f]{40}$ ]] || die "MEGA_EEST_BASE_REV is '$BASE_REV', not a full commit hash"
    if [ "$BASE_REV" = "$REV" ]; then
        CASESET=unmoved
        [ -z "$DECLARED_REMOVALS" ] \
            || log "warning: the pin did not move, and MEGA_EEST_REMOVED_CASES still declares removals; clear it"
    else
        BASE="$WORK/base-src"
        if ! { [ -d "$BASE/.git" ] && at_commit "$BASE" "$BASE_REV"; }; then
            log "fetching the base's pin $BASE_REV"
            fetch_commit "$BASE" "$BASE_REV"
        fi
        CASESET=$(python3 "$HELPER" case-diff --base-rev "$BASE_REV" --rev "$REV" \
            --base-index "$BASE/slice/index.json" --index "$SUITE/slice/index.json" \
            --declared "$DECLARED_REMOVALS" --summary "$WORK/case-diff.md") \
            || die "cannot compare the case sets of $BASE_REV and $REV"
    fi
fi

# Every cargo command runs from this checkout's root: rustup then uses its rust-toolchain.toml and
# cargo its .cargo/config.toml, whichever manifest it builds.
cd "$ROOT_DIR"

# The executable named $1 that the build with the arguments after it produced, from cargo's own
# report of its artifacts, so a stale file at another path is never what runs.
built_executable() {
    local name=$1 messages
    shift
    messages=$(CARGO_TARGET_DIR="$TARGET" cargo build --locked --message-format=json-render-diagnostics "$@") \
        || return 1
    printf '%s\n' "$messages" | python3 "$HELPER" executable --name "$name"
}

STAGE=cli-build
log "building the mega-fixture CLI ($(elapsed))"
CLI=$(built_executable mega-fixture --manifest-path "$SUITE/Cargo.toml" -p mega-fixture-format --bin mega-fixture) \
    || die "the suite's mega-fixture CLI does not build"

STAGE=adapter-resolve
ADAPTER_DIR="$WORK/adapter-revm"
python3 "$HELPER" adapter --tree "$ROOT_DIR" --suite "$SUITE" --out "$ADAPTER_DIR" \
    --engine-rustflags "${MEGA_EEST_ENGINE_RUSTFLAGS:-}" \
    || die "cannot write the adapter workspace"
log "resolving the adapter workspace on this tree's lock ($(elapsed))"
cargo metadata --format-version 1 --manifest-path "$ADAPTER_DIR/Cargo.toml" > /dev/null \
    || die "the adapter workspace does not resolve against this tree's manifests"
STAGE=lock-check
python3 "$HELPER" check-lock --tree "$ROOT_DIR" --adapter "$ADAPTER_DIR" \
    || die "the adapter workspace would not build mega-evm as this tree locks it"

STAGE=adapter-build
log "building the revm adapter against this tree's mega-evm ($(elapsed))"
if ! ADAPTER=$(built_executable adapter-revm --manifest-path "$ADAPTER_DIR/Cargo.toml"); then
    die "the Mega EEST revm adapter at $REV does not build against this tree." \
        "If this change alters the mega-evm API the adapter calls, change the adapter in Mega EEST" \
        "first and move the pin in scripts/mega_eest.env to that commit (AGENTS.md, the Mega EEST gate)."
fi
# The adapter this run built, for the coverage report to read.
printf '%s\n' "$ADAPTER" > "$WORK/adapter.path"
"$ADAPTER" capabilities > "$WORK/capabilities.json" || die "the adapter does not report its capabilities"

STAGE=consume
rm -f "$WORK"/plain.* "$WORK"/instrumented.* "$WORK"/negative.*
for mode in $MODE_WORDS; do
    flag=()
    [ "$mode" = instrumented ] && flag=(--instrumented)
    log "consuming slice/cases, $mode, $JOBS at once ($(elapsed))"
    rc=0
    "$CLI" consume --adapter "$ADAPTER" ${flag[@]+"${flag[@]}"} --jobs "$JOBS" --specs "$SUITE/specs" \
        --report "$WORK/$mode.json" "$SUITE/slice/cases" > "$WORK/$mode.log" || rc=$?
    echo "$rc" > "$WORK/$mode.exit"
    grep '^CONSUME ' "$WORK/$mode.log" >&2 || log "$mode: no CONSUME line; see $WORK/$mode.log"
done
log "consuming slice/negative, the wrong expectation ($(elapsed))"
"$CLI" consume --adapter "$ADAPTER" --specs "$SUITE/specs" --report "$WORK/negative.json" \
    "$SUITE/slice/negative" > "$WORK/negative.log" || true

# The checker prints the summary line and exits with the code it names; anything else, a crash
# included, is no verdict.
STAGE=check
status=0
summary=$(python3 "$HELPER" check --suite "$SUITE" --work "$WORK" --modes "$MODE_WORDS" --rev "$REV" \
    --provenance "$PROVENANCE" --caseset "$CASESET") || status=$?
log "done in $(elapsed); logs and reports in $WORK"
case "$status:$summary" in
    "0:MEGA-EEST "*" result=pass" | "1:MEGA-EEST "*" result=FAIL" | "2:MEGA-EEST "*" result=INCOMPLETE" \
        | "3:MEGA-EEST "*" result=REFUSED") ;;
    *) die "the checker exited $status with no summary line naming that result: '$summary'" ;;
esac
echo "$summary"
case "$status" in
    1) log "a case that fails pins behaviour of the Satin specification: fix the change, or, when it" \
        "changes that behaviour on purpose, change the case in Mega EEST first and move the pin" \
        "(AGENTS.md, the Mega EEST gate)" ;;
    3) log "the cases passed, but the pin is refused (provenance=$PROVENANCE caseset=$CASESET)" ;;
esac
FINISHED=1
exit "$status"
