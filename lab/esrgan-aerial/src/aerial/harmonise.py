# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Bring an open orthophoto's colours to the commercial layer's, the way tuile's radiometry does it.

The method is tuile-radiometry's (``crates/tuile-radiometry``: ``stats``, ``matrix``), re-expressed here for the
lab, not ported:

- a picture's **tone** is the mean of its texels block by block, **in linear light**, leaving out texels too dark or
  too bright to say anything (shadow, water glint, a saturated roof);
- the correction is a **colour matrix**: three rows of four (a channel out from red, green, blue in, plus an
  offset), fitted by least squares from the source's tones to the reference's, **held toward changing nothing**
  (a ridge to the identity, stronger on what a channel takes from the other two and on the offset than on its own
  gain: left free, those press out the content one picture shows and the other does not), and **reweighted round
  after round** by how far the fit leaves each block, so that ground that changed between the two dates counts less;
- not histogram matching: detail is never repainted with the reference's content, only the tone moves.

One matrix per **site** (every anchor of the site pooled), as tuile's ``Look`` is one matrix a zone: a region gets
one look, not a patchwork. The fit is done where the two are compared at the same scale: the orthophoto reduced by
the measured pyramid to the commercial tile's resolution.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np
import numpy.typing as npt

from .imaging import Rgb

BLOCK = 8
TOO_DARK, TOO_BRIGHT = 6 / 255, 250 / 255
HOLD_GAIN, HOLD_REST = 4.0, 40.0
ROUNDS = 6

Affine = npt.NDArray[np.float64]
"""(3, 4): a row a channel out; red, green, blue in, then the offset."""
IDENTITY: Affine = np.hstack([np.eye(3), np.zeros((3, 1))])


def to_linear(img: Rgb) -> npt.NDArray[np.float64]:
    c = img.astype(np.float64)
    return np.where(c <= 0.04045, c / 12.92, ((c + 0.055) / 1.055) ** 2.4)


def to_srgb(lin: npt.NDArray[np.float64]) -> Rgb:
    c = np.clip(lin, 0.0, 1.0)
    return np.asarray(np.where(c <= 0.0031308, 12.92 * c, 1.055 * c ** (1 / 2.4) - 0.055), np.float32)


def tones(img: Rgb, block: int = BLOCK) -> tuple[npt.NDArray[np.float64], npt.NDArray[np.float64]]:
    """(mean linear colour per block, share of the block's texels that counted)."""
    h, w = img.shape[0] // block * block, img.shape[1] // block * block
    a = img[:h, :w]
    ok = (a.min(axis=-1) > TOO_DARK) & (a.max(axis=-1) < TOO_BRIGHT)
    lin = to_linear(a) * ok[..., None]
    sh = (h // block, block, w // block, block)
    n = ok.reshape(sh).sum(axis=(1, 3)).astype(np.float64)
    s = lin.reshape(sh + (3,)).sum(axis=(1, 3))
    mean = s / np.maximum(n, 1)[..., None]
    return mean.reshape(-1, 3), (n / block**2).reshape(-1)


def fit(src: npt.NDArray[np.float64], ref: npt.NDArray[np.float64], valid: npt.NDArray[np.float64],
        min_valid: float = 0.5) -> Affine:
    """The held, reweighted least-squares colour matrix taking tones ``src`` (N, 3) to ``ref`` (N, 3)."""
    keep = valid >= min_valid
    x = np.hstack([src[keep], np.ones((int(keep.sum()), 1))])
    y = ref[keep]
    if len(x) < 8:
        return IDENTITY.copy()
    w = np.ones(len(x))
    m = IDENTITY.copy()
    for _ in range(ROUNDS):
        rows = []
        for o in range(3):
            hold = np.full(4, HOLD_REST)
            hold[o] = HOLD_GAIN
            xw = x * w[:, None]
            a = xw.T @ x + np.diag(hold) * len(x) * 1e-3
            b = xw.T @ y[:, o] + np.diag(hold) * len(x) * 1e-3 @ IDENTITY[o]
            rows.append(np.linalg.solve(a, b))
        m = np.vstack(rows)
        r = np.linalg.norm(x @ m.T - y, axis=1)
        scale = 1.4826 * np.median(r) + 1e-6
        w = 1.0 / np.maximum(1.0, r / (1.5 * scale))  # Huber: blocks far off count less
    return m


def apply(img: Rgb, m: Affine) -> Rgb:
    lin = to_linear(img)
    out = lin @ m[:, :3].T + m[:, 3]
    return to_srgb(out)


@dataclass(frozen=True)
class SiteLook:
    site: str
    matrix: Affine
    pairs: int
    delta_e_before: float
    delta_e_after: float
