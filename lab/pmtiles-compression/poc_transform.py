# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""PoC: rewrite a store archive with compressed tiles, and measure it.

- `ladder` samples up to N tiles per level of an imagery archive and scores
  WebP qualities (and lossless) against the source JPEG's decode, per level;
- `webp` rewrites an imagery archive with every tile as WebP at one quality;
- `gzip` rewrites a terrain archive with every tile gzip-9 (and zopfli on a
  sample), checking each tile inflates back to the original bytes.

Archives are written with the `pmtiles` package's writer (the reference
Python implementation): same tile ids, its own dedup and run-lengths,
`tile_type` and `tile_compression` set, gzip directories, and read back
whole afterwards.

    python -I poc_transform.py ladder ARCHIVE OUT.json [PER_LEVEL]
    python -I poc_transform.py webp ARCHIVE OUT.pmtiles QUALITY STATS.json
    python -I poc_transform.py gzip ARCHIVE OUT.pmtiles STATS.json
    python -I poc_transform.py quality ARCHIVE WEBP.pmtiles OUT.json STRIDE

Re-encoding commercial imagery makes a derived work: a local measurement,
never a layer to deploy without a licence decision.
"""

from __future__ import annotations

import gzip
import io
import json
import statistics as st
import sys
import time
from collections import defaultdict
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import numpy as np
from PIL import Image
from pmtiles.tile import Compression, TileType
from pmtiles.writer import Writer

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive, zxy  # noqa: E402

WORKERS = 4  # bounded: a small laptop, other jobs running


def webp_bytes(jpeg: bytes, quality: int | None) -> bytes:
    im = Image.open(io.BytesIO(jpeg)).convert("RGB")
    buf = io.BytesIO()
    if quality is None:
        im.save(buf, format="WEBP", lossless=True, method=4)
    else:
        im.save(buf, format="WEBP", quality=quality, method=4)
    return buf.getvalue()


def _webp_job(args: tuple[int, bytes, int]) -> tuple[int, bytes, float]:
    tid, jpeg, q = args
    t0 = time.perf_counter()
    out = webp_bytes(jpeg, q)
    return tid, out, time.perf_counter() - t0


def header_of(a: Archive, tile_type: TileType, compression: Compression) -> dict:
    h = a.header
    return {
        "tile_type": tile_type,
        "tile_compression": compression,
        "min_zoom": h.min_zoom,
        "max_zoom": h.max_zoom,
        "min_lon_e7": -1800000000,
        "min_lat_e7": -850511287,
        "max_lon_e7": 1800000000,
        "max_lat_e7": 850511287,
        "center_zoom": h.min_zoom,
        "center_lon_e7": 0,
        "center_lat_e7": 0,
    }


def metadata_of(path: Path) -> dict:
    raw = path.read_bytes()[:16384]
    from pmtstudy.pmtiles_v3 import decompress, parse_header

    h = parse_header(raw)
    with path.open("rb") as f:
        f.seek(h.metadata_offset)
        meta = f.read(h.metadata_length)
    return json.loads(decompress(meta, h.internal_compression))


def ladder(archive: Path, out: Path, per_level: int) -> None:
    from imagery import psnr_y, ssim_y, ssimulacra2  # the study's metrics

    work = out.parent / "ladder-work"
    work.mkdir(exist_ok=True)
    a = Archive.open(archive)
    by_level: dict[int, list] = defaultdict(list)
    for e in a.tile_entries():
        by_level[zxy(e.tile_id)[0]].append(e)
    rows = []
    with archive.open("rb") as f:
        for z, es in sorted(by_level.items()):
            step = max(1, len(es) // per_level)
            for e in es[::step][:per_level]:
                f.seek(a.header.data_offset + e.offset)
                jpeg = f.read(e.length)
                ref = np.asarray(Image.open(io.BytesIO(jpeg)).convert("RGB"))
                ref_png = work / "ref.png"
                Image.fromarray(ref).save(ref_png)
                row = {"level": z, "tile_id": e.tile_id, "jpeg": len(jpeg), "v": {}}
                for q in (70, 75, 80, 85, 90, 95, None):
                    w = webp_bytes(jpeg, q)
                    got = np.asarray(Image.open(io.BytesIO(w)).convert("RGB"))
                    row["v"][str(q or "lossless")] = {
                        "bytes": len(w),
                        "psnr": psnr_y(ref, got),
                        "ssim": ssim_y(ref, got),
                        "s2": 100.0 if q is None else ssimulacra2(ref_png, got, work),
                    }
                rows.append(row)
            print(f"level {z}: {min(per_level, len(es))} tiles of {len(es)}", file=sys.stderr)
    out.write_text(json.dumps(rows))
    levels = sorted({r["level"] for r in rows})
    print(f"{'q':>9} " + " ".join(f"{'z' + str(z):>15}" for z in levels))
    for q in ("70", "75", "80", "85", "90", "95", "lossless"):
        cells = []
        for z in levels:
            rs = [r for r in rows if r["level"] == z]
            ratio = st.median(r["v"][q]["bytes"] / r["jpeg"] for r in rs)
            s2 = st.median(r["v"][q]["s2"] for r in rs)
            cells.append(f"{ratio:6.2f}/{s2:5.1f}")
        print(f"{q:>9} " + " ".join(f"{c:>15}" for c in cells))


def per_level_sizes(a: Archive) -> dict[int, list[int]]:
    out: dict[int, list[int]] = defaultdict(lambda: [0, 0])
    for e in a.tile_entries():
        z = zxy(e.tile_id)[0]
        out[z][0] += 1
        out[z][1] += e.length
    return out


def read_back(path: Path) -> Archive:
    a = Archive.open(path)
    n = sum(1 for _ in a.tiles())
    assert n == a.header.tile_entries, (n, a.header.tile_entries)
    return a


def transform(archive: Path, out: Path, stats: Path, mode: str, quality: int | None) -> None:
    src = Archive.open(archive)
    meta = metadata_of(archive)
    t_all = time.perf_counter()
    enc_s = 0.0
    with out.open("wb") as fo:
        w = Writer(fo)
        if mode == "webp":
            # In batches: `map` submits its whole input at once, and the
            # input is the archive — memory stays at one batch.
            batch: list[tuple[int, bytes, int | None]] = []
            with ProcessPoolExecutor(WORKERS) as pool:
                def drain() -> None:
                    nonlocal enc_s
                    for tid, data, dt in pool.map(_webp_job, batch, chunksize=32):
                        enc_s += dt
                        w.write_tile(tid, data)
                    batch.clear()
                for tid, b in src.tiles():
                    batch.append((tid, b, quality))
                    if len(batch) >= 2048:
                        drain()
                drain()
            header = header_of(src, TileType.WEBP, Compression.NONE)
        else:
            for tid, b in src.tiles():
                t0 = time.perf_counter()
                data = gzip.compress(b, 9, mtime=0)
                enc_s += time.perf_counter() - t0
                w.write_tile(tid, data)
            header = header_of(src, TileType.UNKNOWN, Compression.GZIP)
        meta["tuile:poc"] = f"{mode}{'' if quality is None else '-q' + str(quality)}"
        w.finalize(header, meta)
    wall = time.perf_counter() - t_all

    dst = read_back(out)
    # Same ids, one for one, and the content checks.
    src_ids = [e.tile_id for e in src.tile_entries()]
    dst_ids = [e.tile_id for e in dst.tile_entries()]
    assert src_ids == dst_ids, "tile ids differ"
    dec_times: list[float] = []
    checked = 0
    for (tid, b), (tid2, c) in zip(src.tiles(), dst.tiles()):
        assert tid == tid2
        t0 = time.perf_counter()
        if mode == "gzip":
            back = gzip.decompress(c)
            dec_times.append(time.perf_counter() - t0)
            assert back == b, f"tile {tid} differs after gunzip"
        else:
            im = Image.open(io.BytesIO(c))
            im.load()
            dec_times.append(time.perf_counter() - t0)
            assert im.size == Image.open(io.BytesIO(b)).size
        checked += 1
    src_jpeg_dec = []
    if mode == "webp":
        for i, (_, b) in enumerate(src.tiles()):
            if i >= 2000:
                break
            t0 = time.perf_counter()
            Image.open(io.BytesIO(b)).load()
            src_jpeg_dec.append(time.perf_counter() - t0)
    before, after = per_level_sizes(src), per_level_sizes(dst)
    h, d = src.header, dst.header
    s = {
        "mode": mode, "quality": quality,
        "src_file": archive.stat().st_size, "dst_file": out.stat().st_size,
        "src_data": h.data_length, "dst_data": d.data_length,
        "tiles": h.tile_entries, "dst_tiles": d.tile_entries,
        "dst_contents": d.tile_contents, "dst_clustered": d.clustered,
        "dst_tile_type": d.tile_type, "dst_tile_compression": d.tile_compression,
        "src_dirs": h.root_length + h.leaf_length, "dst_dirs": d.root_length + d.leaf_length,
        "src_leaves": len(src.leaves), "dst_leaves": len(dst.leaves),
        "encode_cpu_s": enc_s, "wall_s": wall,
        "decode_ms_median": st.median(dec_times) * 1e3,
        "decode_ms_p95": sorted(dec_times)[int(0.95 * len(dec_times))] * 1e3,
        "src_jpeg_decode_ms_median": st.median(src_jpeg_dec) * 1e3 if src_jpeg_dec else None,
        "checked": checked,
        "levels": {z: {"tiles": before[z][0], "before": before[z][1], "after": after[z][1]} for z in sorted(before)},
    }
    if mode == "gzip":
        import zopfli.gzip

        sample = [b for i, (_, b) in enumerate(src.tiles()) if i % max(1, h.tile_entries // 200) == 0][:200]
        t0 = time.perf_counter()
        zp = sum(len(zopfli.gzip.compress(b, numiterations=5)) for b in sample)
        s["zopfli_sample"] = {
            "tiles": len(sample), "raw": sum(map(len, sample)),
            "gzip9": sum(len(gzip.compress(b, 9, mtime=0)) for b in sample),
            "zopfli5": zp, "ms_per_tile": (time.perf_counter() - t0) / len(sample) * 1e3,
        }
    stats.write_text(json.dumps(s, indent=1))
    print(json.dumps({k: v for k, v in s.items() if k != "levels"}, indent=1))
    for z, v in s["levels"].items():
        print(f"  z{z}: {v['tiles']:7} tiles  {v['before'] / 1e6:9.2f} MB -> {v['after'] / 1e6:9.2f} MB  ({v['after'] / max(1, v['before']):.3f})")


def quality(src_path: Path, webp_path: Path, out: Path, stride: int) -> None:
    """Every `stride`-th tile of a WebP rewrite against its source JPEG,
    per level: PSNR-Y, SSIM-Y, SSIMULACRA 2."""
    from imagery import psnr_y, ssim_y, ssimulacra2

    work = out.parent / "quality-work"
    work.mkdir(exist_ok=True)
    src, dst = Archive.open(src_path), Archive.open(webp_path)
    rows = []
    for i, ((tid, j), (tid2, w)) in enumerate(zip(src.tiles(), dst.tiles())):
        assert tid == tid2
        if i % stride:
            continue
        ref = np.asarray(Image.open(io.BytesIO(j)).convert("RGB"))
        got = np.asarray(Image.open(io.BytesIO(w)).convert("RGB"))
        ref_png = work / "ref.png"
        Image.fromarray(ref).save(ref_png)
        rows.append({"level": zxy(tid)[0], "tile_id": tid, "ratio": len(w) / len(j),
                     "psnr": psnr_y(ref, got), "ssim": ssim_y(ref, got), "s2": ssimulacra2(ref_png, got, work)})
    out.write_text(json.dumps(rows))
    for z in sorted({r["level"] for r in rows}):
        rs = [r for r in rows if r["level"] == z]
        s2 = sorted(r["s2"] for r in rs)
        q = lambda p: s2[min(len(s2) - 1, int(p * len(s2)))]
        print(f"z{z}: n={len(rs):4} size/src median {st.median(r['ratio'] for r in rs):.3f}  "
              f"PSNR-Y {st.median(r['psnr'] for r in rs):5.1f}  SSIM-Y {st.median(r['ssim'] for r in rs):.4f}  "
              f"SSIMU2 p5 {q(0.05):5.1f} p25 {q(0.25):5.1f} median {q(0.5):5.1f} p75 {q(0.75):5.1f}")


def main() -> None:
    cmd = sys.argv[1]
    if cmd == "ladder":
        ladder(Path(sys.argv[2]), Path(sys.argv[3]), int(sys.argv[4]) if len(sys.argv) > 4 else 12)
    elif cmd == "webp":
        transform(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[5]), "webp", int(sys.argv[4]))
    elif cmd == "quality":
        quality(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]), int(sys.argv[5]))
    elif cmd == "gzip":
        transform(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]), "gzip", None)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
