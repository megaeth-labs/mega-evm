#!/usr/bin/env python3
"""Capture pinned MegaETH mainnet blocks for the mega-evme replay corpus.

The corpus is one archive, `corpus.tar.xz`, holding one RPC capture per block as
the member `<N>.cache.json`, plus `manifest.json`, which pins every block and
the digest of every member and of the archive itself.

For every selected block this script:

1. checks that the RPC endpoint serves the block under its pinned hash;
2. seeds `<N>.cache.json` from the block's member of the archive, if it has one;
3. replays the block online with `mega-evme replay --block N --verify-receipt
   --verify-block`, recording every RPC response into that capture
   (`--rpc.capture-file`, which is incremental: only the requests the seed lacks
   are fetched);
4. replays it again offline from the capture alone (`--rpc.replay-file`), and
   requires both runs to verify every receipt and the block itself.

When any selected block's capture changed, or a block was pinned that the
manifest does not list yet, the whole archive is repacked with the
deterministic packer below and `manifest.json` is updated in place: the changed
members' digests, the archive's digest, and new entries in block order with a
`spec` placeholder (`cargo test -p mega-evme --test replay_corpus` reports the
spec the mainnet schedule assigns to a new block). When nothing changed, both
files are left untouched, so the repository history does not move.

Both files are written in full beside their targets first and only then
renamed into place, the archive before the manifest, so an interrupted or
failed run can only stop between the two renames. Every run starts by finishing
such an update (the staged manifest still beside the replaced archive) and by
checking that the manifest describes the archive: its digest, its members, and
each member's digest. A drift the script cannot attribute to its own
interrupted update fails the run with what to do about it; the script never
re-pins the manifest from an archive it cannot vouch for.

Every repack writes the whole archive into the repository history, so batch
recaptures into one change and recapture only the blocks that need it.

See the corpus README for the SALT bucket capacities the captures replay with.

Usage (from the repository root, against a mainnet archive RPC endpoint):

    cargo build --release -p mega-evme
    RPC_URL=<endpoint> python3 scripts/replay_corpus_capture.py \\
        --bin target/release/mega-evme --blocks 24829789

`--pin N:HASH` captures a block the manifest does not list yet. `--check`
repacks the committed archive's members and fails unless the result is
byte-identical to the committed archive, which is how the packer's determinism
is verified; it needs neither an endpoint nor a binary.
"""

import argparse
import hashlib
import io
import json
import lzma
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

MAINNET_CHAIN_ID = 4326
REPO = Path(__file__).resolve().parent.parent
CORPUS = REPO / "bin" / "mega-evme" / "tests" / "fixtures" / "corpus"
SPEC_PLACEHOLDER = "?"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def member_name(number):
    return f"{number}.cache.json"


def block_of(member):
    return int(member.split(".", 1)[0])


def pack(members):
    """Pack `members` (name -> bytes) into the corpus archive's exact bytes.

    The output is a function of the members alone: they are written in block
    order, as ustar entries with fixed metadata (mode 0644, uid and gid 0, empty
    owner names, mtime 0), and the tar is compressed with xz at preset 9 with the
    extreme flag, single-threaded, with a CRC64 check.
    """
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name in sorted(members, key=block_of):
            data = members[name]
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mtime = 0
            info.mode = 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.type = tarfile.REGTYPE
            tar.addfile(info, io.BytesIO(data))
    return lzma.compress(
        buffer.getvalue(),
        format=lzma.FORMAT_XZ,
        check=lzma.CHECK_CRC64,
        preset=9 | lzma.PRESET_EXTREME,
    )


def read_members(archive):
    """Read every member of `archive` (name -> bytes), refusing anything but
    uniquely named regular files."""
    members = {}
    with tarfile.open(archive, "r:xz") as tar:
        for info in tar.getmembers():
            if not info.isfile():
                raise RuntimeError(f"{archive.name}: {info.name} is not a regular file")
            if info.name in members:
                raise RuntimeError(f"{archive.name}: {info.name} appears twice")
            members[info.name] = tar.extractfile(info).read()
    return members


def staged(path):
    """Where `path`'s next content is written before it replaces `path`."""
    return path.with_name(path.name + ".tmp")


def write_staged(path, data):
    """Write `data` in full beside `path`, flushed to disk, without touching
    `path` itself."""
    with open(staged(path), "wb") as file:
        file.write(data)
        file.flush()
        os.fsync(file.fileno())


def publish(archive, packed, manifest_path, manifest_text):
    """Replace the archive and the manifest that describes it.

    Both are staged in full first, so a failure while writing leaves the
    committed pair untouched. Only the two renames remain: the archive goes
    first, and `recover` finishes the manifest's if the run stops in between.
    """
    write_staged(archive, packed)
    write_staged(manifest_path, manifest_text.encode())
    staged(archive).replace(archive)
    staged(manifest_path).replace(manifest_path)


