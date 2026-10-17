# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Compressing a whole container (a PMTiles archive, a film pack) as a file.

Two families:

- **whole-file** codecs (gzip, zstd, zstd long-range, brotli, xz): ratio and
  encode/decode MB/s. Random access does not survive them;
- **seekable framings**: the file cut into independently compressed frames of
  a fixed *uncompressed* size, as the zstd seekable format and BGZF do, with
  a seek table of one entry per frame. For each frame size: the ratio
  (index included), and what reading a `READ`-byte span at a random offset
  costs: compressed bytes fetched and CPU to decode the frames it touches.

Every input is also measured as the bytes *after* tile-level compression,
when the caller passes one (`--gzip-tiles` for a PMTiles terrain archive:
each tile gzip-9 in place, directories as they are).

    python -I container.py OUT.json [--gzip-tiles] FILE...
"""

from __future__ import annotations

import gzip
import json
import lzma
import sys
import time
from pathlib import Path
from typing import Callable

import brotli
import zstandard as zstd

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive  # noqa: E402

READ = 30 * 1024
FRAMES = (64 << 10, 256 << 10, 1 << 20, 8 << 20)
#: zstd seekable format: 8 bytes per frame (compressed and decompressed
#: sizes), 4 more with checksums, and a 9-byte footer; BGZF needs none
#: beyond the members' own headers, but readers build a `.gzi` of 16 B/frame.
SEEK_ENTRY = 12
SEEK_FOOTER = 9


def gzip_tiles(path: Path) -> bytes:
    """The archive's bytes with every tile replaced by its gzip-9: what an
    archive written `tile_compression: gzip` would weigh, near enough."""
    a = Archive.open(path)
    raw = path.read_bytes()
    head = raw[: a.header.data_offset]
    body = b"".join(gzip.compress(b, 9, mtime=0) for _, b in a.tiles())
    tail = raw[a.header.data_offset + a.header.data_length :]
    return head + body + tail


def timed(f: Callable[[], bytes]) -> tuple[bytes, float]:
    t0 = time.perf_counter()
    out = f()
    return out, time.perf_counter() - t0


def whole(data: bytes) -> dict[str, dict[str, float]]:
    mb = len(data) / 1e6
    ldm = zstd.ZstdCompressionParameters.from_level(19, window_log=27, enable_ldm=True)
    codecs: dict[str, tuple[Callable[[bytes], bytes], Callable[[bytes], bytes]]] = {
        "gzip-6": (lambda b: gzip.compress(b, 6, mtime=0), gzip.decompress),
        "zstd-3": (zstd.ZstdCompressor(level=3).compress, zstd.ZstdDecompressor().decompress),
        "zstd-19": (zstd.ZstdCompressor(level=19).compress, zstd.ZstdDecompressor().decompress),
        "zstd-19-long27": (
            zstd.ZstdCompressor(compression_params=ldm).compress,
            lambda b: zstd.ZstdDecompressor(max_window_size=1 << 27).decompress(b),
        ),
        "brotli-9": (lambda b: brotli.compress(b, quality=9), brotli.decompress),
        "xz-6": (lambda b: lzma.compress(b, preset=6), lzma.decompress),
    }
    out = {}
    for name, (enc, dec) in codecs.items():
        packed, te = timed(lambda: enc(data))
        back, td = timed(lambda: dec(packed))
        assert back == data
        out[name] = {"ratio": len(packed) / len(data), "enc_mb_s": mb / te, "dec_mb_s": mb / td}
        print(f"   whole {name:16} {len(packed) / len(data):6.3f}  enc {mb / te:7.1f} MB/s  dec {mb / td:7.0f} MB/s", file=sys.stderr)
    return out


def framed(data: bytes) -> dict[str, dict[str, float]]:
    out = {}
    for frame in FRAMES:
        chunks = [data[i : i + frame] for i in range(0, len(data), frame)]
        for name, enc, dec in (
            ("zstd-9", zstd.ZstdCompressor(level=9).compress, zstd.ZstdDecompressor().decompress),
            ("zstd-19", zstd.ZstdCompressor(level=19).compress, zstd.ZstdDecompressor().decompress),
            ("bgzf-6", lambda b: gzip.compress(b, 6, mtime=0), gzip.decompress),
        ):
            if name == "bgzf-6" and frame > (64 << 10):
                continue  # BGZF members are at most 64 KiB by definition
            packed = [enc(c) for c in chunks]
            t0 = time.perf_counter()
            for p in packed:
                dec(p)
            dec_s = time.perf_counter() - t0
            index = SEEK_ENTRY * len(chunks) + SEEK_FOOTER
            total = sum(map(len, packed)) + index
            mean_c = sum(map(len, packed)) / len(packed)
            # A READ-byte span at a uniform offset touches (frame + READ - 1) / frame
            # frames on average (capped by the file).
            touched = min(len(chunks), (frame + READ - 1) / frame)
            out[f"{name}@{frame >> 10}KiB"] = {
                "ratio": total / len(data),
                "index_bytes": index,
                "read_fetch_bytes": touched * mean_c,
                "read_amplification": touched * mean_c / READ,
                "read_decode_ms": touched * (dec_s / len(chunks)) * 1e3,
            }
            r = out[f"{name}@{frame >> 10}KiB"]
            print(f"   frame {name:7}@{frame >> 10:5}KiB {r['ratio']:6.3f}  index {index:7} B  "
                  f"30KB read: fetch {r['read_fetch_bytes'] / 1024:8.1f} KiB (x{r['read_amplification']:5.1f}), "
                  f"decode {r['read_decode_ms']:6.2f} ms", file=sys.stderr)
    return out


def main() -> None:
    out = Path(sys.argv[1])
    gz_tiles = "--gzip-tiles" in sys.argv
    files = [Path(a) for a in sys.argv[2:] if not a.startswith("--")]
    rows = []
    for f in files:
        variants = {"as-stored": f.read_bytes()}
        if gz_tiles:
            variants["tiles-gzip9"] = gzip_tiles(f)
        for vname, data in variants.items():
            print(f"{f.name} [{vname}] {len(data) / 1e6:.2f} MB", file=sys.stderr)
            rows.append({"file": f.name, "variant": vname, "bytes": len(data), "whole": whole(data), "framed": framed(data)})
            out.write_text(json.dumps(rows, indent=1))


if __name__ == "__main__":
    main()
