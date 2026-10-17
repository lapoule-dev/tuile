# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The x4 methods the judge compares, behind one call: an input with its halo in, the enlarged tile out.

``halo``: input pixels of real neighbours on every side; the output is cropped back to the tile. Controls the judge
must rank correctly are methods too: ``oversharpened`` (bicubic with strong unsharp masking) and ``noise`` (bicubic
with invented fine texture: high-band noise at the truth's energy), both of which a judge has to put below bicubic
on faithfulness, and the networks' base weights.
"""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path

import numpy as np

from .imaging import Rgb, bicubic, gaussian_blur
from .judge import unsharp

Method = Callable[[Rgb, int], Rgb]
"""(input with halo, halo in input pixels) -> x4 output of the tile alone."""

WEIGHTS = Path(__file__).resolve().parents[2] / "data" / "weights"


def _crop(img: Rgb, halo: int, scale: int = 4) -> Rgb:
    m = halo * scale
    return img[m:img.shape[0] - m, m:img.shape[1] - m] if m else img


def m_bicubic(x: Rgb, halo: int) -> Rgb:
    return _crop(np.clip(bicubic(x, 4), 0, 1).astype(np.float32), halo)


def m_oversharpened(x: Rgb, halo: int) -> Rgb:
    return unsharp(m_bicubic(x, halo), 2.0, 1.2)


def m_noise(x: Rgb, halo: int) -> Rgb:
    b = m_bicubic(x, halo)
    rng = np.random.default_rng(int(b.sum() * 1000) % 2**32)
    n = rng.normal(0.0, 1.0, b.shape[:2]).astype(np.float32)
    n = n - gaussian_blur(np.repeat(n[..., None], 3, -1), 1.0)[..., 0]
    return np.asarray(np.clip(b + 0.04 * n[..., None], 0, 1), np.float32)


class Network:
    """A spandrel-loaded x4 network, on ``device``, run on one haloed input at a time."""

    def __init__(self, path: Path, device: str) -> None:
        import torch
        from spandrel import ModelLoader

        self.torch, self.device = torch, device
        self.model = ModelLoader().load_from_file(str(path)).model.to(device).eval()

    def __call__(self, x: Rgb, halo: int) -> Rgb:
        t = self.torch.from_numpy(np.ascontiguousarray(x)).permute(2, 0, 1)[None].to(self.device)
        with self.torch.no_grad():
            y = self.model(t).clamp(0, 1)[0].permute(1, 2, 0).cpu().numpy()
        return _crop(np.asarray(y, np.float32), halo)


def registry(device: str) -> dict[str, Method]:
    return {
        "bicubic": m_bicubic,
        "oversharpened": m_oversharpened,
        "noise": m_noise,
        "compact-base": Network(WEIGHTS / "realesr-general-x4v3.pth", device),
        "rrdb-base": Network(WEIGHTS / "RealESRGAN_x4plus.pth", device),
    }
