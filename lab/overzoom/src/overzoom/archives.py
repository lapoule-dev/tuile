# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Whole PMTiles archives of one zone, fetched through a list of read-only presigned URLs.

The list is a JSON array of ``{key, url, expires_at}``; the zone's ``manifest.json``, when present, lists the
archives oldest first (the last one wins, as the server reads it); they are merged newest first.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

import requests
from pmtiles.reader import MmapSource, Reader, all_tiles

from .tiles import TileKey, TileStore


@dataclass(frozen=True)
class ArchiveInfo:
    key: str
    path: Path
    tiles: int
    min_zoom: int
    max_zoom: int


def download(url: str, dst: Path, chunk: int = 1 << 20) -> Path:
    if dst.exists() and dst.stat().st_size > 0:
        return dst
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_suffix(dst.suffix + ".part")
    with requests.get(url, stream=True, timeout=120) as r:
        r.raise_for_status()
        with open(tmp, "wb") as f:
            f.writelines(r.iter_content(chunk))
    tmp.rename(dst)
    return dst


def fetch_listed(url_list: Path, dest: Path) -> list[Path]:
    """Download every object of the list under ``dest``, keeping their keys as paths. Returns archives newest first."""
    entries: list[dict[str, str]] = json.loads(url_list.read_text())
    for e in entries:
        download(e["url"], dest / e["key"])
    return listed(dest)


def listed(root: Path) -> list[Path]:
    """The archives under ``root``, laid out by key, newest first: the zone manifest's list reversed, then any other."""
    order: list[Path] = []
    for manifest in sorted(root.rglob("manifest.json")):
        order += [root / a["key"] for a in reversed(json.loads(manifest.read_text()).get("archives", []))]
    order += sorted(p for p in root.rglob("*.pmtiles") if p not in order)
    return [p for p in order if p.exists()]


def read_archive(path: Path) -> tuple[dict[TileKey, bytes], ArchiveInfo]:
    tiles: dict[TileKey, bytes] = {}
    with open(path, "rb") as f:
        src = MmapSource(f)
        header = Reader(src).header()
        for (z, x, y), data in all_tiles(src):
            tiles[TileKey(z, x, y)] = data
    info = ArchiveInfo(path.name, path, len(tiles), int(header["min_zoom"]), int(header["max_zoom"]))
    return tiles, info


def load_zone(archives: list[Path]) -> tuple[TileStore, list[ArchiveInfo]]:
    """Merge archives given newest first into one store."""
    layers, infos = [], []
    for p in archives:
        tiles, info = read_archive(p)
        layers.append(tiles)
        infos.append(info)
    return TileStore.merge(layers), infos
