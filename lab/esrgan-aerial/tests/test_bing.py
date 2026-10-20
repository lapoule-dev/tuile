# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

from aerial.bing import is_no_tile, quadkey


def test_quadkey_matches_tuiles_connector() -> None:
    # the same cases as tuile-bing's own tests
    assert quadkey(0, 0, 1) == "0"
    assert quadkey(1, 0, 1) == "1"
    assert quadkey(0, 1, 1) == "2"
    assert quadkey(1, 1, 1) == "3"
    assert quadkey(3, 5, 3) == "213"


def test_no_tile_is_the_mark_or_an_empty_body() -> None:
    assert is_no_tile("no-tile", b"")
    assert is_no_tile(" No-Tile ", b"\x89PNG a pale square")
    assert is_no_tile(None, b"")
    assert not is_no_tile(None, b"\xff\xd8\xff a photograph")
    assert not is_no_tile("tile", b"\xff\xd8\xff a photograph")
