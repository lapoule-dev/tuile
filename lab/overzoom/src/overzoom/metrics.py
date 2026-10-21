# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Image measures that need no GPU.

Detail is measured on luminance only (the Y of BT.601 YCbCr, as in the super-resolution literature), colour on its
own (CIEDE2000 on low-passed images), so a method that shifts the hue does not pass for one that loses detail.
"""

from __future__ import annotations

import numpy as np
import numpy.typing as npt
from skimage.color import deltaE_ciede2000, rgb2lab
from skimage.filters import gaussian
from skimage.metrics import structural_similarity

from .tiles import Rgb

Gray = npt.NDArray[np.float32]


def luma(img: Rgb) -> Gray:
    """BT.601 Y, studio range, as a fraction: (16 + 65.481 R + 128.553 G + 24.966 B) / 255."""
    y = (16.0 + 65.481 * img[..., 0] + 128.553 * img[..., 1] + 24.966 * img[..., 2]) / 255.0
    return np.asarray(y, dtype=np.float32)


def psnr_y(a: Rgb, b: Rgb) -> float:
    mse = float(np.mean((luma(a) - luma(b)) ** 2))
    return 10.0 * float(np.log10(1.0 / max(mse, 1e-12)))


def ssim_y(a: Rgb, b: Rgb) -> float:
    return float(structural_similarity(luma(a), luma(b), data_range=1.0))


def lowpass(img: Rgb, sigma: float) -> Rgb:
    out = gaussian(img, sigma=sigma, channel_axis=-1, preserve_range=True)
    return np.asarray(out, np.float32)


def delta_e(a: Rgb, b: Rgb, sigma: float = 2.0) -> float:
    """Mean CIEDE2000 between the low-passed images: the colour difference, detail left out."""
    return float(np.mean(deltaE_ciede2000(rgb2lab(lowpass(a, sigma)), rgb2lab(lowpass(b, sigma)))))


def colour_cast(a: Rgb, b: Rgb) -> float:
    """CIEDE2000 between the mean colours of two images: their difference of tint, every detail left out."""
    la = rgb2lab(a.reshape(-1, 1, 3).mean(axis=0, keepdims=True))
    lb = rgb2lab(b.reshape(-1, 1, 3).mean(axis=0, keepdims=True))
    return float(deltaE_ciede2000(la, lb).item())


def box_down2(img: Rgb) -> Rgb:
    """2x2 box reduction — the assumed kernel of the pyramid."""
    h, w = img.shape[0] // 2 * 2, img.shape[1] // 2 * 2
    return np.asarray(img[:h, :w].reshape(h // 2, 2, w // 2, 2, -1).mean(axis=(1, 3)), np.float32)


def radial_spectrum(img: Rgb, bins: int = 64) -> tuple[npt.NDArray[np.float64], npt.NDArray[np.float64]]:
    """Mean log power of the luminance spectrum per radial frequency, in cycles per pixel (0 to 0.5).

    Detail a method adds shows up as power in the high frequencies; whether it continues the truth's slope or
    bends away from it (noise, periodic patterns) is the question.
    """
    y = luma(img).astype(np.float64)
    y = y - y.mean()
    h, w = y.shape
    win = np.outer(np.hanning(h), np.hanning(w))
    p = np.abs(np.fft.fftshift(np.fft.fft2(y * win))) ** 2
    fy = np.fft.fftshift(np.fft.fftfreq(h))[:, None]
    fx = np.fft.fftshift(np.fft.fftfreq(w))[None, :]
    r = np.sqrt(fx**2 + fy**2)
    edges = np.linspace(0.0, 0.5, bins + 1)
    idx = np.digitize(r.ravel(), edges) - 1
    ok = (idx >= 0) & (idx < bins)
    total = np.bincount(idx[ok], weights=np.log10(p.ravel()[ok] + 1e-12), minlength=bins)
    count = np.bincount(idx[ok], minlength=bins)
    return (edges[:-1] + edges[1:]) / 2, total / np.maximum(count, 1)


def seam_score(mosaic: Rgb, period: int) -> float:
    """Mean absolute step across the tile boundaries of a mosaic, over the mean step between interior columns.

    1 is seamless; above 1 the grid shows. Only vertical boundaries are measured (rows are symmetric).
    """
    d = np.abs(np.diff(luma(mosaic), axis=1)).mean(axis=0)
    edges = np.arange(period - 1, mosaic.shape[1] - 1, period)
    inner = np.setdiff1d(np.arange(d.size), np.concatenate([edges - 1, edges, edges + 1]))
    return float(d[edges].mean() / d[inner].mean())
