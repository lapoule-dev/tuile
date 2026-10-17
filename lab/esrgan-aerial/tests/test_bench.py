# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

from aerial.bench import BLOCKS, RING, SIDE, Block, held_out, lonlat_to_tile, tile_centre


def test_tile_centre_round_trips() -> None:
    for z in (17, 19, 20):
        x, y = lonlat_to_tile(0.5945, 42.7905, z)
        lon, lat = tile_centre(z, x, y)
        assert lonlat_to_tile(lon, lat, z) == (x, y)


def test_block_is_one_tile_two_levels_up() -> None:
    b = BLOCKS[0]
    x0, y0 = b.origin()
    assert x0 % SIDE == 0 and y0 % SIDE == 0
    truth = b.tiles(b.zoom)
    assert len(truth) == (SIDE + 2 * RING) ** 2
    # two levels up the block and its ring span 3 x 3 tiles: the parent tile and one neighbour on each side
    assert len(b.tiles(b.zoom - 2)) == 9


def test_held_out_covers_the_block_and_its_neighbours_not_far_ground() -> None:
    b = Block("t", "ign-bdortho-20cm", "village", 0.5945, 42.7905)
    x0, y0 = b.origin()
    assert held_out(19, x0, y0, [b])
    assert held_out(19, x0 + SIDE + RING, y0, [b])  # a neighbour of the ring: leakage
    assert held_out(19, x0 + 30, y0, [b])  # 30 tiles x 256 x 22 cm = 1.7 km: inside the guard
    assert not held_out(19, x0 + 400, y0, [b])  # ~ 22 km away
