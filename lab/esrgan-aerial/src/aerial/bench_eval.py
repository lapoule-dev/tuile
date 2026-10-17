# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The judge on the benchmark blocks: every method, every truth tile, and the 4 x 4 mosaics for seams.

The input of a truth tile is the **source's own** tile two levels up (its real pyramid: same acquisition, same
processing, its own JPEG), with ``HALO`` input pixels of real neighbours. Each method runs per tile twice: with the
halo (how a tile server would run it) and ``isolated`` (no neighbours: the seams a naive server would show).
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pandas as pd
from PIL import Image

from .bench import BLOCKS, RING, SIDE, Block
from .judge import Perceptual, combine, measure
from .imaging import TILE, Rgb, seam_score
from .methods import Method

HALO = 16


def _tile(root: Path, b: Block, z: int, x: int, y: int) -> Rgb:
    return np.asarray(Image.open(root / b.name / str(z) / str(x) / f"{y}.jpg").convert("RGB"), np.float32) / 255.0


def mosaic(root: Path, b: Block, z: int) -> tuple[Rgb, int, int]:
    """Every tile of zoom ``z`` over the block and its ring, stitched; with the global pixel of its top-left."""
    tiles = b.tiles(z)
    xs, ys = sorted({t[1] for t in tiles}), sorted({t[2] for t in tiles})
    out = np.zeros((len(ys) * TILE, len(xs) * TILE, 3), np.float32)
    for _, x, y in tiles:
        p = root / b.name / str(z) / str(x) / f"{y}.jpg"
        if p.exists():
            out[(y - ys[0]) * TILE:(y - ys[0] + 1) * TILE, (x - xs[0]) * TILE:(x - xs[0] + 1) * TILE] = _tile(
                root, b, z, x, y)
    return out, xs[0] * TILE, ys[0] * TILE


def block_pairs(root: Path, b: Block) -> list[dict[str, object]]:
    """For each of the 16 truth tiles: truth (256), input with halo (64 + 2 HALO), input alone (64)."""
    z = b.zoom
    lr, gx, gy = mosaic(root, b, z - 2)
    x0, y0 = b.origin()
    out = []
    for j in range(SIDE):
        for i in range(SIDE):
            x, y = x0 + i, y0 + j
            if not (root / b.name / str(z) / str(x) / f"{y}.jpg").exists():
                continue  # a tile the service does not have: the pair is left out, never filled
            truth = _tile(root, b, z, x, y)
            px, py = x * 64 - gx, y * 64 - gy
            given = lr[py:py + 64, px:px + 64]
            haloed = lr[py - HALO:py + 64 + HALO, px - HALO:px + 64 + HALO]
            if (haloed.max(axis=-1) == 0).mean() > 0.01:
                continue  # part of the input's neighbourhood is missing
            out.append({"block": b.name, "i": i, "j": j, "truth": truth, "given": given, "haloed": haloed})
    return out


def run(root: Path, methods: dict[str, Method], perc: Perceptual, blocks: tuple[Block, ...] = BLOCKS,
        keep: Path | None = None) -> tuple[pd.DataFrame, pd.DataFrame]:
    """(per-tile scores, per-block seam scores). Outputs are written under ``keep`` when given (PNG)."""
    rows, seams = [], []
    for b in blocks:
        pairs = block_pairs(root, b)
        truths = [p["truth"] for p in pairs]
        givens = [p["given"] for p in pairs]
        outs: dict[str, list[Rgb]] = {"truth": truths}
        for name, m in methods.items():
            outs[name] = [m(p["haloed"], HALO) for p in pairs]  # type: ignore[arg-type]
            iso = [m(p["given"], 0) for p in pairs]  # type: ignore[arg-type]
            for mode, imgs in (("halo", outs[name]), ("isolated", iso)):
                mos = np.full((SIDE * TILE, SIDE * TILE, 3), np.nan, np.float32)
                for p, img in zip(pairs, imgs):
                    mos[int(p["j"]) * TILE:(int(p["j"]) + 1) * TILE, int(p["i"]) * TILE:(int(p["i"]) + 1) * TILE] = img  # type: ignore[call-overload]
                seams.append({"block": b.name, "method": name, "mode": mode, "seam": seam_score(mos, TILE)})
                if keep is not None:
                    (keep / b.name).mkdir(parents=True, exist_ok=True)
                    Image.fromarray((mos * 255 + 0.5).astype(np.uint8)).save(keep / b.name / f"{name}-{mode}.png")
        tmos = np.zeros((SIDE * TILE, SIDE * TILE, 3), np.float32)
        for p in pairs:
            tmos[int(p["j"]) * TILE:(int(p["j"]) + 1) * TILE, int(p["i"]) * TILE:(int(p["i"]) + 1) * TILE] = p["truth"]  # type: ignore[call-overload]
        seams.append({"block": b.name, "method": "truth", "mode": "halo", "seam": seam_score(tmos, TILE)})
        if keep is not None:
            Image.fromarray((tmos * 255 + 0.5).astype(np.uint8)).save(keep / b.name / "truth.png")
        for name, imgs in outs.items():
            t = measure(imgs, truths, givens, perc)
            t["block"], t["stratum_intent"], t["source"], t["method"] = b.name, b.stratum, b.source, name
            t["sample"] = [f"{b.name}/{p['i']}/{p['j']}" for p in pairs]
            rows.append(t)
    return combine(pd.concat(rows, ignore_index=True)), pd.DataFrame(seams)


RING_USED = RING
