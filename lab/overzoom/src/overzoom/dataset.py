# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The dataset: an index of clean tiles of the deepest level, and for each, inputs from which to get back to it.

Nothing image-sized is stored. The dataset is the zone's archives plus an index (one row per tile), and every input
is rebuilt on demand, deterministically: damages draw from a generator seeded by the tile, an ancestor's colour
equalisation is recomputed from the pixels. Twenty thousand comparisons would be gigabytes pre-rendered; this way
they cost the archives and a table.

Two families of inputs, on the same tiles, at x2, x4 and x8:

- **synthetic** — the tile damaged by us (``degrade.CATALOGUE``): no acquisition gap at all, the control;
- **levels** — the real ancestor 1, 2 or 3 zooms up (named after its zoom: ``z18``, ``z17``, ``z16`` for z19), in two
  inputs: as served (``z18``: a method has to correct the tint as well as the detail) and brought to the child's
  colours (``z18-eq``: detail only). Each pair is checked once, when the index is built, and classed ``same``,
  ``tinted`` or ``other`` (``z18_class`` etc., see ``pairs``). Measures are read on ``same`` and ``tinted``; the
  ``other`` ones stay in the index, to set the thresholds by eye.

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
from .pairs import OTHER, SAME, TINTED, PairCheck, PairPolicy, check_pair
from .tiles import TILE, Rgb, TileKey, TileStore

EQUALISED = "-eq"
"""Suffix of a level input brought to the child's colours."""


@dataclass(frozen=True)
class DatasetSpec:
    zoom: int = 19
    halo: int = 32
    """Halo of real neighbours around every input, in input pixels."""
    seed: int = 17501
    min_texture: float = 0.01
    """Mean absolute vertical luminance step under which a tile is too flat to tell methods apart."""
    damages: tuple[Damage, ...] = field(default=CATALOGUE)
    depths: tuple[int, ...] = (1, 2, 3)
    """Ancestors used as real inputs: 1 is the parent (x2), 2 the grandparent (x4), 3 (x8)."""
    policy: PairPolicy = field(default_factory=PairPolicy)

    def level(self, depth: int) -> str:
        return f"z{self.zoom - depth}"

    @property
    def levels(self) -> list[str]:
        return [self.level(d) for d in self.depths]

    @property
    def inputs(self) -> list[str]:
        return [d.name for d in self.damages] + [n for lv in self.levels for n in (lv, lv + EQUALISED)]

    def depth(self, name: str) -> int | None:
        """The ancestor depth of a level input (served or equalised), or None for a synthetic one."""
        base = name.removesuffix(EQUALISED)
        return next((d for d in self.depths if self.level(d) == base), None)

    def damage(self, name: str) -> Damage:
        return next(d for d in self.damages if d.name == name)

    def scale(self, name: str) -> int:
        depth = self.depth(name)
        return 2**depth if depth is not None else self.damage(name).scale

    @property
    def max_scale(self) -> int:
        return int(max([d.scale for d in self.damages] + [2**d for d in self.depths]))


def texture(img: Rgb) -> float:
    y = img @ np.array([0.299, 0.587, 0.114], np.float32)
    return float(np.abs(np.diff(y, axis=0)).mean())


def tile_rng(seed: int, key: TileKey, damage: str) -> np.random.Generator:
    return np.random.default_rng([seed, key.z, key.x, key.y, zlib.crc32(damage.encode())])


def clean_window(store: TileStore, key: TileKey, margin: int) -> Rgb | None:
    """The tile with ``margin`` pixels of its real neighbours on every side, at its own zoom."""
    return store.window(key.z, key.x * TILE - margin, key.y * TILE - margin, TILE + 2 * margin, TILE + 2 * margin)


