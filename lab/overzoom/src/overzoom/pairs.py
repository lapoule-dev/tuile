# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Is an (ancestor, child) pair one acquisition seen at two zooms, or two acquisitions?

A served ancestor and its descendants can come from different flights or processings: another date, another sun,
another colour balance, sometimes another content. Before an ancestor is used as the input whose descendant is the
truth, it is compared with the child reduced to its scale (2x2 as many times as needed):

1. **structure**, colour left out: the correlation of the luminance detail (high-pass) of both. It is blind to gain
   and offset, so no colour correction can manufacture it; two unrelated grounds give about 0.
2. **colour balance**: per channel, the low-passed means and spreads are matched (gain and offset). The gain is
   recorded, not judged: on low-contrast ground a ratio of spreads is unstable, and flat areas are not mismatches.
3. **remaining colour**, after that equalisation: CIEDE2000 on the low-passed images;
4. **tint**: CIEDE2000 between the mean colours, before equalisation — what a method fed the ancestor as served has
   to correct on top of the detail.

Each pair gets a class: ``same`` (one acquisition, same tint), ``tinted`` (one acquisition, another tint: same detail,
colours recoverable by equalisation) or ``other`` (unrelated detail, or colours equalisation cannot bring back).

Matching moments rather than fitting least squares matters: a least-squares gain collapses towards 0 when the two
images are unrelated, flattening the ancestor into its mean — which then *looks* similar to anything. Moment matching
only ever corrects the balance.

An ``other`` pair stays in the dataset, flagged, and is never measured as if the ancestor were a degradation of the
child.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np
import numpy.typing as npt

from .metrics import box_down2, colour_cast, delta_e, lowpass, luma, ssim_y
from .tiles import Rgb


@dataclass(frozen=True)
class PairPolicy:
    min_structure: float = 0.3
    """Minimum correlation of the luminance detail of ancestor and reduced child (unrelated grounds: about 0)."""
    max_delta_e: float = 2.0
    """Maximum CIEDE2000 left after equalisation."""
    max_cast: float = 1.5
    """Tint difference (CIEDE2000 of the mean colours, as served) above which a pair is ``tinted``."""
    sigma: float = 4.0
    """Low-pass radius, in parent pixels (x2), of the colour fit, the colour difference and the detail split;
    halved per extra level so it always spans the same ground."""


@dataclass(frozen=True)
class Equalisation:
    gain: npt.NDArray[np.float32]
    offset: npt.NDArray[np.float32]

    def apply(self, img: Rgb) -> Rgb:
        return np.asarray(np.clip(img * self.gain + self.offset, 0.0, 1.0), dtype=np.float32)


SAME, TINTED, OTHER = "same", "tinted", "other"


@dataclass(frozen=True)
class PairCheck:
    structure: float
    cast_raw: float
    ssim_y_raw: float
    delta_e_raw: float
    ssim_y: float
    delta_e: float
    equalisation: Equalisation
    kind: str
    """``same``, ``tinted`` or ``other``."""

    @property
    def valid(self) -> bool:
        return self.kind != OTHER


def fit_equalisation(src: Rgb, ref: Rgb, sigma: float) -> Equalisation:
    """Per channel, the gain and offset that give the low-passed ``src`` the mean and spread of the low-passed ``ref``."""
    a, b = lowpass(src, sigma).reshape(-1, 3), lowpass(ref, sigma).reshape(-1, 3)
    sa, sb = a.std(axis=0), b.std(axis=0)
    gain = np.where(sa > 1e-4, sb / np.maximum(sa, 1e-4), 1.0).astype(np.float32)
    offset = (b.mean(axis=0) - gain * a.mean(axis=0)).astype(np.float32)
    return Equalisation(gain, offset)


def structure(a: Rgb, b: Rgb, sigma: float) -> float:
    """Pearson correlation of the luminance detail (image minus its low-pass) of two images of the same size."""
    da = luma(a) - luma(lowpass(a, sigma))
    db = luma(b) - luma(lowpass(b, sigma))
    da, db = da - da.mean(), db - db.mean()
    den = float(np.sqrt((da**2).sum() * (db**2).sum()))
    return float((da * db).sum() / den) if den > 1e-12 else 0.0


DEFAULT_POLICY = PairPolicy()


def check_pair(ancestor_part: Rgb, child: Rgb, policy: PairPolicy = DEFAULT_POLICY) -> PairCheck:
    """``ancestor_part``: what an ancestor shows of the child, at the ancestor's scale."""
    ref, factor = child, 1
    while ref.shape[0] > ancestor_part.shape[0]:
        ref, factor = box_down2(ref), factor * 2
    if ref.shape != ancestor_part.shape:
        raise ValueError(f"ancestor part {ancestor_part.shape} vs reduced child {ref.shape}")
    sigma = max(0.5, policy.sigma * 2 / factor)
    corr = structure(ancestor_part, ref, sigma)
    eq = fit_equalisation(ancestor_part, ref, sigma)
    after = eq.apply(ancestor_part)
    s_raw, e_raw = ssim_y(ancestor_part, ref), delta_e(ancestor_part, ref, sigma)
    s, e = ssim_y(after, ref), delta_e(after, ref, sigma)
    cast = colour_cast(ancestor_part, ref)
    if corr < policy.min_structure or e > policy.max_delta_e:
        kind = OTHER
    else:
        kind = TINTED if cast > policy.max_cast else SAME
    return PairCheck(corr, cast, s_raw, e_raw, s, e, eq, kind)
