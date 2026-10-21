# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Tiles of one imagery layer, addressed in web-mercator XYZ, and pixel windows across them."""

from __future__ import annotations

import io
from collections import OrderedDict
from collections.abc import Iterable, Iterator, Mapping
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import numpy.typing as npt
from PIL import Image

TILE = 256

Rgb = npt.NDArray[np.float32]
"""An image as float32 RGB in [0, 1], shape (height, width, 3)."""


@dataclass(frozen=True, order=True)
class TileKey:
    z: int
    x: int
    y: int

    def parent(self) -> TileKey:
        return TileKey(self.z - 1, self.x // 2, self.y // 2)

    def quadrant(self) -> tuple[int, int]:
        """Which quarter of its parent this tile covers, as (column, row) in {0, 1}."""
        return self.x % 2, self.y % 2

    def __str__(self) -> str:
        return f"{self.z}/{self.x}/{self.y}"


def decode(data: bytes) -> Rgb:
    return np.asarray(Image.open(io.BytesIO(data)).convert("RGB"), np.float32) / 255.0


class TileStore:
    """Encoded tiles in memory, decoded on demand with a bounded cache."""

    def __init__(self, tiles: Mapping[TileKey, bytes], cache: int = 2048) -> None:
        self._tiles = dict(tiles)
        # Decoded tiles are cached as uint8: a quarter of the memory of float32, converted on the way out.
        self._cache: OrderedDict[TileKey, npt.NDArray[np.uint8]] = OrderedDict()
        self._cache_size = cache

    def __len__(self) -> int:
        return len(self._tiles)

    def __contains__(self, key: object) -> bool:
        return key in self._tiles

    def keys(self, z: int | None = None) -> Iterator[TileKey]:
        return (k for k in self._tiles if z is None or k.z == z)

    def zooms(self) -> list[int]:
        return sorted({k.z for k in self._tiles})

    def encoded(self, key: TileKey) -> bytes | None:
        return self._tiles.get(key)

    def rgb(self, key: TileKey) -> Rgb | None:
        hit = self._cache.get(key)
        if hit is not None:
            self._cache.move_to_end(key)
            return to_float(hit)
        data = self._tiles.get(key)
        if data is None:
            return None
        img = np.asarray(Image.open(io.BytesIO(data)).convert("RGB"))
        self._cache[key] = img
        if len(self._cache) > self._cache_size:
            self._cache.popitem(last=False)
        return to_float(img)

    def window(self, z: int, gx: int, gy: int, w: int, h: int) -> Rgb | None:
        """Pixels [gx, gx+w) x [gy, gy+h) of zoom ``z``, in global pixel coordinates (tile * 256 + local).

        ``None`` when any covered tile is missing: a window never pads with invented ground.
        """
        out = np.empty((h, w, 3), np.float32)
        for ty in range(gy // TILE, (gy + h - 1) // TILE + 1):
            for tx in range(gx // TILE, (gx + w - 1) // TILE + 1):
                t = self.rgb(TileKey(z, tx, ty))
                if t is None:
                    return None
                x0, y0 = max(gx, tx * TILE), max(gy, ty * TILE)
                x1, y1 = min(gx + w, (tx + 1) * TILE), min(gy + h, (ty + 1) * TILE)
                out[y0 - gy:y1 - gy, x0 - gx:x1 - gx] = t[y0 - ty * TILE:y1 - ty * TILE,
                                                          x0 - tx * TILE:x1 - tx * TILE]
        return out

    def save(self, root: Path) -> None:
        """One JPEG per tile under ``root/z/x/y.jpg``."""
        for k, data in self._tiles.items():
            p = root / str(k.z) / str(k.x) / f"{k.y}.jpg"
            if not p.exists():
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_bytes(data)

    @classmethod
    def load(cls, root: Path) -> TileStore:
        tiles: dict[TileKey, bytes] = {}
        for p in root.glob("*/*/*.jpg"):
            tiles[TileKey(int(p.parent.parent.name), int(p.parent.name), int(p.stem))] = p.read_bytes()
        return cls(tiles)

    @classmethod
    def merge(cls, layers: Iterable[Mapping[TileKey, bytes]]) -> TileStore:
        """Merge tile sets given newest first: a tile present in several comes from the newest."""
        tiles: dict[TileKey, bytes] = {}
        for layer in layers:
            for k, v in layer.items():
                tiles.setdefault(k, v)
        return cls(tiles)


def to_uint8(img: Rgb) -> npt.NDArray[np.uint8]:
    return np.asarray(np.clip(img * 255.0 + 0.5, 0, 255), dtype=np.uint8)


def to_float(img: npt.NDArray[np.uint8]) -> Rgb:
    return img.astype(np.float32) / 255.0
