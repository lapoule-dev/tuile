# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The CPU side on synthetic tiles: windows, damages, pair checks, the index and its rebuilt inputs."""

from __future__ import annotations

import numpy as np
import pytest
from conftest import X0, Y0, ground, jpeg

from overzoom.dataset import Dataset, DatasetSpec, build_index
from overzoom.degrade import Damage, jpeg_quality
from overzoom.metrics import box_down2, delta_e, psnr_y, seam_score, ssim_y
from overzoom.pairs import PairPolicy, check_pair
from overzoom.tiles import TILE, TileStore
from overzoom.view import comparisons


def test_window_reads_across_tiles_and_refuses_holes(zone: tuple[TileStore, np.ndarray]) -> None:
    store, big = zone
    w = store.window(19, X0 * TILE + 10, Y0 * TILE + 20, 300, 260)
    assert w is not None and w.shape == (260, 300, 3)
    placed = float(np.abs(w - big[20:280, 10:310]).mean())
    shifted = float(np.abs(w - big[21:281, 10:310]).mean())
    assert placed < 0.03 and shifted > 1.3 * placed
    assert store.window(19, (X0 - 1) * TILE, Y0 * TILE, 10, 10) is None


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
    assert ok.delta_e_raw > ok.delta_e and ok.valid and ok.structure > 0.8 and ok.kind == "tinted"
    assert check_pair(box_down2(child), child, PairPolicy()).kind == "same"
    other = box_down2(ground(TILE, seed=9))
    bad = check_pair(other, child, PairPolicy())
    assert not bad.valid and abs(bad.structure) < 0.2


def test_index_and_rebuilt_inputs(zone: tuple[TileStore, np.ndarray]) -> None:
    store, _ = zone
    spec = DatasetSpec(min_texture=0.0)
    index = build_index(store, spec, progress=False)
    assert len(index) > 0
    # the children of the swapped z18 parent are another acquisition at z18 only
    swapped = (index.x // 2 == X0 // 2 + 2) & (index.y // 2 == Y0 // 2 + 3)
    assert swapped.sum() == 4
    assert (index[swapped].z18_class == "other").all()
    # every other ancestor is the same acquisition in another colour balance: recoverable, so "tinted"
    assert (index[~swapped].z18_class == "tinted").all()
    assert (index.z17_class == "tinted").all() and (index.z16_class == "tinted").all()
    ds = Dataset(store, spec, index)
    for name in spec.inputs:
        x = ds.input(0, name)
        s = TILE // spec.scale(name) + 2 * spec.halo
        assert x.shape == (s, s, 3), name
    i = int(index[~swapped].index[0])
    t = ds.truth(i)
    for depth, name in zip(spec.depths, spec.levels):
        served, eq = ds.crop(ds.input(i, name)), ds.crop(ds.input(i, name + "-eq"))
        ref = t
        for _ in range(depth):
            ref = box_down2(ref)
        assert eq.shape == ref.shape and delta_e(eq, ref) < 2 < delta_e(served, ref), name
    assert psnr_y(ds.crop(ds.input(i, "z18-eq")), box_down2(t)) > 30
    n_inputs = len(spec.inputs)
    # the swapped parent's children lose both their z18 inputs, served and equalised
    assert len(list(ds.comparisons(valid_only=True))) == len(index) * n_inputs - 2 * 4
    # a class filter keeps only the level inputs of that class, never a synthetic one
    others = comparisons(ds, spec.inputs, "other", "index", 1000)
    assert {c.input for c in others} == {"z18", "z18-eq"} and len(others) == 2 * 4
    tinted = comparisons(ds, spec.inputs, "tinted", "index", 100000)
    assert len(tinted) == 2 * (3 * len(index) - 4)


def test_seam_score_sees_a_grid() -> None:
    img = ground(512)
    assert seam_score(img, 128) < 1.3
    steps = img.copy()
    steps[:, 128:256] += 0.1
    assert seam_score(np.clip(steps, 0, 1), 128) > 2
    assert ssim_y(img, img) == pytest.approx(1.0)