def ancestor_window(store: TileStore, key: TileKey, depth: int, halo: int) -> Rgb | None:
    """What the ancestor ``depth`` zooms up shows of ``key`` (256 / 2^depth pixels a side), with ``halo`` around."""
    part = TILE >> depth
    return store.window(key.z - depth, key.x * part - halo, key.y * part - halo, part + 2 * halo, part + 2 * halo)


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

    def check(self, i: int, depth: int) -> tuple[Rgb, PairCheck]:
        """The ancestor's view of the tile as served, halo included, and its check against the tile."""
        a = ancestor_window(self.store, self.key(i), depth, self.spec.halo)
        assert a is not None, "the index only holds tiles whose ancestors' context exists"
        h, part = self.spec.halo, TILE >> depth
        return a, check_pair(a[h:h + part, h:h + part], self.truth(i), self.spec.policy)

    def ancestor(self, i: int, depth: int, equalised: bool = True) -> Rgb:
        a, check = self.check(i, depth)
        return check.equalisation.apply(a) if equalised else a

    def input(self, i: int, name: str) -> Rgb:
        """Input ``name`` of sample ``i``, halo included: (256 / scale + 2 * halo) pixels a side."""
        depth = self.spec.depth(name)
        if depth is not None:
            return self.ancestor(i, depth, equalised=name.endswith(EQUALISED))
        d, key, h = self.spec.damage(name), self.key(i), self.spec.halo
        w = clean_window(self.store, key, h * d.scale)
        assert w is not None, "the index only holds tiles whose neighbours exist"
        return d.apply(w, tile_rng(self.spec.seed, key, name), int(self.index.iloc[i].quality))

    def crop(self, img: Rgb, scale: int = 1) -> Rgb:
        """The tile itself out of a haloed image at ``scale`` times the input resolution."""
        m = self.spec.halo * scale
        return img[m:img.shape[0] - m, m:img.shape[1] - m]

    def kind(self, i: int, name: str) -> str | None:
        """The class of a level input's pair (``same``, ``tinted``, ``other``); None for a synthetic input."""
        depth = self.spec.depth(name)
        return None if depth is None else str(self.index.iloc[i][f"{self.spec.level(depth)}_class"])

    def valid(self, i: int, name: str) -> bool:
        """A synthetic input is always valid; a level one unless its ancestor is another acquisition."""
        return self.kind(i, name) != OTHER

    def comparisons(self, inputs: list[str] | None = None, valid_only: bool = False) -> Iterator[tuple[int, str]]:
        """Every (sample, input) pair, sample-major."""
        names = inputs or self.spec.inputs
        for i in range(len(self)):
            for name in names:
                if not valid_only or self.valid(i, name):
                    yield i, name

    def sample(self, n: int, seed: int) -> list[int]:
        """``n`` samples drawn in proportion from every stratum (each level's pair class, texture tercile), in a
        random order: any first part of the list is itself about as balanced as the whole."""
        rng = np.random.default_rng(seed)
        strata = self.index[[f"{lv}_class" for lv in self.spec.levels]].copy()
        strata["texture"] = pd.qcut(self.index.texture, 3, labels=False)
        picked: list[int] = []
        for _, pos in sorted(strata.groupby(list(strata.columns)).indices.items(), key=lambda kv: str(kv[0])):
            k = min(len(pos), max(1, round(n * len(pos) / len(self))))
            picked += [int(i) for i in rng.choice(pos, k, replace=False)]
        return [int(i) for i in rng.permutation(picked)[:n]]

    def save(self, root: Path, sources: list[str]) -> None:
        root.mkdir(parents=True, exist_ok=True)
        self.index.to_parquet(root / "index.parquet")
        manifest = {
            "created": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "spec": asdict(self.spec),
            "sources": sources,
            "samples": len(self.index),
            "classes": {lv: {k: int((self.index[f"{lv}_class"] == k).sum()) for k in (SAME, TINTED, OTHER)}
                        for lv in self.spec.levels},
        }
        (root / "manifest.json").write_text(json.dumps(manifest, indent=2))

    @staticmethod
    def load_spec(root: Path) -> tuple[DatasetSpec, list[str]]:
        manifest = json.loads((root / "manifest.json").read_text())
        s = dict(manifest["spec"])
        damages = tuple(Damage(**d) for d in s.pop("damages"))
        policy = PairPolicy(**s.pop("policy"))
        depths = tuple(s.pop("depths"))
        return DatasetSpec(**s, damages=damages, depths=depths, policy=policy), list(manifest["sources"])

    @classmethod
    def load(cls, root: Path, store: TileStore) -> Dataset:
        spec, _ = cls.load_spec(root)
        return cls(store, spec, pd.read_parquet(root / "index.parquet"))


def build_index(store: TileStore, spec: DatasetSpec, limit: int | None = None,
                progress: bool = True) -> pd.DataFrame:
    """Every textured tile of ``spec.zoom`` with its neighbours and all its ancestors' context, with its pair checks.

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
        ancestors = {d: ancestor_window(store, key, d, spec.halo) for d in spec.depths}
        if any(a is None for a in ancestors.values()):
            continue
        tex = texture(tile)
        if tex < spec.min_texture:
            continue
        row: dict[str, object] = {"z": key.z, "x": key.x, "y": key.y, "quality": jpeg_quality(data), "texture": tex}
        h = spec.halo
        for d, a in ancestors.items():
            assert a is not None
            part = TILE >> d
            c = check_pair(a[h:h + part, h:h + part], tile, spec.policy)
            lv = spec.level(d)
            row |= {f"{lv}_class": c.kind, f"{lv}_structure": c.structure, f"{lv}_cast_raw": c.cast_raw,
                    f"{lv}_gain_max": float(np.abs(np.log(c.equalisation.gain)).max()),
                    f"{lv}_ssim_y_raw": c.ssim_y_raw, f"{lv}_delta_e_raw": c.delta_e_raw,
                    f"{lv}_ssim_y": c.ssim_y, f"{lv}_delta_e": c.delta_e}
        rows.append(row)
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
