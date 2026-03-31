#!/usr/bin/env bash
# Replay Benchmark Runner wrapper.
#
# Usage:
#   ./replay_runner.sh correctness
#   ./replay_runner.sh perf <baseline> <feature> [rounds]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
exec python3 "${SCRIPT_DIR}/replay_runner.py" "$@"