def drift(archive, members, manifest):
    """How `manifest` fails to describe `archive` (holding `members`), one
    line per difference; empty when it describes it exactly."""
    problems = []
    actual = sha256(archive.read_bytes())
    if actual != manifest["sha256"]:
        problems.append(f"{archive.name} has sha256 {actual}; the manifest pins {manifest['sha256']}")
    pinned = {entry["member"]: entry["sha256"] for entry in manifest["blocks"]}
    for name in sorted(set(pinned) - set(members), key=block_of):
        problems.append(f"{name} is in the manifest but not in {archive.name}")
    for name in sorted(set(members) - set(pinned), key=block_of):
        problems.append(f"{name} is in {archive.name} but not in the manifest")
    for name in sorted(set(pinned) & set(members), key=block_of):
        digest = sha256(members[name])
        if digest != pinned[name]:
            problems.append(f"{name} has sha256 {digest}; the manifest pins {pinned[name]}")
    return problems


def recover(archive, manifest_path):
    """Finish or discard an update an earlier run left staged.

    A staged archive means the run stopped before replacing anything, so the
    committed pair is still the old one and the staged files are discarded. A
    staged manifest alone means the archive was already replaced; the staged
    manifest was written by the same run, so it is moved into place once it is
    shown to describe the archive exactly.
    """
    if staged(archive).exists():
        for path in (staged(archive), staged(manifest_path)):
            if path.exists():
                path.unlink()
        print(f"discarded an unfinished update: {archive.name} was never replaced")
        return
    if not staged(manifest_path).exists():
        return
    manifest = json.loads(staged(manifest_path).read_text())
    problems = drift(archive, read_members(archive), manifest)
    if problems:
        raise RuntimeError(
            f"{staged(manifest_path).name} does not describe {archive.name}, so the "
            "interrupted update cannot be finished:\n  " + "\n  ".join(problems)
        )
    staged(manifest_path).replace(manifest_path)
    print(f"finished an interrupted update: {manifest_path.name} now describes {archive.name}")


def require_consistent(archive, members, manifest, manifest_path):
    """Fail unless the manifest describes the archive exactly."""
    problems = drift(archive, members, manifest)
    if problems:
        raise RuntimeError(
            f"{manifest_path.name} does not describe {archive.name}:\n  "
            + "\n  ".join(problems)
            + "\nThe two files only ever change together, and this drift is not an update "
            "this script left unfinished. Restore both from version control (git checkout "
            f"-- {archive} {manifest_path}) and rerun the capture. The script does not re-pin "
            "the manifest from the archive by itself: the manifest is what the corpus test "
            "trusts, and an archive of unknown provenance must not become trusted that way."
        )


def rpc(url, method, params):
    payload = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    # Public endpoints may reject urllib's default `Python-urllib/x.y` user agent with 403.
    headers = {"Content-Type": "application/json", "User-Agent": "mega-evme-corpus-capture"}
    request = urllib.request.Request(url, payload.encode(), headers)
    with urllib.request.urlopen(request, timeout=60) as response:
        answer = json.load(response)
    if answer.get("error"):
        raise RuntimeError(f"{method}: {answer['error']}")
    return answer["result"]


def check_run(path, number, block_hash, tx_hashes):
    """Require one verified line per body transaction, then one verified block line."""
    lines = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    if not lines:
        raise RuntimeError("the run printed nothing")
    *txs, block = lines
    if [line.get("tx_hash") for line in txs] != tx_hashes:
        raise RuntimeError("transaction lines differ from the pinned block body")
    for index, line in enumerate(txs):
        if (
            line.get("error")
            or line.get("verification") != {"match": True}
            or line.get("block_number") != number
            or line.get("tx_index") != index
        ):
            raise RuntimeError(f"transaction {index} did not verify: {line}")
    if block != {
        "block_number": number,
        "block_hash": block_hash,
        "block_verification": {"match": True},
    }:
        raise RuntimeError(f"the block did not verify: {block}")


def replay(command, report):
    with report.with_suffix(".ndjson").open("w") as out, report.with_suffix(
        ".stderr"
    ).open("w") as err:
        run = subprocess.run(command, stdout=out, stderr=err, timeout=3600)
    if run.returncode:
        raise RuntimeError(f"exited {run.returncode}; see {report.with_suffix('.stderr')}")
    return report.with_suffix(".ndjson")


def capture_block(binary, url, number, block_hash, work, seed):
    """Capture one block into `work`, starting from `seed` (its current member's
    bytes, or None), and return the block's body length and its new capture."""
    header = rpc(url, "eth_getBlockByNumber", [hex(number), False])
    if not header or header.get("hash") != block_hash or int(header["number"], 16) != number:
        raise RuntimeError("the endpoint serves a different block at this height")
    tx_hashes = header["transactions"]

    capture = work / member_name(number)
    if seed is not None:
        # Start from the existing capture so only missing requests are fetched.
        capture.write_bytes(seed)

    base = [
        str(binary), "replay", "--block", str(number),
        "--verify-receipt", "--verify-block", "--json",
    ]
    online = base + ["--rpc", url, "--rpc.capture-file", str(capture)]
    offline = base + ["--rpc.replay-file", str(capture)]
    print(f"{number}: capturing {len(tx_hashes)} transaction(s)", flush=True)
    for phase, command in (("online", online), ("offline", offline)):
        check_run(replay(command, work / phase), number, block_hash, tx_hashes)

    data = capture.read_bytes()
    envelope = json.loads(data)
    if envelope.get("chain_id") != MAINNET_CHAIN_ID:
        raise RuntimeError(f"the capture is for chain {envelope.get('chain_id')}")
    if envelope.get("external_env", {}).get("bucket_capacities"):
        raise RuntimeError("the capture carries bucket capacities; the corpus uses none")
    return len(tx_hashes), data


