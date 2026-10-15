# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The CPU side on synthetic tiles: windows, damages, pair checks, the index and its rebuilt inputs."""

from __future__ import annotations

import io

import numpy as np
import pytest
from PIL import Image

from overzoom.dataset import LEVELS, Dataset, DatasetSpec, build_index
from overzoom.degrade import Damage, gaussian_blur, jpeg_quality
from overzoom.metrics import box_down2, delta_e, psnr_y, seam_score, ssim_y
from overzoom.pairs import PairPolicy, check_pair
from overzoom.tiles import TILE, TileKey, TileStore, to_uint8


def jpeg(a: np.ndarray, quality: int = 92) -> bytes:
    b = io.BytesIO()
    Image.fromarray(to_uint8(a)).save(b, "JPEG", quality=quality)
    return b.getvalue()


def ground(n: int, seed: int = 0) -> np.ndarray:
    """A textured random field, smooth enough to survive a 2x2 reduction."""
    rng = np.random.default_rng(seed)
    f = np.cumsum(np.cumsum(rng.normal(0, 1, (n, n, 3)), 0), 1)
    f = gaussian_blur(((f - f.min()) / (f.max() - f.min())).astype(np.float32), 1.0)
    f += rng.normal(0, 0.03, f.shape).astype(np.float32)
    return np.clip(f, 0, 1).astype(np.float32)


@pytest.fixture(scope="module")
def zone() -> tuple[TileStore, np.ndarray]:
    """6x6 tiles at z19 and their 3x3 parents at z18 as an exact 2x2 reduction, except parent (500, 1001),
    which is replaced by another ground: an acquisition that does not match its children."""
    big = ground(6 * TILE)
    tiles: dict[TileKey, bytes] = {}
    for j in range(6):
        for i in range(6):
            tiles[TileKey(19, 1000 + i, 2000 + j)] = jpeg(big[j * TILE:(j + 1) * TILE, i * TILE:(i + 1) * TILE])
    small = box_down2(big)
    other = ground(TILE, seed=7)
    for j in range(3):
        for i in range(3):
            p = small[j * TILE:(j + 1) * TILE, i * TILE:(i + 1) * TILE]
            if (i, j) == (0, 1):
                p = other
            # a colour balance of another flight
            tiles[TileKey(18, 500 + i, 1000 + j)] = jpeg(np.clip(p * [1.1, 0.95, 0.9] + 0.03, 0, 1))
    return TileStore(tiles), big


def test_window_reads_across_tiles_and_refuses_holes(zone: tuple[TileStore, np.ndarray]) -> None:
    store, big = zone
    w = store.window(19, 1000 * TILE + 10, 2000 * TILE + 20, 300, 260)
    assert w is not None and w.shape == (260, 300, 3)
    placed = float(np.abs(w - big[20:280, 10:310]).mean())
    shifted = float(np.abs(w - big[21:281, 10:310]).mean())
    assert placed < 0.03 and shifted > 1.3 * placed
    assert store.window(19, 999 * TILE, 2000 * TILE, 10, 10) is None


def test_jpeg_quality_is_read_back() -> None:
    for q in (60, 75, 90):
        assert abs(jpeg_quality(jpeg(ground(64), q)) - q) <= 2


def test_damage_is_deterministic_and_shrinks() -> None:
    img = ground(320)
    d = Damage("reduce+all", blur=1.0, reduce=1, noise=0.02, jpeg=0)
    a = d.apply(img, np.random.default_rng(3), 85)
    b = d.apply(img, np.random.default_rng(3), 85)
    assert a.shape == (160, 160, 3) and np.array_equal(a, b)
    assert d.scale == 2


def test_pair_check_equalises_colour_and_rejects_another_ground() -> None:
    child = ground(TILE)
    same = np.clip(box_down2(child) * [1.1, 0.95, 0.9] + 0.03, 0, 1).astype(np.float32)
    ok = check_pair(same, child, PairPolicy())
    assert ok.delta_e_raw > ok.delta_e and ok.valid
    other = box_down2(ground(TILE, seed=9))
    bad = check_pair(other, child, PairPolicy())
    assert not bad.valid


def test_index_and_rebuilt_inputs(zone: tuple[TileStore, np.ndarray]) -> None:
    store, _ = zone
    spec = DatasetSpec(min_texture=0.0)
    index = build_index(store, spec, progress=False)
    # tiles with all their neighbours (halo 64 at x2) and their parent context: the inner 4x4
    assert len(index) == 16
    # the children of the swapped parent are flagged as another acquisition, the others are not
    swapped = (index.x // 2 == 500) & (index.y // 2 == 1001)
    assert not index[swapped].level_valid.any() and index[~swapped].level_valid.all()
    ds = Dataset(store, spec, index)
    for name in spec.inputs:
        x = ds.input(0, name)
        s = TILE // spec.scale(name) + 2 * spec.halo
        assert x.shape == (s, s, 3), name
    lv = ds.crop(ds.input(int(index[~swapped].index[0]), LEVELS))
    t = ds.truth(int(index[~swapped].index[0]))
    assert psnr_y(lv, box_down2(t)) > 30 and delta_e(lv, box_down2(t)) < 2
    assert len(list(ds.comparisons(valid_levels_only=True))) == 16 * len(spec.inputs) - int(swapped.sum())


def test_seam_score_sees_a_grid() -> None:
    img = ground(512)
    assert seam_score(img, 128) < 1.3
    steps = img.copy()
    steps[:, 128:256] += 0.1
    assert seam_score(np.clip(steps, 0, 1), 128) > 2
    assert ssim_y(img, img) == pytest.approx(1.0)
