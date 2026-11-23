#!/usr/bin/env bash
set -euo pipefail

scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT

cp -R crates/system-contracts "$scratch/"
crate_dir="$scratch/system-contracts"
probe_version=$(python3 - "$crate_dir/contracts/KeylessDeploy.sol" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
source = path.read_text()
matches = list(re.finditer(
    r'function version\(\) external pure returns \(string memory\) \{\s*return '
    r'"(\d+\.\d+\.\d+)";',
    source,
))
if len(matches) != 1:
    raise SystemExit("Could not change the KeylessDeploy.version() return value")
match = matches[0]
major, minor, patch = match[1].split(".")
probe_version = f"{major}.{minor}.{int(patch) + 1}"
path.write_text(source[:match.start(1)] + probe_version + source[match.end(1):])
print(probe_version)
PY
)

if mise run system-contracts:attest "$crate_dir" >"$scratch/attest.log" 2>&1; then
    cat "$scratch/attest.log"
    echo "Attestation accepted a changed KeylessDeploy.version()" >&2
    exit 1
fi

if ! grep -Fq 'KeylessDeploy contract bytecode mismatch' "$scratch/attest.log" ||
    ! grep -Fq "Compiled version $probe_version and code hash" "$scratch/attest.log"; then
    cat "$scratch/attest.log"
    echo "Attestation failed without detecting the changed KeylessDeploy.version()" >&2
    exit 1
fi

echo "Attestation rejected the changed KeylessDeploy.version()"
