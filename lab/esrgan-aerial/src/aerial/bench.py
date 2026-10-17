# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A small held-out benchmark of true high-resolution ground, from open orthophotos, over the zones we fly.

Each **block** is a 4 x 4 square of tiles at the source's ``truth_zoom`` (the truth), with a ring of one tile around
it (real neighbours, for a network's halo and for 3 x 3 mosaics across tile borders), and the source's own coarser
levels over the same ground (its real pyramid: the input of a full-reference x2 or x4, same acquisition, same
processing — never another service's tiles).

Blocks are **held out**: ``held_out`` says whether a tile falls within ``GUARD_M`` of any of them, so that no future
training set can sample them or their neighbours. Each block has a stratum (``village``, ``forest``, ``rock``,
``meadow``) chosen for what the contact sheets must show: roads, crosswalks, cars and roofs; canopy; scree and cliffs;
fields.
"""

from __future__ import annotations

import json
import math
import time
from collections.abc import Iterable
from dataclasses import asdict, dataclass
from pathlib import Path

import requests

from .sources import SOURCES, OrthoSource

EARTH = 6378137.0
GUARD_M = 2000.0
"""Training tiles closer than this to any benchmark block are excluded (no neighbour leakage)."""
SIDE = 4
"""Truth tiles per block side."""
RING = 1
"""Tiles of real neighbours around the truth, per side."""


def lonlat_to_tile(lon: float, lat: float, z: int) -> tuple[int, int]:
    n = 2**z
    x = int((lon + 180.0) / 360.0 * n)
    y = int((1.0 - math.asinh(math.tan(math.radians(lat))) / math.pi) / 2.0 * n)
    return x, y


def tile_centre(z: int, x: int, y: int) -> tuple[float, float]:
    """(lon, lat) of a tile's centre."""
    n = 2**z
    lon = (x + 0.5) / n * 360.0 - 180.0
    lat = math.degrees(math.atan(math.sinh(math.pi * (1.0 - 2.0 * (y + 0.5) / n))))
    return lon, lat


def ground_m(lon_a: float, lat_a: float, lon_b: float, lat_b: float) -> float:
    """Great-circle distance in metres (haversine, spherical Earth): enough for a guard band."""
    p1, p2 = math.radians(lat_a), math.radians(lat_b)
    dp, dl = p2 - p1, math.radians(lon_b - lon_a)
    h = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2.0 * EARTH * math.asin(math.sqrt(h))


@dataclass(frozen=True)
class Block:
    name: str
    source: str
    stratum: str
    lon: float
    lat: float
    truth_zoom: int | None = None
    """Overrides the source's (SWISSIMAGE is 10 cm only in the valleys)."""

    @property
    def ortho(self) -> OrthoSource:
        return SOURCES[self.source]

    @property
    def zoom(self) -> int:
        return self.truth_zoom if self.truth_zoom is not None else self.ortho.truth_zoom

    def origin(self) -> tuple[int, int]:
        """Top-left truth tile, aligned on SIDE so the block is exactly one tile two levels up."""
        x, y = lonlat_to_tile(self.lon, self.lat, self.zoom)
        return x // SIDE * SIDE, y // SIDE * SIDE

    def tiles(self, z: int) -> list[tuple[int, int, int]]:
        """Every tile of zoom ``z`` (truth zoom or coarser) covering the truth and its ring."""
        x0, y0 = self.origin()
        d = self.zoom - z
        lo_x, lo_y = (x0 - RING) >> d, (y0 - RING) >> d
        hi_x, hi_y = (x0 + SIDE + RING - 1) >> d, (y0 + SIDE + RING - 1) >> d
        return [(z, x, y) for y in range(lo_y, hi_y + 1) for x in range(lo_x, hi_x + 1)]


# Mountains: Luchon and the central Pyrenees, both sides of the border; the Alps where our tracks fly (Chamonix and
# the Mer de Glace, Annecy) and the Bernese Oberland's valleys, the only open 10 cm ground found over mountains.
# Coasts: Crozon (Brittany) and Mallorca. Every one lies in a test region of ``sites``.
BLOCKS: tuple[Block, ...] = (
    Block("luchon-centre", "ign-bdortho-20cm", "village", 0.5945, 42.7905),
    Block("luchon-forest", "ign-bdortho-20cm", "forest", 0.6150, 42.7750),
    Block("luchon-meadow", "ign-bdortho-20cm", "meadow", 0.6020, 42.8180),
    Block("oo-rock", "ign-bdortho-20cm", "snow", 0.5000, 42.7050),
    Block("chamonix-centre", "ign-bdortho-20cm", "village", 6.8694, 45.9237),
    Block("chamonix-rock", "ign-bdortho-20cm", "snow", 6.9020, 45.8930),
    Block("talloires", "ign-bdortho-20cm", "village", 6.2120, 45.8410),
    Block("forclaz-forest", "ign-bdortho-20cm", "forest", 6.2470, 45.8130),
    Block("benasque-centre", "ign-es-pnoa-ma", "village", 0.5230, 42.6040),
    Block("aneto-rock", "ign-es-pnoa-ma", "rock", 0.6550, 42.6300),
    Block("interlaken-centre", "swisstopo-swissimage", "village", 7.8632, 46.6863),
    Block("lauterbrunnen", "swisstopo-swissimage", "meadow", 7.9091, 46.5935),
    Block("harder-forest", "swisstopo-swissimage", "forest", 7.8700, 46.6950),
    Block("lauterbrunnen-cliff", "swisstopo-swissimage", "treeline", 7.9180, 46.5990),
    Block("mer-de-glace", "ign-bdortho-20cm", "glacier", 6.9300, 45.9000),
    # coasts: Brittany cliffs and harbour, Mallorca harbour, cliffs and beach
    Block("dinan-cliff", "ign-bdortho-20cm", "rocky-shore", -4.5638, 48.2306),
    Block("morgat-beach", "ign-bdortho-20cm", "beach", -4.5000, 48.2250),
    Block("camaret-harbour", "ign-bdortho-20cm", "harbour", -4.5900, 48.2790),
    Block("soller-harbour", "ign-es-pnoa-ma", "harbour", 2.6930, 39.7970),
    Block("formentor-cliff", "ign-es-pnoa-ma", "cliff", 3.2050, 39.9580),
    Block("alcudia-beach", "ign-es-pnoa-ma", "beach", 3.1300, 39.8400),
)


def held_out(z: int, x: int, y: int, blocks: Iterable[Block] = BLOCKS, guard_m: float = GUARD_M) -> bool:
    """Whether a tile is within ``guard_m`` of a benchmark block's centre (plus the block's own half-diagonal)."""
    lon, lat = tile_centre(z, x, y)
    for b in blocks:
        x0, y0 = b.origin()
        blon, blat = tile_centre(b.zoom, x0 + SIDE // 2, y0 + SIDE // 2)
        half = (SIDE / 2 + RING) * 2 * math.pi * EARTH * math.cos(math.radians(blat)) / 2**b.zoom * math.sqrt(2)
        if ground_m(lon, lat, blon, blat) <= guard_m + half:
            return True
    return False


def fetch(block: Block, root: Path, levels: tuple[int, ...] = (0, 1, 2), pause: float = 0.15,
          session: requests.Session | None = None) -> dict[str, int]:
    """Download the block's truth (and ring) and its source's own coarser levels ``truth_zoom - d`` for d in
    ``levels``, under ``root/<block>/<z>/<x>/<y>.jpg``. Politely: one request at a time, ``pause`` between them.
    A 404 is recorded, never filled."""
    s = session or requests.Session()
    s.headers["User-Agent"] = "tuile-image-lab/0.1 (research benchmark; laurent@lapoule.dev)"
    counts = {"fetched": 0, "cached": 0, "missing": 0}
    for d in levels:
        for z, x, y in block.tiles(block.zoom - d):
            p = root / block.name / str(z) / str(x) / f"{y}.jpg"
            if p.exists():
                counts["cached"] += 1
                continue
            r = s.get(block.ortho.url.format(z=z, x=x, y=y), timeout=60)
            time.sleep(pause)
            if r.status_code == 404:
                counts["missing"] += 1
                continue
            r.raise_for_status()
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(r.content)
            counts["fetched"] += 1
    return counts


def write_manifest(root: Path, blocks: Iterable[Block] = BLOCKS) -> None:
    sources = {b.source for b in blocks}
    manifest = {
        "created": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "side": SIDE, "ring": RING, "guard_m": GUARD_M,
        "blocks": [asdict(b) | {"zoom": b.zoom, "origin": b.origin()} for b in blocks],
        "sources": {n: asdict(SOURCES[n]) for n in sorted(sources)},
    }
    root.mkdir(parents=True, exist_ok=True)
    (root / "manifest.json").write_text(json.dumps(manifest, indent=1))
