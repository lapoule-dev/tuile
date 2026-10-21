# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Imagery tiles: what each encoding would weigh, cost and look like.

Samples up to `PER_LEVEL` tiles per zoom level from each archive, then for
each tile measures, against the source JPEG decoded:

- lossless repackings of the source bytes (jpegtran, JPEG XL JPEG
  reconstruction): bit-exact JPEG back, no pixel touched;
- pixel re-encodings (JPEG, WebP, AVIF, JPEG XL ladders);
- GPU block formats (KTX2 Basis ETC1S and UASTC), and their transcode times.

Quality is PSNR and SSIM on luma (BT.601) and SSIMULACRA 2, always against the
source tile of the *same* level. Results are kept per level; nothing here
averages across levels.

    venv/bin/python -I imagery.py WORKDIR OUT.json ARCHIVE.pmtiles...

`WORKDIR` receives per-tile temporaries (scratch, never committed).
"""

from __future__ import annotations

import io
import json
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

import numpy as np
from PIL import Image
from pillow_jxl.JpegXLImagePlugin import Decoder, Encoder
from skimage.metrics import structural_similarity
from ssimulacra2 import compute_ssimulacra2

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive, zxy  # noqa: E402

PER_LEVEL = 6
REPEAT = 5


@dataclass
class Variant:
    name: str
    bytes: int
    enc_ms: float
    dec_ms: float
    psnr_y: float
    ssimulacra2: float
    ssim_y: float = 1.0
    extra: dict[str, float] = field(default_factory=dict)


def best_ms(f: Callable[[], object], repeat: int = REPEAT) -> float:
    t = float("inf")
    for _ in range(repeat):
        t0 = time.perf_counter()
        f()
        t = min(t, time.perf_counter() - t0)
    return t * 1e3


def luma(a: np.ndarray) -> np.ndarray:
    a = a.astype(np.float64)
    return 0.299 * a[..., 0] + 0.587 * a[..., 1] + 0.114 * a[..., 2]


def psnr_y(ref: np.ndarray, got: np.ndarray) -> float:
    mse = float(np.mean((luma(ref) - luma(got)) ** 2))
    return 99.0 if mse == 0 else 10 * np.log10(255.0**2 / mse)


def ssimulacra2(ref_png: Path, got: np.ndarray, work: Path) -> float:
    """SSIMULACRA 2 through the `ssimulacra2` Python port (the libjxl CLI
    would do as well; its scores can differ from the port's by a point)."""
    p = work / "cmp.png"
    Image.fromarray(got).save(p)
    return float(compute_ssimulacra2(str(ref_png), str(p)))


def ssim_y(ref: np.ndarray, got: np.ndarray) -> float:
    return float(structural_similarity(luma(ref), luma(got), data_range=255.0))


def pil_variant(
    name: str, src: Image.Image, ref: np.ndarray, ref_png: Path, work: Path, **kw
) -> Variant:
    buf = io.BytesIO()
    t0 = time.perf_counter()
    src.save(buf, **kw)
    enc = (time.perf_counter() - t0) * 1e3
    data = buf.getvalue()

    def dec() -> np.ndarray:
        return np.asarray(Image.open(io.BytesIO(data)).convert("RGB"))

    got = dec()
    return Variant(
        name, len(data), enc, best_ms(dec), psnr_y(ref, got), ssimulacra2(ref_png, got, work), ssim_y(ref, got)
    )


def basis_variant(name: str, args: list[str], ref: np.ndarray, ref_png: Path, work: Path) -> Variant:
    out = work / f"{name}.ktx2"
    t0 = time.perf_counter()
    subprocess.run(
        ["basisu", *args, "-file", str(ref_png), "-output_file", str(out)],
        capture_output=True,
        check=True,
    )
    enc = (time.perf_counter() - t0) * 1e3
    up = work / f"unpack-{name}"
    up.mkdir(exist_ok=True)
    log = subprocess.run(
        ["basisu", "-unpack", "-file", str(out), "-output_path", str(up)],
        capture_output=True,
        text=True,
    ).stdout
    times = {
        m.group(1): float(m.group(2))
        for m in re.finditer(r"format (\w+) succeeded in ([\d.]+) ms", log)
    }
    rgba32 = sorted(up.glob("*_rgb_RGBA32_*.png"))
    got = np.asarray(Image.open(rgba32[0]).convert("RGB")) if rgba32 else ref * 0
    shutil.rmtree(up)
    extra = {f"transcode_ms_{k}": v for k, v in times.items() if k in ("BC7_RGBA", "BC1_RGB", "ASTC_LDR_4X4_RGBA", "ETC2_RGBA", "ETC1_RGB")}
    return Variant(
        name,
        out.stat().st_size,
        enc,
        min(extra.values()) if extra else float("nan"),
        psnr_y(ref, got),
        ssimulacra2(ref_png, got, work),
        ssim_y(ref, got),
        extra,
    )


def jpeg_quality_estimate(im: Image.Image) -> float:
    """libjpeg-scale quality from the luma table (IJG formula inverted)."""
    std = [16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99]
    q = getattr(im, "quantization", None) or {}
    if 0 not in q:
        return float("nan")
    t = list(q[0])
    scale = float(np.median([100.0 * a / b for a, b in zip(sorted(t), sorted(std))]))
    return (200 - scale) / 2 if scale <= 100 else 5000 / scale


def study_tile(jpeg: bytes, work: Path) -> dict:
    src_path = work / "src.jpg"
    src_path.write_bytes(jpeg)
    src = Image.open(src_path)
    src.load()
    ref = np.asarray(src.convert("RGB"))
    ref_png = work / "ref.png"
    Image.fromarray(ref).save(ref_png)
    rgb = Image.fromarray(ref)
    variants: list[Variant] = []

    def dec_src() -> np.ndarray:
        return np.asarray(Image.open(io.BytesIO(jpeg)).convert("RGB"))

    variants.append(Variant("source-jpeg", len(jpeg), 0.0, best_ms(dec_src), 99.0, 100.0))

    # Lossless repackings: the JPEG's own coefficients, re-entropy-coded.
    for name, args in (
        ("jpegtran-optimize", ["-optimize", "-copy", "none"]),
        ("jpegtran-progressive", ["-progressive", "-optimize", "-copy", "none"]),
    ):
        t0 = time.perf_counter()
        out = subprocess.run(["jpegtran", *args, str(src_path)], capture_output=True, check=True).stdout
        enc = (time.perf_counter() - t0) * 1e3
        got = np.asarray(Image.open(io.BytesIO(out)).convert("RGB"))
        variants.append(
            Variant(name, len(out), enc, best_ms(lambda: np.asarray(Image.open(io.BytesIO(out)).convert("RGB"))), psnr_y(ref, got), 100.0 if np.array_equal(ref, got) else ssimulacra2(ref_png, got, work))
        )
    jenc = Encoder(mode="RGB", lossless=False, quality=100, decoding_speed=0, effort=7, use_container=False, use_original_profile=False, num_threads=1)
    t0 = time.perf_counter()
    jxl = jenc(jpeg, src.width, src.height, jpeg_encode=True)
    enc = (time.perf_counter() - t0) * 1e3
    jdec = Decoder(num_threads=1)
    is_jpeg, _info, back, _icc, _boxes = jdec(jxl)
    exact = bool(is_jpeg) and bytes(back) == jpeg
    variants.append(
        Variant(
            "jxl-jpeg-reconstruction",
            len(jxl),
            enc,
            best_ms(lambda: jdec(jxl)),  # to the JPEG bytes; add the JPEG decode
            99.0,
            100.0,
            1.0,
            {"bit_exact_jpeg": float(exact)},
        )
    )

    for q in (60, 75, 85, 92):
        variants.append(pil_variant(f"jpeg-q{q}", rgb, ref, ref_png, work, format="JPEG", quality=q, optimize=True))
    for q in (60, 75, 90):
        variants.append(pil_variant(f"webp-q{q}", rgb, ref, ref_png, work, format="WEBP", quality=q, method=4))
    variants.append(pil_variant("webp-lossless", rgb, ref, ref_png, work, format="WEBP", lossless=True, method=4))
    for q in (40, 60, 75):
        variants.append(pil_variant(f"avif-q{q}", rgb, ref, ref_png, work, format="AVIF", quality=q, speed=6))
    for q in (75, 90):
        variants.append(pil_variant(f"jxl-q{q}", rgb, ref, ref_png, work, format="JXL", quality=q, effort=7, lossless_jpeg=False))
    variants.append(basis_variant("ktx2-etc1s-q128", ["-q", "128"], ref, ref_png, work))
    variants.append(basis_variant("ktx2-etc1s-q255", ["-q", "255"], ref, ref_png, work))
    variants.append(basis_variant("ktx2-uastc-rdo1", ["-uastc", "-uastc_rdo_l", "1.0"], ref, ref_png, work))
    variants.append(basis_variant("ktx2-uastc-rdo3", ["-uastc", "-uastc_rdo_l", "3.0"], ref, ref_png, work))
    return {
        "source_bytes": len(jpeg),
        "size": list(src.size),
        "subsampling": str(getattr(src, "layer", "")),
        "quality_estimate": jpeg_quality_estimate(src),
        "progressive": bool(src.info.get("progressive") or src.info.get("progression")),
        "variants": [v.__dict__ for v in variants],
    }


def main() -> None:
    work = Path(sys.argv[1])
    out = Path(sys.argv[2])
    work.mkdir(parents=True, exist_ok=True)
    rows = []
    for p in sys.argv[3:]:
        a = Archive.open(Path(p))
        entries = a.tile_entries()
        by_level: dict[int, list] = {}
        for e in entries:
            by_level.setdefault(zxy(e.tile_id)[0], []).append(e)
        with Path(p).open("rb") as f:
            for z, es in sorted(by_level.items()):
                step = max(1, len(es) // PER_LEVEL)
                for e in es[::step][:PER_LEVEL]:
                    f.seek(a.header.data_offset + e.offset)
                    jpeg = f.read(e.length)
                    r = study_tile(jpeg, work)
                    r.update({"archive": Path(p).name, "zxy": list(zxy(e.tile_id))})
                    rows.append(r)
                print(f"{Path(p).name} z{z}: {min(PER_LEVEL, len(es))} tiles", file=sys.stderr)
                out.write_text(json.dumps(rows))
    out.write_text(json.dumps(rows))


if __name__ == "__main__":
    main()
