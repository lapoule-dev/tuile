# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A film pack, block by block: what each kind of payload weighs as stored,
and what it would weigh under another per-block codec — so that the pack
keeps its random access (one block = one independent payload).

Mesh blocks are LZ4 today: decoded, then re-encoded with zstd (with and
without a dictionary trained on the pack's own mesh blocks). Textures are
PNG: re-encoded losslessly (Pillow PNG at maximum effort, WebP lossless),
and zstd over the raw RGBA, all checked pixel-exact.

    python -I pack_blocks.py GEN OUT.json PACK.tuilepack...

`GEN` is the directory `flatc --python` generated the schema's accessors in.
"""

from __future__ import annotations

import io
import json
import sys
import time
from collections import defaultdict
from pathlib import Path

import lz4.block
import numpy as np
import zstandard as zstd
from PIL import Image

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pack import open_pack  # noqa: E402


def main() -> None:
    gen, out = Path(sys.argv[1]), Path(sys.argv[2])
    rows = []
    for f in sys.argv[3:]:
        p = open_pack(Path(f), gen)
        raw = Path(f).read_bytes()
        blob = raw[p.blob_start :]
        acc: dict[str, dict[str, float]] = defaultdict(lambda: defaultdict(float))
        meshes: list[bytes] = []
        for b in p.blocks:
            stored = blob[b.offset : b.offset + b.stored]
            a = acc[b.kind]
            a["n"] += 1
            a["stored"] += b.stored
            a["raw"] += b.raw
            if b.kind == "texture":
                im = Image.open(io.BytesIO(stored))
                im.load()
                px = np.asarray(im)
                a["px"] += px.size
                a["side"] = float(im.size[0])
                for name, kw in (("png-opt9", {"format": "PNG", "optimize": True, "compress_level": 9}),
                                 ("webp-lossless", {"format": "WEBP", "lossless": True, "method": 6, "quality": 100})):
                    buf = io.BytesIO()
                    t0 = time.perf_counter()
                    im.save(buf, **kw)
                    a[f"enc_s:{name}"] += time.perf_counter() - t0
                    back = np.asarray(Image.open(io.BytesIO(buf.getvalue())).convert(im.mode))
                    assert np.array_equal(back, px), name
                    a[name] += len(buf.getvalue())
                    t0 = time.perf_counter()
                    np.asarray(Image.open(io.BytesIO(buf.getvalue())))
                    a[f"dec_s:{name}"] += time.perf_counter() - t0
                t0 = time.perf_counter()
                np.asarray(Image.open(io.BytesIO(stored)))
                a["dec_s:png-as-stored"] += time.perf_counter() - t0
                a["zstd19-rgba"] += len(zstd.ZstdCompressor(level=19).compress(px.tobytes()))
            else:
                plain = lz4.block.decompress(stored, uncompressed_size=b.raw) if b.raw else b""
                meshes.append(plain)
                for lvl in (3, 19):
                    c = zstd.ZstdCompressor(level=lvl).compress(plain)
                    a[f"zstd{lvl}"] += len(c)
                t0 = time.perf_counter()
                lz4.block.decompress(stored, uncompressed_size=b.raw) if b.raw else b""
                a["dec_s:lz4"] += time.perf_counter() - t0
                c = zstd.ZstdCompressor(level=19).compress(plain)
                t0 = time.perf_counter()
                zstd.ZstdDecompressor().decompress(c)
                a["dec_s:zstd19"] += time.perf_counter() - t0
        d = zstd.train_dictionary(64 * 1024, [m for m in meshes if m]) if len(meshes) > 8 else None
        if d is not None:
            c19 = zstd.ZstdCompressor(level=19, dict_data=d)
            for b, m in zip([b for b in p.blocks if b.kind != "texture"], meshes):
                acc[b.kind]["zstd19+dict64k"] += len(c19.compress(m))
        table = raw[16 : p.blob_start]
        row = {
            "pack": Path(f).name, "version": p.version, "content": p.content,
            "file_bytes": len(raw), "table_bytes": len(table),
            "table_zstd19": len(zstd.ZstdCompressor(level=19).compress(table)),
            "blob_bytes": len(blob), "kinds": {k: dict(v) for k, v in acc.items()},
        }
        rows.append(row)
        print(json.dumps(row, indent=1))
    out.write_text(json.dumps(rows, indent=1))


if __name__ == "__main__":
    main()
