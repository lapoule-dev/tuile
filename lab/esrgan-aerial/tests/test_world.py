# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

from aerial.world import split_of


def test_test_regions_are_never_train() -> None:
    assert split_of(0.5945, 42.7905) == "test"  # Luchon
    assert split_of(7.9091, 46.5935) == "test"  # Lauterbrunnen
    assert split_of(170.1, -43.4) == "test"  # Westland
    assert split_of(-149.5, -17.6) == "test"  # Oceania (Tahiti)
    assert split_of(55.3, 25.2) == "test"  # Middle East


def test_elsewhere_is_train_or_val() -> None:
    assert split_of(6.80, 45.40) in {"train", "val"}  # Vanoise
    assert split_of(-105.6, 40.3) in {"train", "val"}  # Rockies
