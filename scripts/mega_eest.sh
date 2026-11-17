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
#    with its cases, specs, adapters and format crate unmodified, or else a shallow fetch of the
#    commit into $MEGA_EEST_WORK/src, reused while it is at the pin.
# 2. Builds the suite's `mega-fixture` CLI, and its revm adapter against this checkout: the adapter
#    is written out as a workspace of its own ($MEGA_EEST_WORK/adapter-revm, by scripts/mega_eest.py)
#    whose mega-evm is a path dependency on crates/mega-evm here, whose other crates take their
#    sources from this tree's [workspace.dependencies], and whose [patch] and [profile] are this
#    tree's root manifest's, so a change that moves the revm or op-revm fork is tested with its own
#    fork.
#    The lockfile: the adapter's own Cargo.lock was resolved for the mega-evm commit it pins and is
#    not used. The workspace's lock is seeded with this tree's Cargo.lock, cargo resolves it once
#    (pruning what the adapter does not link and adding what only the adapter needs), every package
#    mega-evm links must then be at the version and source this tree locks, with one revm, and the
#    build runs --locked on that lock.
#    Cargo runs from this checkout's root, so the toolchain is its rust-toolchain.toml and the
#    configuration its .cargo/config.toml.
# 3. Consumes every gating case (slice/cases) with `mega-fixture consume`, plain and then
#    --instrumented, and the suite's deliberately wrong expectation (slice/negative).
# 4. Decides (scripts/mega_eest.py check): in each mode the cases consumed are exactly the slice's
#    index and every one passed; plain and instrumented observations are the same bytes; the wrong
#    expectation fails where the index says. A failure prints the case id, the first differing path
#    and both values, and the rules the case pins.
#
# It prints one summary line on stdout, `MEGA-EEST rev=... cases=... plain=pass:N,... result=pass`,
# everything else on stderr, and exits non-zero when anything above does not hold: a fetch of a
# commit that does not exist, an adapter that does not build against this tree (an API it calls
# changed), a case that does not pass.
#
# Environment:
#   MEGA_EEST_DIR         a Mega EEST checkout at the pin to use instead of fetching one
#   MEGA_EEST_WORK        where the checkout, the adapter workspace, logs and reports go
#                         (default target/mega-eest)
#   MEGA_EEST_TARGET_DIR  the cargo target directory of both builds (default $MEGA_EEST_WORK/target)
#   MEGA_EEST_MODES       the modes to consume in (default "plain instrumented")
#   MEGA_EEST_JOBS        cases run at once (default: the number of CPUs)
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
die() {
    echo "mega-eest: error: $*" >&2
    exit 1
}
started=$SECONDS
elapsed() { echo "$((SECONDS - started))s"; }

python3 -c 'import sys; sys.exit(sys.version_info < (3, 11))' 2> /dev/null \
    || die "needs Python 3.11 or newer as python3 (for tomllib); found $(python3 --version 2>&1 || echo none)"

for mode in $MODES; do
    case "$mode" in
        plain | instrumented) ;;
        *) die "MEGA_EEST_MODES names '$mode'; the modes are plain and instrumented" ;;
    esac
done

# One `NAME=value` line of the pin file; anything else fails rather than running another commit.
read_pin() {
    local value
    value=$(sed -n "s/^$1=\(.*\)$/\1/p" "$PIN_FILE")
    if [ -z "$value" ] || [ "$(printf '%s\n' "$value" | grep -c .)" -ne 1 ]; then
        die "$PIN_FILE: no single $1 line"
    fi
    printf '%s' "$value"
}
REPO=$(read_pin MEGA_EEST_REPO)
REV=$(read_pin MEGA_EEST_REV)
[[ "$REV" =~ ^[0-9a-f]{40}$ ]] || die "$PIN_FILE: MEGA_EEST_REV is '$REV', not a full commit hash"

# The paths of a checkout that decide what runs: a change to any of them is a different suite.
SUITE_PATHS=(slice specs adapters crates Cargo.toml Cargo.lock)

# Whether the checkout $1 is the pinned commit with those paths as committed. Git takes no optional
# lock for it: several runs may read one checkout at once (the mutation gate's build directories).
export GIT_OPTIONAL_LOCKS=0
at_pin() {
    [ "$(git -C "$1" rev-parse HEAD 2> /dev/null)" = "$REV" ] \
        && [ -z "$(git -C "$1" status --porcelain --untracked-files=all -- "${SUITE_PATHS[@]}")" ]
}

