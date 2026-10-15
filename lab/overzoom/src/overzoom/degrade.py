# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Damage applied to clean tiles, so that a method's output can be compared with the tile it came from.

The truth is always a real tile, and the input is that same tile damaged by us: no other flight, no other
processing, nothing but the damage under study. Each damage is a pipeline in a fixed order:

    blur (Gaussian) → reduction (2x2 box, k times) → noise (Gaussian) → JPEG re-encoding

and every random part is drawn from a generator seeded by the tile, so a dataset rebuilds byte for byte.
"""

from __future__ import annotations

import io
from dataclasses import dataclass

import numpy as np
from PIL import Image

from .metrics import box_down2
from .tiles import Rgb, to_float, to_uint8

# The IJG reference luminance table (ITU T.81 Annex K), in natural order.
_IJG_LUMA = np.array([
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
], np.float64)


def jpeg_quality(data: bytes) -> int:
    """The IJG quality a JPEG was most likely written at, from its luminance quantisation table; 90 when unknown."""
    im = Image.open(io.BytesIO(data))
    tables = getattr(im, "quantization", None)
    if not tables or 0 not in tables:
        return 90
    q = np.asarray(tables[0], np.float64)
    if q.size != 64:
        return 90
    scale = float(np.mean(q * 100.0 / _IJG_LUMA))
    quality = (200.0 - scale) / 2.0 if scale <= 100.0 else 5000.0 / scale
    return int(np.clip(round(quality), 1, 100))


def gaussian_blur(img: Rgb, sigma: float) -> Rgb:
    if sigma <= 0:
        return img
    r = max(1, int(np.ceil(3 * sigma)))
    k = np.exp(-(np.arange(-r, r + 1) ** 2) / (2 * sigma**2))
    k /= k.sum()
    pad = np.pad(img, ((r, r), (r, r), (0, 0)), mode="reflect")
    h, w = img.shape[:2]
    rows = np.zeros((h + 2 * r, w, 3), np.float64)
    for i in range(2 * r + 1):
        rows += k[i] * pad[:, i:i + w]
    out = np.zeros((h, w, 3), np.float64)
    for i in range(2 * r + 1):
        out += k[i] * rows[i:i + h]
    return out.astype(np.float32)


def reencode(img: Rgb, quality: int) -> Rgb:
    buf = io.BytesIO()
    Image.fromarray(to_uint8(img)).save(buf, "JPEG", quality=quality)
    return to_float(np.asarray(Image.open(io.BytesIO(buf.getvalue())).convert("RGB")))


@dataclass(frozen=True)
class Damage:
    """One damage pipeline. ``jpeg`` is an IJG quality, or 0 for "the quality of the source tile", or None."""

    name: str
    blur: float = 0.0
    reduce: int = 1
    noise: float = 0.0
    jpeg: int | None = None

    @property
    def scale(self) -> int:
        """How much a method has to enlarge to get back to the truth's size."""
        return 2**self.reduce if self.reduce > 0 else 1

    def apply(self, img: Rgb, rng: np.random.Generator, source_quality: int = 90) -> Rgb:
        out = gaussian_blur(img, self.blur)
        for _ in range(self.reduce):
            out = box_down2(out)
        if self.noise > 0:
            out = np.clip(out + rng.normal(0.0, self.noise, out.shape), 0.0, 1.0).astype(np.float32)
        if self.jpeg is not None:
            out = reencode(out, source_quality if self.jpeg == 0 else self.jpeg)
        return out


# A small catalogue, one variable at a time, then everything together. All at x2: the overzoom question.
CATALOGUE: tuple[Damage, ...] = (
    Damage("reduce", reduce=1),
    Damage("reduce+blur", blur=1.0, reduce=1),
    Damage("reduce+noise", reduce=1, noise=0.02),
    Damage("reduce+jpeg", reduce=1, jpeg=0),
    Damage("reduce+all", blur=1.0, reduce=1, noise=0.02, jpeg=0),
)
