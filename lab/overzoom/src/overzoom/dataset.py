# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The dataset: an index of clean tiles of the deepest level, and for each, inputs from which to get back to it.

Nothing image-sized is stored. The dataset is the zone's archives plus an index (one row per tile), and every input
is rebuilt on demand, deterministically: damages draw from a generator seeded by the tile, the parent's colour
equalisation is recomputed from the pixels. Twenty thousand comparisons would be fifteen gigabytes pre-rendered;
this way they cost the archives and a table.

Two families of inputs, on the same tiles:

- **synthetic** — the tile damaged by us (``degrade.CATALOGUE``): no acquisition gap at all, the control;
- **levels** — the real parent, one zoom up, brought to the child's colours (``pairs``). Each pair is checked once,
  when the index is built; ``level_valid`` says whether parent and child are one acquisition. Measures are read on
  the valid ones; the rejected ones stay in the index, to set the threshold by eye.

On disk (one directory, synced to the bucket as is): ``manifest.json`` (spec, sources, counts) and ``index.parquet``.
"""

from __future__ import annotations

import json
import time
import zlib
from collections.abc import Iterator
from dataclasses import asdict, dataclass, field
from pathlib import Path

import numpy as np
import pandas as pd

from .degrade import CATALOGUE, Damage, jpeg_quality
from .pairs import PairPolicy, check_pair
from .tiles import TILE, Rgb, TileKey, TileStore

LEVELS = "levels"


@dataclass(frozen=True)
class DatasetSpec:
    zoom: int = 19
    halo: int = 32
    """Halo of real neighbours around every input, in input pixels."""
    seed: int = 17501
    min_texture: float = 0.01
    """Mean absolute vertical luminance step under which a tile is too flat to tell methods apart."""
    damages: tuple[Damage, ...] = field(default=CATALOGUE)
    policy: PairPolicy = field(default_factory=PairPolicy)

    @property
    def inputs(self) -> list[str]:
        return [d.name for d in self.damages] + [LEVELS]

    def damage(self, name: str) -> Damage:
        return next(d for d in self.damages if d.name == name)

    def scale(self, name: str) -> int:
        return 2 if name == LEVELS else self.damage(name).scale


def texture(img: Rgb) -> float:
    y = img @ np.array([0.299, 0.587, 0.114], np.float32)
    return float(np.abs(np.diff(y, axis=0)).mean())


def tile_rng(seed: int, key: TileKey, damage: str) -> np.random.Generator:
    return np.random.default_rng([seed, key.z, key.x, key.y, zlib.crc32(damage.encode())])


def clean_window(store: TileStore, key: TileKey, margin: int) -> Rgb | None:
    """The tile with ``margin`` pixels of its real neighbours on every side, at its own zoom."""
    return store.window(key.z, key.x * TILE - margin, key.y * TILE - margin, TILE + 2 * margin, TILE + 2 * margin)


def parent_window(store: TileStore, key: TileKey, halo: int) -> Rgb | None:
    """What the parent shows of ``key`` (half its size, at the parent's zoom), with ``halo`` parent pixels around."""
    half = TILE // 2
    return store.window(key.z - 1, key.x * half - halo, key.y * half - halo, half + 2 * halo, half + 2 * halo)


class Dataset:
    """The index over a tile store, and the inputs rebuilt from it."""

    def __init__(self, store: TileStore, spec: DatasetSpec, index: pd.DataFrame) -> None:
        self.store, self.spec, self.index = store, spec, index.reset_index(drop=True)

    def __len__(self) -> int:
        return len(self.index)

    def key(self, i: int) -> TileKey:
        r = self.index.iloc[i]
        return TileKey(int(r.z), int(r.x), int(r.y))

    def truth(self, i: int) -> Rgb:
        t = self.store.rgb(self.key(i))
        assert t is not None, "the index only holds tiles of the store"
        return t

    def parent(self, i: int, equalised: bool = True) -> Rgb:
        """The parent's view of the tile, halo included; brought to the tile's colours unless ``equalised=False``."""
        key = self.key(i)
        p = parent_window(self.store, key, self.spec.halo)
        assert p is not None, "the index only holds tiles whose parent context exists"
        if not equalised:
            return p
        h = self.spec.halo
        return check_pair(p[h:h + TILE // 2, h:h + TILE // 2], self.truth(i), self.spec.policy).equalisation.apply(p)

    def input(self, i: int, name: str) -> Rgb:
        """Input ``name`` of sample ``i``, halo included: (256 / scale + 2 * halo) pixels a side."""
        if name == LEVELS:
            return self.parent(i)
        d, key, h = self.spec.damage(name), self.key(i), self.spec.halo
        w = clean_window(self.store, key, h * d.scale)
        assert w is not None, "the index only holds tiles whose neighbours exist"
        return d.apply(w, tile_rng(self.spec.seed, key, name), int(self.index.iloc[i].quality))

    def crop(self, img: Rgb, scale: int = 1) -> Rgb:
        """The tile itself out of a haloed image at ``scale`` times the input resolution."""
        m = self.spec.halo * scale
        return img[m:img.shape[0] - m, m:img.shape[1] - m]

    def comparisons(self, inputs: list[str] | None = None, valid_levels_only: bool = False
                    ) -> Iterator[tuple[int, str]]:
        """Every (sample, input) pair, sample-major."""
        names = inputs or self.spec.inputs
        for i in range(len(self)):
            for name in names:
                if name == LEVELS and valid_levels_only and not bool(self.index.iloc[i].level_valid):
                    continue
                yield i, name

    def save(self, root: Path, sources: list[str]) -> None:
        root.mkdir(parents=True, exist_ok=True)
        self.index.to_parquet(root / "index.parquet")
        manifest = {
            "created": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "spec": asdict(self.spec),
            "sources": sources,
            "samples": len(self.index),
            "levels_valid": int(self.index["level_valid"].sum()),
        }
        (root / "manifest.json").write_text(json.dumps(manifest, indent=2))

    @staticmethod
    def load_spec(root: Path) -> tuple[DatasetSpec, list[str]]:
        manifest = json.loads((root / "manifest.json").read_text())
        s = dict(manifest["spec"])
        damages = tuple(Damage(**d) for d in s.pop("damages"))
        policy = PairPolicy(**s.pop("policy"))
        return DatasetSpec(**s, damages=damages, policy=policy), list(manifest["sources"])

    @classmethod
    def load(cls, root: Path, store: TileStore) -> Dataset:
        spec, _ = cls.load_spec(root)
        return cls(store, spec, pd.read_parquet(root / "index.parquet"))


def build_index(store: TileStore, spec: DatasetSpec, limit: int | None = None,
                progress: bool = True) -> pd.DataFrame:
    """Every tile of ``spec.zoom`` that is textured and has its neighbours and its parent context, with its pair check.

    Tiles are visited in row order, so neighbouring windows hit the decode cache.
    """
    keys = sorted(store.keys(spec.zoom), key=lambda k: (k.y, k.x))
    margin = spec.halo * max(d.scale for d in spec.damages)
    rows: list[dict[str, object]] = []
    t0 = time.time()
    for n, key in enumerate(keys):
        if limit is not None and len(rows) >= limit:
            break
        if progress and n and n % 2000 == 0:
            print(f"{n}/{len(keys)} tiles visited, {len(rows)} kept, {time.time() - t0:.0f} s", flush=True)
        tile, data = store.rgb(key), store.encoded(key)
        if tile is None or data is None or clean_window(store, key, margin) is None:
            continue
        parent = parent_window(store, key, spec.halo)
        if parent is None:
            continue
        tex = texture(tile)
        if tex < spec.min_texture:
            continue
        h = spec.halo
        check = check_pair(parent[h:h + TILE // 2, h:h + TILE // 2], tile, spec.policy)
        rows.append({
            "z": key.z, "x": key.x, "y": key.y, "quality": jpeg_quality(data), "texture": tex,
            "level_ssim_y_raw": check.ssim_y_raw, "level_delta_e_raw": check.delta_e_raw,
            "level_ssim_y": check.ssim_y, "level_delta_e": check.delta_e, "level_valid": check.valid,
        })
    frame: pd.DataFrame = pd.DataFrame.from_records(rows)
    return frame


def pick_block(store: TileStore, spec: DatasetSpec, n: int = 3, tries: int = 2000) -> tuple[TileKey, Rgb]:
    """The most textured n x n block of the deepest level whose tiles and halo all exist, among ``tries`` origins.

    The block is what gets enlarged past the deepest level, where no truth exists.
    """
    m = spec.halo
    keys = sorted(store.keys(spec.zoom))
    order = np.random.default_rng(spec.seed + 1).permutation(len(keys))[:tries]
    best: tuple[float, TileKey, Rgb] | None = None
    for i in order:
        key = keys[int(i)]
        w = store.window(key.z, key.x * TILE - m, key.y * TILE - m, n * TILE + 2 * m, n * TILE + 2 * m)
        if w is None:
            continue
        t = texture(w)
        if best is None or t > best[0]:
            best = (t, key, w)
    if best is None:
        raise ValueError(f"no complete {n}x{n} block at zoom {spec.zoom}")
    return best[1], best[2]
