# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Controls the judge must rank correctly, built from outputs already on disk (``bench_eval`` keeps them).

Each is a known damage to a known good picture, so the right order is not a matter of taste:

- ``truth+sharp`` / ``rrdb-base+sharp``: unsharp masking (amount 1, sigma 1) on the truth and on the best base
  network: oversharpening at the detail level of the picture itself, the "spectacular" look; must score below the
  picture it was made from;
- ``truth+jpeg30``: the truth re-encoded at JPEG 30: blocking and lost texture; must score below the truth;
- ``truth+shift2``: the truth moved by 2 output pixels (half an input pixel): the same ground slightly misregistered;
  shows how much the judge charges for misregistration rather than for content (a weakness, measured).
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pandas as pd
from PIL import Image

from .bench import BLOCKS
from .bench_eval import block_pairs
from .degrade import jpeg
from .imaging import TILE, Rgb
from .judge import Perceptual, combine, measure, unsharp


def _tile(mos: Rgb, i: int, j: int) -> Rgb:
    return mos[j * TILE:(j + 1) * TILE, i * TILE:(i + 1) * TILE]


def run(bench: Path, outs: Path, perc: Perceptual, scores: pd.DataFrame) -> pd.DataFrame:
    rows = []
    for b in BLOCKS:
        pairs = block_pairs(bench, b)
        rr = np.asarray(Image.open(outs / b.name / "rrdb-base-halo.png"), np.float32) / 255.0
        truths = [p["truth"] for p in pairs]
        givens = [p["given"] for p in pairs]
        variants: dict[str, list[Rgb]] = {
            "truth+sharp": [unsharp(t, 1.0, 1.0) for t in truths],  # type: ignore[arg-type]
            "rrdb-base+sharp": [unsharp(_tile(rr, int(p["i"]), int(p["j"])), 1.0, 1.0) for p in pairs],  # type: ignore[call-overload]
            "truth+jpeg30": [jpeg(t, 30) for t in truths],  # type: ignore[arg-type]
            "truth+shift2": [np.roll(t, (2, 2), axis=(0, 1)) for t in truths],  # type: ignore[call-overload]
        }
        for name, imgs in variants.items():
            t = measure(imgs, truths, givens, perc)  # type: ignore[arg-type]
            t["block"], t["source"], t["method"] = b.name, b.source, name
            t["sample"] = [f"{b.name}/{p['i']}/{p['j']}" for p in pairs]
            rows.append(t)
    raw = [c for c in scores.columns if not c.startswith("n_") and c != "score"]
    return combine(pd.concat([scores[raw], *rows], ignore_index=True))
