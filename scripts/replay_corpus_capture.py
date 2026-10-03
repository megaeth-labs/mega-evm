#!/usr/bin/env python3
"""Capture pinned MegaETH mainnet blocks for the mega-evme replay corpus.

For every selected block this script:

1. checks that the RPC endpoint serves the block under its pinned hash;
2. replays the block online with `mega-evme replay --block N --verify-receipt
   --verify-block`, recording every RPC response into `<N>.cache.json`
   (`--rpc.capture-file`);
3. replays it again offline from that capture alone (`--rpc.replay-file`), and
   requires both runs to verify every receipt and the block itself;
4. packs the capture as `<N>.cache.json.tar.xz` (a tar holding only that file,
   compressed with xz at its highest preset) into the corpus directory;
5. prints the block's manifest entry, ready to paste into `manifest.json`.

Recapture only the blocks that need it: every archive written into the corpus
directory is committed to the repository history for good. When an archive for
the block already exists, its capture is extracted first and the online replay
starts from it, so only the requests it lacks are fetched (`--rpc.capture-file`
is incremental). When the replay needed nothing new, the existing archive is
left untouched, so its digest — and the repository history — do not move.

The captures carry no SALT bucket capacities, so every bucket replays at the
minimum capacity, as the corpus manifest declares (`"salt": "default-minimum"`).

Usage (from the repository root, against a mainnet archive RPC endpoint):

    cargo build --release -p mega-evme
    RPC_URL=<endpoint> python3 scripts/replay_corpus_capture.py \\
        --bin target/release/mega-evme --blocks 24829789

Blocks are selected from the manifest by number; `--pin N:HASH` captures a block
the manifest does not list yet. A new block's printed entry carries a `spec`
placeholder: `cargo test -p mega-evme --test replay_corpus` reports the spec the
mainnet schedule assigns to it.
"""

import argparse
import hashlib
import io
import json
import lzma
import os
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


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


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


def pack(capture, archive):
    """Write `archive` as an xz-compressed tar holding only `capture`."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        info = tar.gettarinfo(str(capture), arcname=capture.name)
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        info.mode = 0o644
        with capture.open("rb") as handle:
            tar.addfile(info, handle)
    temporary = archive.with_name(archive.name + ".tmp")
    temporary.write_bytes(lzma.compress(buffer.getvalue(), preset=9 | lzma.PRESET_EXTREME))
    temporary.replace(archive)


def capture_block(binary, url, out, number, block_hash, work):
    header = rpc(url, "eth_getBlockByNumber", [hex(number), False])
    if not header or header.get("hash") != block_hash or int(header["number"], 16) != number:
        raise RuntimeError("the endpoint serves a different block at this height")
    tx_hashes = header["transactions"]

    capture = work / f"{number}.cache.json"
    archive = out / f"{number}.cache.json.tar.xz"
    existing = None
    if archive.exists():
        # Start from the existing capture so only missing requests are fetched.
        with tarfile.open(archive, "r:xz") as tar:
            members = tar.getnames()
            if members != [capture.name]:
                raise RuntimeError(f"{archive.name} holds {members}, not {capture.name}")
            if hasattr(tarfile, "data_filter"):
                tar.extractall(work, filter="data")
            else:
                tar.extractall(work)
        existing = sha256(capture)

    base = [
        str(binary), "replay", "--block", str(number),
        "--verify-receipt", "--verify-block", "--json",
    ]
    online = base + ["--rpc", url, "--rpc.capture-file", str(capture)]
    offline = base + ["--rpc.replay-file", str(capture)]
    print(f"{number}: capturing {len(tx_hashes)} transaction(s)", flush=True)
    for phase, command in (("online", online), ("offline", offline)):
        check_run(replay(command, work / phase), number, block_hash, tx_hashes)

    envelope = json.loads(capture.read_text())
    if envelope.get("chain_id") != MAINNET_CHAIN_ID:
        raise RuntimeError(f"the capture is for chain {envelope.get('chain_id')}")
    if envelope.get("external_env", {}).get("bucket_capacities"):
        raise RuntimeError("the capture carries bucket capacities; the corpus uses none")

    if existing == sha256(capture):
        print(f"{number}: capture unchanged; keeping {archive.name}", flush=True)
    else:
        pack(capture, archive)
    return {
        "number": number,
        "hash": block_hash,
        "spec": SPEC_PLACEHOLDER,
        "tx_count": len(tx_hashes),
        "archive": archive.name,
        "sha256": sha256(archive),
    }


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--bin", required=True, help="mega-evme binary to capture with")
    parser.add_argument("--manifest", default=str(CORPUS / "manifest.json"))
    parser.add_argument("--out", default=str(CORPUS), help="directory the archives go to")
    parser.add_argument("--blocks", nargs="+", type=int, default=[],
                        help="manifest blocks to (re)capture")
    parser.add_argument("--pin", nargs="+", default=[], metavar="N:HASH",
                        help="blocks not in the manifest yet, pinned by number and hash")
    args = parser.parse_args()

    url = os.environ.get("RPC_URL")
    if not url:
        parser.error("set RPC_URL to a mainnet archive endpoint")
    if int(rpc(url, "eth_chainId", []), 16) != MAINNET_CHAIN_ID:
        parser.error(f"RPC_URL is not MegaETH mainnet (chain {MAINNET_CHAIN_ID})")

    manifest = json.loads(Path(args.manifest).read_text())
    known = {entry["number"]: entry for entry in manifest["blocks"]}
    selected = []
    for number in args.blocks:
        if number not in known:
            parser.error(f"block {number} is not in the manifest; pin it with --pin N:HASH")
        selected.append((number, known[number]["hash"]))
    for pin in args.pin:
        number, _, block_hash = pin.partition(":")
        selected.append((int(number), block_hash.lower()))
    if not selected:
        parser.error("select blocks with --blocks and/or --pin")

    binary = Path(args.bin).resolve()
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    failed = []
    for number, block_hash in selected:
        work = Path(tempfile.mkdtemp(prefix=f"corpus-{number}-"))
        try:
            entry = capture_block(binary, url, out, number, block_hash, work)
        except Exception as error:  # Report every block, then fail once.
            failed.append(number)
            print(f"{number}: FAILED: {error} (reports in {work})", file=sys.stderr, flush=True)
            continue
        previous = known.get(number, {})
        for kept in ("spec", "note"):
            if kept in previous:
                entry[kept] = previous[kept]
        print(f"{number}: PASS; manifest entry:", flush=True)
        print(json.dumps(entry, indent=2), flush=True)
    print(f"captured {len(selected) - len(failed)}/{len(selected)}; failed: {failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
