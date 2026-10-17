# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Tables from the judge's scores: per method and stratum, mean score with a 95 % block-bootstrap interval."""

from __future__ import annotations

import numpy as np
import pandas as pd

from .judge import COMPONENTS, bootstrap_ci

COAST_BLOCKS = {"dinan-cliff", "morgat-beach", "camaret-harbour", "soller-harbour", "formentor-cliff",
                "alcudia-beach", "talloires"}


def stratum(block: str) -> str:
    return "coast" if block in COAST_BLOCKS else "mountain"


def summary(scores: pd.DataFrame, value: str = "score") -> pd.DataFrame:
    s = scores.assign(stratum=scores.block.map(stratum))
    rows = []
    for (method, st), g in list(s.groupby(["method", "stratum"])) + [((m, "all"), g) for m, g in s.groupby("method")]:
        mean, lo, hi = bootstrap_ci(g[value].to_numpy(), g.block.to_numpy())
        rows.append({"method": method, "stratum": st, "n": len(g), "blocks": g.block.nunique(), value: mean,
                     "ci_lo": lo, "ci_hi": hi})
    return pd.DataFrame(rows).sort_values(["stratum", value], ascending=[True, False])


def components(scores: pd.DataFrame) -> pd.DataFrame:
    return scores.groupby("method")[[c.name for c in COMPONENTS]].median().round(3)


def paired_vs(scores: pd.DataFrame, a: str, b: str = "bicubic") -> tuple[float, float, float, float]:
    """Mean of score(a) - score(b) per tile with its block-bootstrap interval, and the share of tiles a wins."""
    p = scores.pivot_table(index=["block", "sample"], columns="method", values="score").reset_index()
    d = (p[a] - p[b]).to_numpy()
    mean, lo, hi = bootstrap_ci(d, p.block.to_numpy())
    return mean, lo, hi, float((d > 0).mean())
