# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A run on disk: its spec, its outputs and their index. Read by the GPU side (``infer``) and the laptop (``score``)
alike, so it needs no torch.

Hundreds of thousands of 256 x 256 outputs do not live as files: they are packed in Parquet shards, one row per
output (its PNG bytes and its keys), one row group per tile, so a single output is read without the rest of its shard.
Next to each shard, an index of the same rows without the images: small, it is what tells a run what is done and
where each output lives. Shards can be left in the bucket; the index is always fetched::

    run.json                       the spec
    index/<shard>.parquet          sample, z, x, y, input, family, scale, seconds, shard, group
    shards/<shard>.parquet         the same (without shard and group), and png
"""

from __future__ import annotations

import io
import json
from dataclasses import asdict, dataclass, field
from pathlib import Path

import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.parquet as pq
from PIL import Image

from .tiles import Rgb, to_float, to_uint8

COLUMNS = ["sample", "z", "x", "y", "input", "family", "scale", "seconds"]


@dataclass(frozen=True)
class RunSpec:
    dataset: str
    samples: int | None = 200
    """How many samples, drawn by ``Dataset.sample``; None for every one, in index order."""
    inputs: tuple[str, ...] = ("x2+all", "x4+all", "x8+all", "z18-eq", "z16-eq", "z16")
    families: tuple[str, ...] = ("swinir", "realesrgan")
    seed: int = 17502
    budget: float = 900.0
    """Seconds; no batch starts once it is spent, but every launch does at least one, so a resume always moves."""
    batch: int = 8
    shard_tiles: int = 64
    """Tiles per shard."""
    net_halo: dict[str, int] = field(default_factory=lambda: {"swinir": 16, "realesrgan": 32})
    """Per family, the input pixels kept around the tile for the network (at most the dataset's halo). SwinIR's output
    does not move with 16 (0.2 of a level on average), at a third of the compute; a GAN's texture depends on all the
    context it is given, so Real-ESRGAN keeps 32."""

    def save(self, run_dir: Path) -> None:
        run_dir.mkdir(parents=True, exist_ok=True)
        (run_dir / "run.json").write_text(json.dumps(asdict(self), indent=2))

    @classmethod
    def load(cls, run_dir: Path) -> RunSpec:
        raw = json.loads((run_dir / "run.json").read_text())
        return cls(**{**raw, "inputs": tuple(raw["inputs"]), "families": tuple(raw["families"])})


def results(run_dir: Path) -> pd.DataFrame:
    """Every output written so far, one row each, without the images."""
    parts = sorted((run_dir / "index").glob("*.parquet"))
    if not parts:
        empty: pd.DataFrame = pd.DataFrame(columns=[*COLUMNS, "shard", "group"])
        return empty
    frame: pd.DataFrame = pd.concat([pd.read_parquet(p) for p in parts], ignore_index=True)
    return frame


def encode(img: Rgb) -> bytes:
    buf = io.BytesIO()
    Image.fromarray(to_uint8(img)).save(buf, "PNG")
    return buf.getvalue()


def decode(png: bytes) -> Rgb:
    return to_float(np.asarray(Image.open(io.BytesIO(png)).convert("RGB")))


class ShardWriter:
    """Writes one shard: ``add`` each tile's outputs, then ``close``, which writes the shard's index next to it."""

    def __init__(self, run_dir: Path, name: str) -> None:
        self.name = name
        self.path = run_dir / "shards" / f"{name}.parquet"
        self.index_path = run_dir / "index" / f"{name}.parquet"
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.index_path.parent.mkdir(parents=True, exist_ok=True)
        self._writer: pq.ParquetWriter | None = None
        self._rows: list[dict[str, object]] = []
        self.tiles = 0

    def add(self, rows: list[dict[str, object]], images: list[bytes]) -> None:
        """One tile: its outputs' keys, and their PNG bytes in the same order. One tile, one row group."""
        table = pa.Table.from_pandas(pd.DataFrame.from_records(rows)[COLUMNS], preserve_index=False)
        table = table.append_column("png", pa.array(images, pa.binary()))
        if self._writer is None:
            self._writer = pq.ParquetWriter(self.path, table.schema)
        self._writer.write_table(table)
        self._rows += [{**r, "shard": self.name, "group": self.tiles} for r in rows]
        self.tiles += 1

    def close(self) -> None:
        if self._writer is None:
            return
        self._writer.close()
        pd.DataFrame.from_records(self._rows).to_parquet(self.index_path)


class Run:
    """A run read back: its spec, its index, and any output by (family, input, sample) from the shards on disk."""

    def __init__(self, run_dir: Path) -> None:
        self.dir, self.spec, self.index = run_dir, RunSpec.load(run_dir), results(run_dir)
        ix = self.index
        self._where = {(int(s), str(n), str(f)): (str(h), int(g)) for s, n, f, h, g in
                       zip(ix["sample"], ix["input"], ix["family"], ix["shard"], ix["group"])}
        self._files: dict[str, pq.ParquetFile] = {}

    def local(self) -> pd.DataFrame:
        """The index rows whose shard is on this disk."""
        here = {p.stem for p in (self.dir / "shards").glob("*.parquet")}
        frame: pd.DataFrame = self.index[self.index["shard"].isin(here)]
        return frame

    def output(self, family: str, name: str, sample: int) -> Rgb:
        shard, group = self._where[sample, name, family]
        if shard not in self._files:
            self._files[shard] = pq.ParquetFile(self.dir / "shards" / f"{shard}.parquet")
        t = self._files[shard].read_row_group(group, columns=["sample", "input", "family", "png"]).to_pandas()
        hit = t[(t["sample"] == sample) & (t["input"] == name) & (t["family"] == family)]
        return decode(bytes(hit["png"].iloc[0]))
