# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

import random

import numpy as np
import pytest
from PIL import Image

from aerial.harvest import native_psnr, nodata_share, target_gsd


def _ground(seed: int = 0) -> np.ndarray:
    rng = np.random.default_rng(seed)
    return np.clip(rng.normal(128, 40, (256, 256, 3)), 0, 255).astype(np.uint8)


def test_native_psnr_tells_upsampled_from_native() -> None:
    native = _ground()
    half = np.asarray(Image.fromarray(native).resize((128, 128), Image.Resampling.BOX))
    upsampled = np.asarray(Image.fromarray(half).resize((256, 256), Image.Resampling.BICUBIC))
    assert native_psnr(native) < 25.0
    assert native_psnr(upsampled) > 39.0


def test_nodata_share_counts_fill_only() -> None:
    img = _ground()
    assert nodata_share(img) < 0.01
    img[:16] = 0
    assert abs(nodata_share(img) - 16 / 256) < 0.01


def test_target_gsd() -> None:
    rng = random.Random(0)
    assert target_gsd(0.2, rng) == 0.2
    assert 0.075 <= target_gsd(0.03, rng) <= 0.15
    with pytest.raises(ValueError):
        target_gsd(0.6, rng)
