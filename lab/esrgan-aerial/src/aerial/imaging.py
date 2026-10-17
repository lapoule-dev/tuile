# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Image basics, the same as the overzoom lab's (``lab/overzoom``), kept here so this lab stands on its own.

Images are float32 RGB in [0, 1], shape (height, width, 3). Detail is measured on BT.601 luminance (Y).
"""

from __future__ import annotations

import numpy as np
import numpy.typing as npt
from PIL import Image
from skimage.metrics import structural_similarity

Rgb = npt.NDArray[np.float32]
TILE = 256


def to_uint8(img: Rgb) -> npt.NDArray[np.uint8]:
    return np.asarray(np.clip(img * 255.0 + 0.5, 0, 255), dtype=np.uint8)


def to_float(img: npt.NDArray[np.uint8]) -> Rgb:
    return img.astype(np.float32) / 255.0


def luma(img: Rgb) -> npt.NDArray[np.float32]:
    y = (16.0 + 65.481 * img[..., 0] + 128.553 * img[..., 1] + 24.966 * img[..., 2]) / 255.0
    return np.asarray(y, dtype=np.float32)


def psnr_y(a: Rgb, b: Rgb) -> float:
    mse = float(np.mean((luma(a) - luma(b)) ** 2))
    return 10.0 * float(np.log10(1.0 / max(mse, 1e-12)))


def ssim_y(a: Rgb, b: Rgb) -> float:
    return float(structural_similarity(luma(a), luma(b), data_range=1.0))


def box_down2(img: Rgb) -> Rgb:
    h, w = img.shape[0] // 2 * 2, img.shape[1] // 2 * 2
    return np.asarray(img[:h, :w].reshape(h // 2, 2, w // 2, 2, -1).mean(axis=(1, 3)), np.float32)


def gaussian_blur(img: Rgb, sigma: float) -> Rgb:
    if sigma <= 0:
        return img
    r = max(1, int(np.ceil(3 * sigma)))
    k = np.exp(-(np.arange(-r, r + 1) ** 2) / (2 * sigma**2))
    k /= k.sum()
    pad = np.pad(img, ((r, r), (r, r), (0, 0)), mode="reflect")
    h, w = img.shape[:2]
    rows = np.zeros((h + 2 * r, w, img.shape[2]), np.float64)
    for i in range(2 * r + 1):
        rows += k[i] * pad[:, i:i + w]
    out = np.zeros((h, w, img.shape[2]), np.float64)
    for i in range(2 * r + 1):
        out += k[i] * rows[i:i + h]
    return out.astype(np.float32)


def bicubic(img: Rgb, scale: int) -> Rgb:
    if scale == 1:
        return img
    h, w = img.shape[:2]
    chans = [np.asarray(Image.fromarray(img[..., c], "F").resize((w * scale, h * scale), Image.Resampling.BICUBIC))
             for c in range(3)]
    return np.asarray(np.stack(chans, axis=-1), np.float32)


def seam_score(mosaic: Rgb, period: int) -> float:
    """Mean absolute luminance step across the vertical and horizontal tile boundaries of a mosaic, over the mean
    step between interior pixels: 1 is seamless, above 1 the grid shows."""
    y = luma(mosaic)
    ratios = []
    for d in (np.abs(np.diff(y, axis=1)).mean(axis=0), np.abs(np.diff(y, axis=0)).mean(axis=1)):
        edges = np.arange(period - 1, d.size, period)
        edges = edges[edges < d.size]
        inner = np.setdiff1d(np.arange(d.size), np.concatenate([edges - 1, edges, edges + 1]))
        ratios.append(float(d[edges].mean() / d[inner].mean()))
    return float(np.mean(ratios))
