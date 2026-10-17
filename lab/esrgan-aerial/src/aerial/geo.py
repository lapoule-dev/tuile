# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""What kind of ground a place is, from open global layers, so every sample is labelled the same way.

- **coast**: within ``COAST_M`` of the Natural Earth 1:10m coastline (public domain);
- **mountain**: from the Copernicus DEM GLO-90 (free licence, attribution): elevation above ``HIGH_M``, or relief
  (max minus min elevation) above ``RELIEF_M`` within ``RELIEF_RADIUS_M``;
- **land cover**: the shares of ESA WorldCover 2021 v200 classes (CC BY 4.0) over the sample's footprint.

Mountain wins over coast when both hold (a cliff coast in a range is still flown as a mountain); everything else is
``other``. The thresholds are written here and in the dataset's manifest: a sample's stratum is a pure function of its
place.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

import numpy as np
import rasterio
from rasterio.windows import from_bounds
from shapely import STRtree
from shapely.geometry import LineString, Point, shape
from shapely.geometry import box as mkbox

COAST_M = 2500.0  # Natural Earth 1:10m is off by up to ~1-2 km
HIGH_M = 1500.0
RELIEF_M = 500.0
RELIEF_RADIUS_M = 3000.0
EARTH = 6378137.0

WORLDCOVER = {10: "tree", 20: "shrub", 30: "grass", 40: "crop", 50: "built", 60: "bare", 70: "snow", 80: "water",
              90: "wetland", 95: "mangrove", 100: "moss"}
STRATA = ("mountain", "coast", "other")


def deg_lat(m: float) -> float:
    return m / (math.pi * EARTH / 180.0)


def deg_lon(m: float, lat: float) -> float:
    return m / (math.pi * EARTH / 180.0 * max(0.05, math.cos(math.radians(lat))))


class Coastline:
    """Distance to the nearest coastline, in metres (local equirectangular: fine at a few kilometres)."""

    def __init__(self, geojson: Path) -> None:
        feats = json.loads(geojson.read_text())["features"]
        lines: list[LineString] = []
        for f in feats:
            g = shape(f["geometry"])
            lines += list(g.geoms) if hasattr(g, "geoms") else [g]
        self._lines = lines
        self._tree = STRtree(lines)

    def distance_m(self, lon: float, lat: float, search_m: float = 20000.0) -> float:
        dx, dy = deg_lon(search_m, lat), deg_lat(search_m)
        box = mkbox(lon - dx, lat - dy, lon + dx, lat + dy)
        best = math.inf
        sx = math.pi * EARTH / 180.0 * math.cos(math.radians(lat))
        sy = math.pi * EARTH / 180.0
        for i in self._tree.query(box):
            line = self._lines[int(i)]
            coords = np.asarray(line.coords)
            local = LineString(np.c_[(coords[:, 0] - lon) * sx, (coords[:, 1] - lat) * sy])
            best = min(best, local.distance(Point(0.0, 0.0)))
        return best


def _dem_url(lon: float, lat: float) -> str:
    la, lo = math.floor(lat), math.floor(lon)
    ns, ew = ("N" if la >= 0 else "S"), ("E" if lo >= 0 else "W")
    name = f"Copernicus_DSM_COG_30_{ns}{abs(la):02d}_00_{ew}{abs(lo):03d}_00_DEM"
    return f"https://copernicus-dem-90m.s3.amazonaws.com/{name}/{name}.tif"


def _worldcover_url(lon: float, lat: float) -> str:
    la, lo = math.floor(lat / 3) * 3, math.floor(lon / 3) * 3
    ns, ew = ("N" if la >= 0 else "S"), ("E" if lo >= 0 else "W")
    return ("https://esa-worldcover.s3.eu-central-1.amazonaws.com/v200/2021/map/"
            f"ESA_WorldCover_10m_2021_v200_{ns}{abs(la):02d}{ew}{abs(lo):03d}_Map.tif")


@lru_cache(maxsize=64)
def _open(url: str) -> rasterio.DatasetReader | None:
    try:
        return rasterio.open(url)
    except rasterio.errors.RasterioIOError:
        return None  # no tile: open sea for the DEM, outside the map for WorldCover


def relief(lon: float, lat: float) -> tuple[float, float]:
    """(median elevation around the place, max - min elevation within RELIEF_RADIUS_M); (0, 0) over open sea."""
    r = _open(_dem_url(lon, lat))
    if r is None:
        return 0.0, 0.0
    dx, dy = deg_lon(RELIEF_RADIUS_M, lat), deg_lat(RELIEF_RADIUS_M)
    a = r.read(1, window=from_bounds(lon - dx, lat - dy, lon + dx, lat + dy, r.transform), boundless=True,
               fill_value=0).astype(np.float64)
    if a.size == 0:
        return 0.0, 0.0
    h, w = a.shape
    return float(np.median(a)), float(a.max() - a.min())


def land_cover(lon: float, lat: float, half_m: float) -> dict[str, float]:
    """Shares of the WorldCover classes over a square of side ``2 * half_m`` around the place."""
    r = _open(_worldcover_url(lon, lat))
    if r is None:
        return {}
    dx, dy = deg_lon(half_m, lat), deg_lat(half_m)
    a = r.read(1, window=from_bounds(lon - dx, lat - dy, lon + dx, lat + dy, r.transform), boundless=True,
               fill_value=0)
    vals, counts = np.unique(a, return_counts=True)
    total = counts.sum()
    return {WORLDCOVER[int(v)]: float(c / total) for v, c in zip(vals, counts) if int(v) in WORLDCOVER}


@dataclass(frozen=True)
class Place:
    stratum: str
    elevation: float
    relief: float
    coast_m: float
    cover: dict[str, float]

    @property
    def dominant(self) -> str:
        return max(self.cover, key=lambda k: self.cover[k]) if self.cover else "unknown"


def stratum_of(elevation: float, relief_m: float, coast_m: float) -> str:
    if elevation >= HIGH_M or relief_m >= RELIEF_M:
        return "mountain"
    if coast_m <= COAST_M:
        return "coast"
    return "other"


def classify(lon: float, lat: float, coast: Coastline, half_m: float = 60.0) -> Place:
    e, rel = relief(lon, lat)
    d = coast.distance_m(lon, lat)
    return Place(stratum_of(e, rel, d), e, rel, d, land_cover(lon, lat, half_m))
