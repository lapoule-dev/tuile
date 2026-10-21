# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Per-tile compression of quantized-mesh tiles, as they lie in the store.

Each tile is compressed alone (a PMTiles tile is fetched alone, by range), at
several codecs and levels; zstd also with a dictionary trained on the *other*
archives (leave-one-zone-out, so the dictionary never saw the tiles it is
measured on). Times are native, single-threaded, per tile: encode once, decode best of
`REPEAT`.

    python -I terrain.py OUT.json ARCHIVE.pmtiles...
"""

from __future__ import annotations

import gzip
import json
import struct
import sys
import time
from pathlib import Path
from typing import Callable

import brotli
import zstandard

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive, zxy  # noqa: E402

REPEAT = 3
Codec = tuple[Callable[[bytes], bytes], Callable[[bytes], bytes]]


def codecs(dicts: dict[int, zstandard.ZstdCompressionDict]) -> dict[str, Codec]:
    out: dict[str, Codec] = {
        "none": (lambda b: b, lambda b: b),
    }
    for lvl in (1, 6, 9):
        out[f"gzip-{lvl}"] = (
            (lambda lv: lambda b: gzip.compress(b, lv, mtime=0))(lvl),
            gzip.decompress,
        )
    # zopfli is measured apart (zopfli_and_jpeg_gzip.py): it is slow.
    for q in (5, 9, 11):
        out[f"brotli-{q}"] = (
            (lambda qq: lambda b: brotli.compress(b, quality=qq))(q),
            brotli.decompress,
        )
    d = zstandard.ZstdDecompressor()
    for lvl in (3, 9, 19, 22):
        c = zstandard.ZstdCompressor(level=lvl)
        out[f"zstd-{lvl}"] = (c.compress, d.decompress)
    for size, zd in dicts.items():
        c = zstandard.ZstdCompressor(level=19, dict_data=zd)
        dd = zstandard.ZstdDecompressor(dict_data=zd)
        out[f"zstd-19+dict{size // 1024}k"] = (c.compress, dd.decompress)
    return out


def best(f: Callable[[], object]) -> float:
    t = float("inf")
    for _ in range(REPEAT):
        t0 = time.perf_counter()
        f()
        t = min(t, time.perf_counter() - t0)
    return t


def extensions(tile: bytes) -> list[int]:
    """Extension ids after the index data, to say what the tiles carry."""
    try:
        pos = 88
        (nv,) = struct.unpack_from("<I", tile, pos)
        pos += 4 + nv * 6
        wide = nv > 65536
        if wide and pos % 4:
            pos += 4 - pos % 4
        elif not wide and pos % 2:
            pos += 2 - pos % 2
        (nt,) = struct.unpack_from("<I", tile, pos)
        isz = 4 if wide else 2
        pos += 4 + nt * 3 * isz
        for _ in range(4):
            (n,) = struct.unpack_from("<I", tile, pos)
            pos += 4 + n * isz
        ids = []
        while pos + 5 <= len(tile):
            eid = tile[pos]
            (ln,) = struct.unpack_from("<I", tile, pos + 1)
            ids.append(eid)
            pos += 5 + ln
        return ids
    except struct.error:
        return [-1]


def main() -> None:
    out = Path(sys.argv[1])
    paths = [Path(p) for p in sys.argv[2:]]
    zones = {p.name: [b for _, b in Archive.open(p).tiles()] for p in paths}
    levels = {
        p.name: sorted({zxy(t)[0] for t, _ in Archive.open(p).tiles()}) for p in paths
    }
    ext_seen: dict[int, int] = {}
    for tiles in zones.values():
        for t in tiles:
            for e in extensions(t):
                ext_seen[e] = ext_seen.get(e, 0) + 1
    results: dict[str, dict[str, float]] = {}
    for name, tiles in zones.items():
        others = [t for n, ts in zones.items() if n != name for t in ts]
        dicts = {
            size: zstandard.train_dictionary(size, others, level=19)
            for size in (16 * 1024, 64 * 1024, 110 * 1024)
        }
        for cname, (enc, dec) in codecs(dicts).items():
            r = results.setdefault(
                cname, {"raw": 0, "packed": 0, "enc_s": 0.0, "dec_s": 0.0, "n": 0}
            )
            for t in tiles:
                t0 = time.perf_counter()
                p = enc(t)
                r["enc_s"] += time.perf_counter() - t0
                assert dec(p) == t
                r["raw"] += len(t)
                r["packed"] += len(p)
                r["n"] += 1
                r["dec_s"] += best(lambda: dec(p)) if cname != "none" else 0.0
        print(f"zone {name} done ({len(tiles)} tiles)", file=sys.stderr)
    raw_mb = results["none"]["raw"] / 1e6
    summary = {
        "tiles": int(results["none"]["n"]),
        "raw_mb": raw_mb,
        "archive_levels": levels,
        "extensions_seen": ext_seen,
        "codecs": {},
    }
    print(f"{'codec':22} {'ratio':>6} {'MB':>7} {'enc ms/tile':>11} {'dec ms/tile':>11} {'dec MB/s':>9}")
    for cname, r in results.items():
        n = r["n"]
        row = {
            "ratio": r["packed"] / r["raw"],
            "packed_mb": r["packed"] / 1e6,
            "enc_ms_per_tile": r["enc_s"] / n * 1e3,
            "dec_ms_per_tile": r["dec_s"] / n * 1e3,
            "dec_mb_s_of_output": (r["raw"] / 1e6) / r["dec_s"] if r["dec_s"] else 0.0,
        }
        summary["codecs"][cname] = row
        print(
            f"{cname:22} {row['ratio']:6.3f} {row['packed_mb']:7.2f} "
            f"{row['enc_ms_per_tile']:11.3f} {row['dec_ms_per_tile']:11.3f} "
            f"{row['dec_mb_s_of_output']:9.0f}"
        )
    out.write_text(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
