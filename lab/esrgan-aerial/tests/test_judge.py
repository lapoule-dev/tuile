# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

import numpy as np
import pandas as pd

from aerial.judge import COMPONENTS, combine, edge_scores, high_band_gap, unsharp


def _scene(seed: int = 0) -> np.ndarray:
    """A roof-and-road-like picture: flat areas with straight boundaries, a little texture."""
    rng = np.random.default_rng(seed)
    img = np.full((128, 128, 3), 0.35, np.float32)
    img[20:60, 30:90] = (0.7, 0.4, 0.3)
    img[80:90, :] = (0.2, 0.2, 0.22)
    img[:, 100:104] = (0.9, 0.9, 0.9)
    return np.clip(img + rng.normal(0, 0.01, img.shape), 0, 1).astype(np.float32)


def test_invented_edges_lower_precision_missing_ones_lower_recall() -> None:
    gt = _scene()
    p, r = edge_scores(gt, gt)
    assert p > 0.99 and r > 0.99
    invented = gt.copy()
    invented[100:120, 10:40] = 0.8  # a "car" that is not there
    p2, r2 = edge_scores(invented, gt)
    assert p2 < p - 0.05 and r2 > 0.95
    missing = gt.copy()
    missing[20:60, 30:90] = 0.35  # the roof is gone
    p3, r3 = edge_scores(missing, gt)
    assert r3 < r - 0.2 and p3 > 0.95


def test_high_band_gap_sign_tells_oversharpened_from_blurred() -> None:
    rng = np.random.default_rng(1)
    gt = np.clip(rng.normal(0.5, 0.15, (128, 128, 3)), 0, 1).astype(np.float32)
    from aerial.imaging import gaussian_blur

    gt = gaussian_blur(gt, 1.0)  # natural-ish falloff
    assert abs(high_band_gap(gt, gt)) < 1e-6
    assert high_band_gap(gaussian_blur(gt, 1.0), gt) < -0.3
    assert high_band_gap(unsharp(gt, 2.0), gt) > 0.3


def test_combine_puts_bicubic_at_zero_and_truth_at_one() -> None:
    rows = []
    for method, k in (("bicubic", 0.0), ("truth", 1.0), ("half", 0.5)):
        row: dict[str, object] = {"sample": 0, "method": method}
        for c in COMPONENTS:
            lo, hi = (0.4, 0.0) if c.lower_is_better else (20.0, 40.0)
            row[c.name] = 36.0 if c.hinge_db else lo + k * (hi - lo)  # a hinge only counts below the truth
        rows.append(row)
    out = combine(pd.DataFrame(rows))
    s = out.set_index("method")["score"]
    assert abs(s["bicubic"]) < 1e-9 and abs(s["truth"] - 1) < 1e-9 and abs(s["half"] - 0.5) < 1e-9


def test_consistency_above_the_truths_earns_nothing_below_it_costs() -> None:
    rows = []
    for method, cons in (("bicubic", 36.5), ("truth", 35.3), ("over", 39.7), ("drifts", 32.3)):
        row: dict[str, object] = {"sample": 0, "method": method}
        for c in COMPONENTS:
            row[c.name] = 0.5
        row["consistency"] = cons
        rows.append(row)
    n = combine(pd.DataFrame(rows)).set_index("method")["n_consistency"]
    assert n["over"] == n["truth"] == n["bicubic"] == 0.0
    assert abs(n["drifts"] - (-1.0)) < 1e-9  # 3 dB under the truth's own agreement with the input
