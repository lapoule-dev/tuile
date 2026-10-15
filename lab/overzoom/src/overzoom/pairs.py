# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Is a (parent, child) pair one acquisition seen at two zooms, or two acquisitions?

A served parent and its children can come from different flights or processings: another date, another sun,
another colour balance, sometimes another content. Before a parent is used as the input whose child is the
truth, it is brought to the child's colours, then compared with the child reduced to its scale:

1. reduce the child 2x2 to the parent's scale;
2. fit a gain and an offset per channel on the low frequencies, parent onto reduced child — the colour balance;
3. after that equalisation, compare structure (SSIM on luminance) and remaining colour (CIEDE2000).

A pair under the policy's thresholds is "another acquisition": it stays in the dataset, flagged, and is never
measured as if the parent were a degradation of the child.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np
import numpy.typing as npt

from .metrics import box_down2, delta_e, lowpass, ssim_y
from .tiles import Rgb


@dataclass(frozen=True)
class PairPolicy:
    min_ssim_y: float = 0.70
    max_delta_e: float = 4.0
    sigma: float = 4.0
    """Low-pass radius, in parent pixels, of the colour fit and of the colour difference."""


@dataclass(frozen=True)
class Equalisation:
    gain: npt.NDArray[np.float32]
    offset: npt.NDArray[np.float32]

    def apply(self, img: Rgb) -> Rgb:
        return np.asarray(np.clip(img * self.gain + self.offset, 0.0, 1.0), dtype=np.float32)


@dataclass(frozen=True)
class PairCheck:
    ssim_y_raw: float
    delta_e_raw: float
    ssim_y: float
    delta_e: float
    equalisation: Equalisation
    valid: bool


def fit_equalisation(src: Rgb, ref: Rgb, sigma: float) -> Equalisation:
    """Least-squares gain and offset per channel, ``ref ≈ gain * src + offset``, on low-passed images."""
    a, b = lowpass(src, sigma).reshape(-1, 3), lowpass(ref, sigma).reshape(-1, 3)
    gain = np.empty(3, np.float32)
    offset = np.empty(3, np.float32)
    for c in range(3):
        x, y = a[:, c].astype(np.float64), b[:, c].astype(np.float64)
        var = float(np.var(x))
        g = float(np.cov(x, y, bias=True)[0, 1] / var) if var > 1e-8 else 1.0
        gain[c], offset[c] = g, float(y.mean() - g * x.mean())
    return Equalisation(gain, offset)


DEFAULT_POLICY = PairPolicy()


def check_pair(parent_quarter: Rgb, child: Rgb, policy: PairPolicy = DEFAULT_POLICY) -> PairCheck:
    """``parent_quarter``: what the parent shows of the child, at the parent's scale (half the child's size)."""
    ref = box_down2(child)
    if ref.shape != parent_quarter.shape:
        raise ValueError(f"parent quarter {parent_quarter.shape} vs reduced child {ref.shape}")
    eq = fit_equalisation(parent_quarter, ref, policy.sigma)
    after = eq.apply(parent_quarter)
    s_raw, e_raw = ssim_y(parent_quarter, ref), delta_e(parent_quarter, ref, policy.sigma)
    s, e = ssim_y(after, ref), delta_e(after, ref, policy.sigma)
    return PairCheck(s_raw, e_raw, s, e, eq, s >= policy.min_ssim_y and e <= policy.max_delta_e)
