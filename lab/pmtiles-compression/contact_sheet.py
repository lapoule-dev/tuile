# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A contact sheet: the same tiles as the source JPEG and as WebP rewrites,
side by side, plus a crop at 3x and a difference amplified 8x.

Each row is one kind of ground, at one level (never across levels). An
archive covers what it covers, so the rows are not chosen by name but by
pixel statistics among the tiles present at that level: the most
coast-like (water and land), forest-like (dark green), mountain-like (relief
and bright rock or snow) and most textured (grey, busy) tile. Each row
says where its tile is. A village needs a level where it is a few pixels
wide: at levels 5-9 (about 300 m a pixel at 9) none is.

    python -I contact_sheet.py SRC.pmtiles OUT.png LEVEL WEBP_A.pmtiles[:label] [WEBP_B...]
"""

from __future__ import annotations

import io
import math
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive, zxy  # noqa: E402

ROWS = 4


def lat_lon(z: int, x: int, y: int) -> tuple[float, float]:
    n = 2**z
    lon = (x + 0.5) / n * 360 - 180
    lat = math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * (y + 0.5) / n))))
    return lat, lon


def scores(rgb: np.ndarray) -> dict[str, float]:
    a = rgb.astype(np.float64) / 255
    r, g, b = a[..., 0], a[..., 1], a[..., 2]
    lum = 0.299 * r + 0.587 * g + 0.114 * b
    water = ((b > r + 0.02) & (b >= g) & (lum < 0.35)).mean()
    green = ((g > r + 0.02) & (g > b) & (lum < 0.35)).mean()
    sat = a.max(axis=2) - a.min(axis=2)
    grey = ((sat < 0.06) & (lum > 0.3) & (lum < 0.75)).mean()
    edges = np.abs(np.diff(lum, axis=0)).mean() + np.abs(np.diff(lum, axis=1)).mean()
    bright = (lum > 0.7).mean()
    return {
        "coast": min(water, 1 - water) * (edges > 0.02),
        "forest": green - water,
        "mountain": lum.std() + bright * 0.5 - water,
        "textured": grey + edges * 2 - water - bright * 2,
    }


def tiles_by_zxy(path: Path) -> dict[tuple[int, int, int], bytes]:
    return {zxy(t): b for t, b in Archive.open(path).tiles()}


def main() -> None:
    src_path, out, level = Path(sys.argv[1]), Path(sys.argv[2]), int(sys.argv[3])
    variants = []
    for arg in sys.argv[4:]:
        p, _, label = arg.partition(":")
        variants.append((label or Path(p).stem, tiles_by_zxy(Path(p))))
    src = tiles_by_zxy(Path(src_path))
    present = [k for k in src if k[0] == level]
    cols = 1 + len(variants)
    cell, crop, pad, head = 256, 384, 8, 22
    width = pad + cols * (cell + pad) + (crop + pad) * (1 + len(variants)) + (cell + pad)
    sheet = Image.new("RGB", (width, head + ROWS * (max(cell, crop) + head + pad)), "white")
    d = ImageDraw.Draw(sheet)
    y0 = head
    labels = ["source JPEG"] + [v[0] for v in variants]
    stats = {k: scores(np.asarray(Image.open(io.BytesIO(src[k])).convert("RGB"))) for k in present}
    chosen: list[tuple[str, tuple[int, int, int]]] = []
    for kind in ("mountain", "coast", "forest", "textured"):
        best = max((k for k in present if k not in [c[1] for c in chosen]), key=lambda k: stats[k][kind])
        chosen.append((kind, best))
    for kind, key in chosen:
        lat, lon = lat_lon(*key)
        name = f"{kind}-like (by pixel statistics), near {lat:.2f}, {lon:.2f}"
        imgs = [Image.open(io.BytesIO(src[key])).convert("RGB")]
        sizes = [len(src[key])]
        for _, t in variants:
            imgs.append(Image.open(io.BytesIO(t[key])).convert("RGB"))
            sizes.append(len(t[key]))
        d.text((pad, y0 - 16), f"{name}  z{key[0]}/{key[1]}/{key[2]}", fill="black")
        xo = pad
        for im, lab, sz in zip(imgs, labels, sizes):
            sheet.paste(im, (xo, y0))
            d.text((xo, y0 + cell + 2), f"{lab}  {sz / 1024:.1f} KiB", fill="black")
            xo += cell + pad
        # The same 128 px crop at 3x, nearest: where artefacts show.
        box = (64, 64, 192, 192)
        for im in imgs:
            sheet.paste(im.crop(box).resize((crop, crop), Image.NEAREST), (xo, y0))
            xo += crop + pad
        diff = np.abs(np.asarray(imgs[0], dtype=np.int16) - np.asarray(imgs[1], dtype=np.int16)) * 8
        sheet.paste(Image.fromarray(np.clip(diff, 0, 255).astype(np.uint8)), (xo, y0))
        d.text((xo, y0 + cell + 2), f"|src - {labels[1]}| x8", fill="black")
        y0 += max(cell, crop) + head + pad
    sheet.save(out)
    print(out)


if __name__ == "__main__":
    main()
