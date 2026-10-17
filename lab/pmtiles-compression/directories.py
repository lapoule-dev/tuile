# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Directories, dedup and run-lengths of PMTiles archives, and what each
internal compression would make of the directories.

    python -I directories.py OUT.json ARCHIVE.pmtiles...
"""

from __future__ import annotations

import gzip
import hashlib
import json
import sys
import time
from collections import Counter
from pathlib import Path

import brotli
import zstandard

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive, serialize_directory, zxy  # noqa: E402

CODECS = {
    "none": (lambda b: b, lambda b: b),
    "gzip-6": (lambda b: gzip.compress(b, 6, mtime=0), gzip.decompress),
    "gzip-9": (lambda b: gzip.compress(b, 9, mtime=0), gzip.decompress),
    "zstd-19": (
        lambda b: zstandard.ZstdCompressor(level=19).compress(b),
        lambda b: zstandard.ZstdDecompressor().decompress(b),
    ),
    "brotli-11": (lambda b: brotli.compress(b, quality=11), brotli.decompress),
}


def study(path: Path) -> dict:
    a = Archive.open(path)
    h = a.header
    entries = a.tile_entries()
    digests: Counter[str] = Counter()
    by_zoom: Counter[int] = Counter()
    sizes = []
    for tid, b in a.tiles():
        digests[hashlib.blake2b(b, digest_size=16).hexdigest()] += 1
        by_zoom[zxy(tid)[0]] += 1
        sizes.append(len(b))
    dirs = [a.root] + a.leaves
    plain = [serialize_directory(d) for d in dirs]
    internal = {}
    for name, (enc, dec) in CODECS.items():
        packed = [enc(p) for p in plain]
        t0 = time.perf_counter()
        for p in packed:
            dec(p)
        dt = time.perf_counter() - t0
        internal[name] = {
            "root_bytes": len(packed[0]),
            "leaves_bytes": sum(len(p) for p in packed[1:]),
            "decode_ms_all": dt * 1e3,
        }
    return {
        "archive": path.name,
        "file_bytes": path.stat().st_size,
        "tile_type": h.tile_type,
        "tile_compression": h.tile_compression,
        "internal_compression": h.internal_compression,
        "clustered": h.clustered,
        "zooms": [h.min_zoom, h.max_zoom],
        "addressed_tiles": h.addressed_tiles,
        "tile_entries": h.tile_entries,
        "tile_contents": h.tile_contents,
        "entries_with_run_gt_1": sum(1 for e in entries if e.run_length > 1),
        "distinct_payloads_by_hash": len(digests),
        "duplicate_entries": sum(c - 1 for c in digests.values()),
        "root_entries": len(a.root),
        "leaf_dirs": len(a.leaves),
        "root_stored_bytes": len(a.root_bytes),
        "leaves_stored_bytes": sum(len(b) for b in a.leaf_bytes),
        "metadata_bytes": h.metadata_length,
        "data_bytes": h.data_length,
        "tiles_by_archive_zoom": dict(sorted(by_zoom.items())),
        "mean_tile_bytes": sum(sizes) / max(1, len(sizes)),
        "first_request_bytes_needed": h.root_offset + h.root_length,
        "internal": internal,
    }


def main() -> None:
    out = Path(sys.argv[1])
    rows = [study(Path(p)) for p in sys.argv[2:]]
    out.write_text(json.dumps(rows, indent=1))
    for r in rows:
        print(
            f"{r['archive'][:40]:40} {r['tile_type']:7} tc={r['tile_compression']:5} "
            f"ic={r['internal_compression']:5} n={r['tile_entries']:6} "
            f"distinct={r['distinct_payloads_by_hash']:6} rl>1={r['entries_with_run_gt_1']:4} "
            f"leaves={r['leaf_dirs']:3} root={r['root_stored_bytes']:6} "
            f"leafB={r['leaves_stored_bytes']:7} mean={r['mean_tile_bytes']:8.0f}"
        )
        print(
            "   internal:",
            {k: (v["root_bytes"], v["leaves_bytes"]) for k, v in r["internal"].items()},
        )


if __name__ == "__main__":
    main()
