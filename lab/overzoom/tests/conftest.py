# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A synthetic zone shared by the tests: a textured ground at z19 and its ancestors in another colour balance."""

from __future__ import annotations

import io

import numpy as np
import pytest
from PIL import Image

from overzoom.degrade import gaussian_blur
from overzoom.metrics import box_down2
from overzoom.tiles import TILE, TileKey, TileStore, to_uint8


def jpeg(a: np.ndarray, quality: int = 92) -> bytes:
    b = io.BytesIO()
    Image.fromarray(to_uint8(a)).save(b, "JPEG", quality=quality)
    return b.getvalue()


def ground(n: int, seed: int = 0) -> np.ndarray:
    """A textured random field, smooth enough to survive a 2x2 reduction."""
    rng = np.random.default_rng(seed)
    f = np.cumsum(np.cumsum(rng.normal(0, 1, (n, n, 3)), 0), 1)
    g = gaussian_blur(((f - f.min()) / (f.max() - f.min())).astype(np.float32), 1.0)
    g += rng.normal(0, 0.03, g.shape).astype(np.float32)
    return np.clip(g, 0, 1).astype(np.float32)


N = 16  # z19 tiles a side
X0, Y0 = 1024, 2048  # divisible by 8, so every ancestor grid lines up


@pytest.fixture(scope="module")
def zone() -> tuple[TileStore, np.ndarray]:
    """16x16 tiles at z19, and their ancestors at z18, z17, z16 as exact reductions in another colour balance —
    except parent (X0/2 + 2, Y0/2 + 3) at z18, replaced by another ground: an acquisition that does not match."""
    big = ground(N * TILE)
    tiles: dict[TileKey, bytes] = {}
    level = big
    for depth in range(4):
        z, n = 19 - depth, N >> depth
        for j in range(n):
            for i in range(n):
                p = level[j * TILE:(j + 1) * TILE, i * TILE:(i + 1) * TILE]
                if depth > 0:
                    if (depth, i, j) == (1, 2, 3):
                        p = ground(TILE, seed=7)
                    p = np.clip(p * [1.1, 0.95, 0.9] + 0.03, 0, 1)  # another flight's colour balance
                tiles[TileKey(z, (X0 >> depth) + i, (Y0 >> depth) + j)] = jpeg(p)
        level = box_down2(level)
    return TileStore(tiles), big
