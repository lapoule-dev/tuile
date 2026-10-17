# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

from aerial.geo import stratum_of


def test_mountain_wins_over_coast() -> None:
    assert stratum_of(200.0, 900.0, 300.0) == "mountain"  # a cliff coast in a range


def test_high_plateau_is_mountain_even_flat() -> None:
    assert stratum_of(2400.0, 120.0, 1e6) == "mountain"


def test_coast_and_other() -> None:
    assert stratum_of(5.0, 30.0, 800.0) == "coast"
    assert stratum_of(150.0, 80.0, 30000.0) == "other"
