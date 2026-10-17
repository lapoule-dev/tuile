# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The damage between real ground and the tile we are given, modelled on what was measured of the commercial layer.

Measured on the lab's zone (``luchon-z19-v3``, 200 ``same`` pairs, 2026-10-17): a served z18 tile is best matched by
its z19 child blurred by a Gaussian of **sigma 0.7** child pixels then reduced 2 x 2 (PSNR-Y 38.0 dB, bicubic
reduction; box 37.9; no blur 37.4, sigma 1.2 36.7). Every level is JPEG at IJG quality **79** (95 % of tiles; 92
otherwise), **4:2:0** chroma, baseline. Its z19 holds less native detail than its z18 (``sources``), so a share of
inputs is softened as an upsampled level would be.

A x4 input is therefore made from a high-resolution sample by

    acquisition softness (sigma 0 to 0.5)
    → [pyramid step: Gaussian sigma 0.5 to 0.9, reduction x2 (bicubic or box)] x 2,
      with an intermediate JPEG on 30 % of samples (a level built from the previous level's tiles)
    → on 25 % of samples, a soft upsampled level: reduced by 1.2 to 1.6 and enlarged back
    → a little sensor noise on 20 % of samples (sigma up to 0.008)
    → JPEG 4:2:0: q79 on 80 % of samples, q92 on 10 %, q75 to 95 otherwise.

Every draw comes from a generator seeded by the sample, so an input rebuilds byte for byte. ``pipeline_down`` is the
deterministic part alone (no JPEG, no noise, the measured sigma): what a method's output, reduced, is compared with
for consistency.
"""

from __future__ import annotations

import io
import zlib
from dataclasses import dataclass

import numpy as np
from PIL import Image

from .imaging import Rgb, box_down2, gaussian_blur, to_float, to_uint8

PYRAMID_SIGMA = 0.7
"""Measured: the Gaussian, in pixels of the finer level, the commercial pyramid applies before each 2 x 2 step."""
JPEG_QUALITY = 79
CHROMA = "4:2:0"


def bicubic_down2(img: Rgb) -> Rgb:
    h, w = img.shape[0] // 2, img.shape[1] // 2
    chans = [np.asarray(Image.fromarray(img[..., c], "F").resize((w, h), Image.Resampling.BICUBIC))
             for c in range(3)]
    return np.asarray(np.stack(chans, axis=-1), np.float32)


def resize(img: Rgb, w: int, h: int) -> Rgb:
    chans = [np.asarray(Image.fromarray(img[..., c], "F").resize((w, h), Image.Resampling.BICUBIC))
             for c in range(3)]
    return np.asarray(np.clip(np.stack(chans, axis=-1), 0.0, 1.0), np.float32)


def jpeg(img: Rgb, quality: int, subsampling: str = CHROMA) -> Rgb:
    buf = io.BytesIO()
    Image.fromarray(to_uint8(img)).save(buf, "JPEG", quality=quality, subsampling=subsampling)
    return to_float(np.asarray(Image.open(io.BytesIO(buf.getvalue())).convert("RGB")))


def pyramid_step(img: Rgb, sigma: float = PYRAMID_SIGMA, box: bool = False) -> Rgb:
    out = gaussian_blur(img, sigma)
    return box_down2(out) if box else bicubic_down2(out)


def pipeline_down(img: Rgb, scale: int = 4) -> Rgb:
    """The deterministic part of the pyramid: measured blur and bicubic 2 x 2, ``log2(scale)`` times."""
    while scale > 1:
        img, scale = pyramid_step(img), scale // 2
    return np.asarray(np.clip(img, 0.0, 1.0), np.float32)


@dataclass(frozen=True)
class Draw:
    """What one sample's damage was, for the record."""

    pre_sigma: float
    sigmas: tuple[float, float]
    box: tuple[bool, bool]
    mid_jpeg: int | None
    soften: float | None
    noise: float
    quality: int


def draw(rng: np.random.Generator) -> Draw:
    pre = float(rng.uniform(0.0, 0.5))
    sig = (float(rng.uniform(0.5, 0.9)), float(rng.uniform(0.5, 0.9)))
    box = (bool(rng.random() < 0.5), bool(rng.random() < 0.5))
    mid = int(rng.choice([79, 79, 85, 92])) if rng.random() < 0.3 else None
    soften = float(rng.uniform(1.2, 1.6)) if rng.random() < 0.25 else None
    noise = float(rng.uniform(0.0, 0.008)) if rng.random() < 0.2 else 0.0
    u = rng.random()
    q = JPEG_QUALITY if u < 0.8 else (92 if u < 0.9 else int(rng.integers(75, 96)))
    return Draw(pre, sig, box, mid, soften, noise, q)


def sample_rng(seed: int, sample_id: str) -> np.random.Generator:
    return np.random.default_rng([seed, zlib.crc32(sample_id.encode())])


def degrade(hr: Rgb, d: Draw, rng: np.random.Generator) -> Rgb:
    """The x4 input of ``hr`` (height and width multiples of 4) under the damage ``d``."""
    if hr.shape[0] % 4 or hr.shape[1] % 4:
        raise ValueError(f"{hr.shape}: sides must be multiples of 4")
    out = gaussian_blur(hr, d.pre_sigma)
    out = pyramid_step(out, d.sigmas[0], d.box[0])
    if d.mid_jpeg is not None:
        out = jpeg(out, d.mid_jpeg)
    out = pyramid_step(out, d.sigmas[1], d.box[1])
    out = np.asarray(np.clip(out, 0.0, 1.0), np.float32)
    if d.soften is not None:
        h, w = out.shape[:2]
        out = resize(resize(out, max(1, round(w / d.soften)), max(1, round(h / d.soften))), w, h)
    if d.noise > 0:
        out = np.asarray(np.clip(out + rng.normal(0.0, d.noise, out.shape), 0.0, 1.0), np.float32)
    return jpeg(out, d.quality)


def make_input(hr: Rgb, sample_id: str, seed: int = 17501) -> tuple[Rgb, Draw]:
    rng = sample_rng(seed, sample_id)
    d = draw(rng)
    return degrade(hr, d, rng), d