mkdir -p "$WORK"
if [ -n "${MEGA_EEST_DIR:-}" ]; then
    SUITE="$(cd "$MEGA_EEST_DIR" && pwd)"
    if ! at_pin "$SUITE"; then
        head=$(git -C "$SUITE" rev-parse HEAD 2> /dev/null || echo "no commit")
        die "MEGA_EEST_DIR=$SUITE is at $head, or changes the suite, and the pin is $REV:" \
            "check the pin out there, or move the pin in scripts/mega_eest.env"
    fi
    log "Mega EEST at $REV in $SUITE"
else
    SUITE="$WORK/src"
    if [ -d "$SUITE/.git" ] && at_pin "$SUITE"; then
        log "Mega EEST at $REV in $SUITE (reused)"
    else
        log "fetching Mega EEST $REV from $REPO"
        rm -rf "$SUITE"
        mkdir -p "$SUITE"
        git -C "$SUITE" init -q
        git -C "$SUITE" remote add origin "$REPO"
        # One retry: a fetch that fails twice is a network failure or a commit the repository does
        # not have, and either fails the gate.
        if ! git -C "$SUITE" fetch -q --depth 1 origin "$REV"; then
            log "the fetch failed; retrying once"
            sleep 5
            git -C "$SUITE" fetch -q --depth 1 origin "$REV" \
                || die "cannot fetch $REV from $REPO: the repository has no such commit, or the network failed"
        fi
        git -C "$SUITE" -c advice.detachedHead=false checkout -q FETCH_HEAD
        at_pin "$SUITE" || die "the fetched checkout is not at $REV"
    fi
fi

# Every cargo command runs from this checkout's root: rustup then uses its rust-toolchain.toml and
# cargo its .cargo/config.toml, whichever manifest it builds.
cd "$ROOT_DIR"

log "building the mega-fixture CLI ($(elapsed))"
CARGO_TARGET_DIR="$TARGET" cargo build --locked --manifest-path "$SUITE/Cargo.toml" \
    -p mega-fixture-format --bin mega-fixture \
    || die "the suite's mega-fixture CLI does not build"
CLI="$TARGET/debug/mega-fixture"

ADAPTER_DIR="$WORK/adapter-revm"
python3 "$HELPER" adapter --tree "$ROOT_DIR" --suite "$SUITE" --out "$ADAPTER_DIR" \
    --engine-rustflags "${MEGA_EEST_ENGINE_RUSTFLAGS:-}" \
    || die "cannot write the adapter workspace"
log "resolving the adapter workspace on this tree's lock ($(elapsed))"
cargo metadata --format-version 1 --manifest-path "$ADAPTER_DIR/Cargo.toml" > /dev/null \
    || die "the adapter workspace does not resolve against this tree's manifests"
python3 "$HELPER" check-lock --tree "$ROOT_DIR" --adapter "$ADAPTER_DIR"

log "building the revm adapter against this tree's mega-evm ($(elapsed))"
if ! CARGO_TARGET_DIR="$TARGET" cargo build --locked --manifest-path "$ADAPTER_DIR/Cargo.toml"; then
    die "the Mega EEST revm adapter at $REV does not build against this tree." \
        "If this change alters the mega-evm API the adapter calls, change the adapter in Mega EEST" \
        "first and move the pin in scripts/mega_eest.env to that commit (AGENTS.md, the Mega EEST gate)."
fi
ADAPTER="$TARGET/debug/adapter-revm"
"$ADAPTER" capabilities > "$WORK/capabilities.json" || die "the adapter does not report its capabilities"

rm -f "$WORK"/plain.* "$WORK"/instrumented.* "$WORK"/negative.*
for mode in $MODES; do
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

status=0
python3 "$HELPER" check --suite "$SUITE" --work "$WORK" --modes "$MODES" --rev "$REV" || status=$?
log "done in $(elapsed); logs and reports in $WORK"
if [ "$status" -ne 0 ]; then
    log "a case that fails pins behaviour of the Satin specification: fix the change, or, when it" \
        "changes that behaviour on purpose, change the case in Mega EEST first and move the pin" \
        "(AGENTS.md, the Mega EEST gate)"
fi
exit "$status"
