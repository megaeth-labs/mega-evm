#!/usr/bin/env python3
"""Pack a block cache tree into the deterministic `tests/fixtures/blocks.tar.xz`.

Usage: scripts/pack_blocks_fixture.py <cache-dir> [<archive>]

`<cache-dir>` holds `blocks/` and `codes/`.
Members are regular files only, in sorted path order, in ustar with mode 0644, uid/gid 0, empty
owner names and mtime 0; the tar is compressed with xz preset 9 extreme, single-threaded.
Packing the same files therefore always gives the same bytes.
`--check <archive>` unpacks nothing: it repacks the archive's own members and requires the same bytes.
"""
import io
import lzma
import sys
import tarfile
from pathlib import Path

DEFAULT_ARCHIVE = Path(__file__).resolve().parent.parent / "tests/fixtures/blocks.tar.xz"


def pack(members):
    """members: sorted list of (relative posix path, bytes)."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = 0
            tar.addfile(info, io.BytesIO(data))
    filters = [{"id": lzma.FILTER_LZMA2, "preset": 9 | lzma.PRESET_EXTREME}]
    return lzma.compress(raw.getvalue(), format=lzma.FORMAT_XZ, check=lzma.CHECK_CRC64, filters=filters)


def main():
    args = sys.argv[1:]
    if args and args[0] == "--check":
        archive = Path(args[1] if len(args) > 1 else DEFAULT_ARCHIVE)
        with tarfile.open(archive, "r:xz") as tar:
            members = [(m.name, tar.extractfile(m).read()) for m in tar.getmembers() if m.isfile()]
        if pack(members) != archive.read_bytes():
            sys.exit(f"{archive}: a repack of its members differs")
        print(f"{archive}: repack is byte-identical")
        return
    root = Path(args[0])
    out = Path(args[1]) if len(args) > 1 else DEFAULT_ARCHIVE
    files = sorted(p for p in root.rglob("*") if p.is_file())
    members = [(p.relative_to(root).as_posix(), p.read_bytes()) for p in files]
    out.write_bytes(pack(members))
    print(f"{out}: {len(members)} members, {out.stat().st_size} bytes")


main()
