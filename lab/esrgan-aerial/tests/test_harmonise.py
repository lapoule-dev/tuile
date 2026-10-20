# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

import numpy as np

from aerial.harmonise import IDENTITY, apply, fit, to_linear, to_srgb, tones


def _ground(seed: int = 0) -> np.ndarray:
    rng = np.random.default_rng(seed)
    base = rng.uniform(0.15, 0.8, (16, 16, 3))
    return np.repeat(np.repeat(base, 8, 0), 8, 1).astype(np.float32)


def test_linear_round_trip() -> None:
    img = _ground()
    assert np.abs(to_srgb(to_linear(img)) - img).max() < 1e-5


def test_a_known_tint_is_recovered() -> None:
    img = _ground()
    tint = np.array([[1.10, 0.0, 0.0, 0.0], [0.0, 0.95, 0.0, 0.0], [0.0, 0.0, 0.85, 0.01]])
    ref = apply(img, tint)
    s, v = tones(img)
    r, _ = tones(ref)
    m = fit(s, r, v)
    assert np.abs(m - tint).max() < 0.03


def test_changed_ground_does_not_drag_the_fit() -> None:
    """A quarter of the blocks repainted (a new roof, a mown field): the robust fit stays near the true tint."""
    img = _ground()
    tint = np.array([[1.05, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 0.9, 0.0]])
    ref = apply(img, tint)
    ref[:64, :64] = np.float32(0.2)  # changed between the two dates
    s, v = tones(img)
    r, _ = tones(ref)
    m = fit(s, r, v)
    assert np.abs(m - tint).max() < 0.06


def test_nothing_to_fit_changes_nothing() -> None:
    img = _ground()
    s, v = tones(img)
    m = fit(s[:3], s[:3], v[:3])
    assert np.array_equal(m, IDENTITY)