def check(archive, members):
    """Fail unless repacking the archive's members reproduces it byte for byte."""
    committed = archive.read_bytes()
    repacked = pack(members)
    if repacked != committed:
        print(
            f"{archive.name}: repacking its {len(members)} member(s) gives {len(repacked)} "
            f"bytes (sha256 {sha256(repacked)}), not the committed {len(committed)} bytes "
            f"(sha256 {sha256(committed)})",
            file=sys.stderr,
        )
        return 1
    print(
        f"{archive.name}: {len(members)} member(s) repack byte-identically "
        f"({len(committed)} bytes, sha256 {sha256(committed)})"
    )
    return 0


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--bin", help="mega-evme binary to capture with")
    parser.add_argument("--manifest", default=str(CORPUS / "manifest.json"),
                        help="corpus manifest; it names the archive beside it")
    parser.add_argument("--blocks", nargs="+", type=int, default=[],
                        help="manifest blocks to (re)capture")
    parser.add_argument("--pin", nargs="+", default=[], metavar="N:HASH",
                        help="blocks not in the manifest yet, pinned by number and hash")
    parser.add_argument("--check", action="store_true",
                        help="verify that repacking the archive reproduces it, and exit")
    args = parser.parse_args()

    manifest_path = Path(args.manifest).resolve()
    archive = manifest_path.parent / json.loads(manifest_path.read_text())["archive"]
    try:
        recover(archive, manifest_path)
        manifest = json.loads(manifest_path.read_text())
        members = read_members(archive)
        require_consistent(archive, members, manifest, manifest_path)
    except (RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if args.check:
        return check(archive, members)

    if not args.bin:
        parser.error("--bin is required to capture")
    url = os.environ.get("RPC_URL")
    if not url:
        parser.error("set RPC_URL to a mainnet archive endpoint")
    if int(rpc(url, "eth_chainId", []), 16) != MAINNET_CHAIN_ID:
        parser.error(f"RPC_URL is not MegaETH mainnet (chain {MAINNET_CHAIN_ID})")

    known = {entry["number"]: entry for entry in manifest["blocks"]}
    selected = []
    for number in args.blocks:
        if number not in known:
            parser.error(f"block {number} is not in the manifest; pin it with --pin N:HASH")
        selected.append((number, known[number]["hash"]))
    for pin in args.pin:
        number, _, block_hash = pin.partition(":")
        if int(number) in known:
            parser.error(f"block {number} is already in the manifest; select it with --blocks")
        selected.append((int(number), block_hash.lower()))
    if not selected:
        parser.error("select blocks with --blocks and/or --pin")

    binary = Path(args.bin).resolve()
    changed = {}
    failed = []
    for number, block_hash in selected:
        name = member_name(number)
        work = Path(tempfile.mkdtemp(prefix=f"corpus-{number}-"))
        try:
            tx_count, data = capture_block(
                binary, url, number, block_hash, work, members.get(name)
            )
        except Exception as error:  # Report every block, then fail once.
            failed.append(number)
            print(f"{number}: FAILED: {error} (reports in {work})", file=sys.stderr, flush=True)
            continue
        # Only a failed block keeps its work directory, for its reports.
        shutil.rmtree(work)
        if members.get(name) == data:
            print(f"{number}: PASS; capture unchanged", flush=True)
            continue
        members[name] = data
        changed[number] = (block_hash, tx_count, sha256(data))
        print(f"{number}: PASS; capture {'changed' if number in known else 'added'}", flush=True)

    if changed:
        packed = pack(members)
        for number, (block_hash, tx_count, digest) in changed.items():
            entry = known.get(number)
            if entry is None:
                entry = {
                    "number": number,
                    "hash": block_hash,
                    "spec": SPEC_PLACEHOLDER,
                    "tx_count": tx_count,
                    "member": member_name(number),
                }
                known[number] = entry
            entry["sha256"] = digest
            print(f"{number}: manifest entry:\n{json.dumps(entry, indent=2)}", flush=True)
        manifest["blocks"] = [known[number] for number in sorted(known)]
        manifest["sha256"] = sha256(packed)
        publish(archive, packed, manifest_path, json.dumps(manifest, indent=2) + "\n")
        print(f"rewrote {archive.name} ({len(packed)} bytes) and {manifest_path.name}")
    else:
        print(f"nothing changed; {archive.name} and {manifest_path.name} are untouched")
    print(f"captured {len(selected) - len(failed)}/{len(selected)}; failed: {failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
