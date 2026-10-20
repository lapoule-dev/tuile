# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Where on the planet the commercial layer is sampled, and which split a place belongs to.

Places are drawn, in this order of weight:

- **mountains**: uniformly inside the boxes of ``RANGES`` (every continent: Alps, Pyrenees, Rockies, Andes, Himalaya
  foothills, Southern Alps, Japanese Alps and more), weighted by the box's area capped so small ranges count;
- **coasts**: on vertices of the Natural Earth 1:10m coastline, every continent in proportion to its coastline,
  shifted by up to 1 km either way so that beaches, cliffs, harbours and shallow water all come up;
- **settlements and biomes the random draws would miss** (``CURATED``): dense Asian cities, informal settlements of
  Africa, Latin America and South Asia, rice terraces, US suburbs, tropical forest, desert, tundra;
- **the rest of the land**: uniform on the sphere between 56 S and 72 N, kept when the land cover map has ground.

The measured stratum (``geo.classify``) is what the sample records, not the reason it was drawn. A place inside a
``TEST_BOXES`` region is test, never train; the val share is drawn per 1-degree cell.
"""

from __future__ import annotations

import hashlib
import json
import math
import random
from dataclasses import dataclass
from pathlib import Path

RANGES: dict[str, tuple[float, float, float, float]] = {
    # lon0, lat0, lon1, lat1
    "alps": (5.5, 44.0, 16.0, 47.8),
    "pyrenees": (-1.8, 42.2, 2.8, 43.2),
    "massif-central-jura-vosges": (2.3, 44.8, 7.5, 48.2),
    "corsica-sardinia": (8.4, 39.5, 9.6, 42.9),
    "apennines": (11.0, 41.0, 15.5, 44.5),
    "carpathians-tatras": (19.0, 45.0, 26.5, 49.6),
    "balkans": (18.5, 41.0, 23.5, 44.0),
    "scandinavian": (6.0, 60.0, 18.0, 69.0),
    "caucasus": (40.0, 42.0, 48.0, 44.0),
    "atlas": (-8.5, 30.5, -3.5, 33.0),
    "ethiopian-highlands": (36.5, 8.5, 40.5, 14.0),
    "east-african-volcanoes": (35.0, -4.0, 38.0, 0.5),
    "drakensberg": (28.5, -30.5, 30.5, -28.0),
    "himalaya-foothills": (78.0, 27.0, 92.0, 31.5),
    "tian-shan-pamir": (69.0, 38.5, 80.0, 43.0),
    "japanese-alps": (137.0, 35.3, 138.6, 36.9),
    "taiwan-central": (120.8, 23.0, 121.5, 24.6),
    "rockies": (-116.0, 37.0, -104.5, 50.5),
    "sierra-nevada-us": (-120.5, 36.0, -118.0, 39.0),
    "cascades": (-122.5, 44.5, -120.5, 49.0),
    "alaska-range": (-152.0, 61.5, -144.0, 64.0),
    "appalachians": (-84.5, 35.0, -77.5, 39.5),
    "mexican-sierra": (-105.0, 18.5, -97.0, 24.0),
    "andes-north": (-78.5, -5.0, -72.0, 7.0),
    "andes-central": (-72.0, -24.0, -65.0, -10.0),
    "andes-south": (-72.5, -46.0, -69.5, -30.0),
    "patagonia": (-73.8, -51.5, -71.0, -46.0),
    "southern-alps-nz": (168.0, -45.5, 172.5, -42.0),
    "australian-alps": (146.5, -37.5, 149.0, -35.5),
}

CURATED: dict[str, tuple[float, float, float]] = {
    # lon, lat, radius km
    "mumbai-dharavi": (72.855, 19.040, 3), "dhaka": (90.41, 23.81, 6), "tokyo": (139.70, 35.68, 10),
    "hanoi": (105.85, 21.03, 6), "jakarta": (106.83, -6.18, 8), "cairo": (31.24, 30.04, 8),
    "lagos-makoko": (3.39, 6.49, 4), "nairobi-kibera": (36.79, -1.31, 3), "kinshasa": (15.31, -4.33, 8),
    "rio-rocinha": (-43.25, -22.99, 3), "lima-hills": (-77.02, -12.00, 10), "port-au-prince": (-72.33, 18.55, 5),
    "mexico-city-edge": (-99.05, 19.35, 10), "phoenix-suburbs": (-112.00, 33.55, 15),
    "atlanta-suburbs": (-84.20, 33.90, 15), "banaue-terraces": (121.06, 16.92, 6),
    "yuanyang-terraces": (102.77, 23.12, 8), "bali-jatiluwih": (115.12, -8.37, 6), "sapa": (103.84, 22.34, 8),
    "amazon-manaus": (-60.0, -3.1, 40), "congo-forest": (21.0, 0.5, 60), "borneo": (114.5, 1.0, 60),
    "sahara-erg": (8.0, 26.5, 80), "atacama": (-69.5, -23.5, 60), "namib": (15.0, -24.5, 50),
    "mongolian-steppe": (104.0, 47.0, 80), "kazakh-steppe": (70.0, 50.0, 80), "pampas": (-61.0, -35.0, 60),
    "tundra-lapland": (25.0, 69.0, 60), "tundra-yukon": (-137.0, 64.0, 60), "svalbard-edge": (15.6, 78.2, 15),
    "iceland": (-19.0, 64.8, 60), "greenland-coast": (-51.7, 64.2, 20),
}

TEST_BOXES: dict[str, tuple[float, float, float, float]] = {
    "central-pyrenees": (0.2, 42.4, 1.0, 42.95),
    "bernese-oberland": (7.6, 46.4, 8.3, 46.8),
    "mont-blanc-annecy": (6.0, 45.7, 7.1, 46.1),
    "nz-westland": (168.5, -44.5, 171.5, -42.0),
    "crozon": (-4.7, 48.1, -4.3, 48.4),
    "mallorca": (2.3, 39.2, 3.5, 40.0),
    "japanese-alps-north": (137.4, 36.3, 137.9, 36.9),
    "patagonia": (-73.8, -51.5, -71.0, -46.0),
}
VAL_SHARE = 0.08


def region_of(lon: float, lat: float) -> str:
    """A coarse continent label, the same for every source (OpenAerialMap's split uses it too)."""
    if lon <= -170 or lon > 160 or (lon > 110 and lat < -10) or (lon < -82 and lat < 5) or (lon < -140 and lat < 30):
        return "oceania"  # the Pacific, east of the date line included (Polynesia, Hawaii)
    if lat > -60 and -170 < lon < -30:
        return "americas-N" if lat > 15 else ("caribbean-centralam" if lat > 5 else "americas-S")
    if 34 < lon < 63 and 12 < lat < 38:
        return "middle-east"
    if -20 < lon < 55 and lat < 37:
        return "africa"
    if -25 < lon < 45 and lat >= 35:
        return "europe"
    if 55 <= lon <= 150 and lat > -12:
        return "asia"
    return "other"


def in_test_box(lon: float, lat: float) -> bool:
    return any(a <= lon <= c and b <= lat <= d for (a, b, c, d) in TEST_BOXES.values())


def split_of(lon: float, lat: float) -> str:
    if in_test_box(lon, lat):
        return "test"
    if region_of(lon, lat) in {"oceania", "caribbean-centralam", "middle-east"}:
        return "test"
    cell = (int(lon // 1), int(lat // 1))
    return "val" if int(hashlib.sha1(f"{cell}".encode()).hexdigest(), 16) % 1000 < VAL_SHARE * 1000 else "train"


@dataclass(frozen=True)
class Place:
    lon: float
    lat: float
    drawn_as: str
    group: str


def _uniform_in_box(rng: random.Random, box: tuple[float, float, float, float]) -> tuple[float, float]:
    a, b, c, d = box
    # uniform in area: sin(lat) uniform
    s = rng.uniform(math.sin(math.radians(b)), math.sin(math.radians(d)))
    return rng.uniform(a, c), math.degrees(math.asin(s))


def _jitter(rng: random.Random, lon: float, lat: float, km: float) -> tuple[float, float]:
    r = km * 1000 * math.sqrt(rng.random())
    t = rng.random() * 2 * math.pi
    return (lon + r * math.cos(t) / (111320 * max(0.1, math.cos(math.radians(lat)))), lat + r * math.sin(t) / 111320)


def coast_vertices(geojson: Path) -> list[tuple[float, float]]:
    pts: list[tuple[float, float]] = []
    for f in json.loads(geojson.read_text())["features"]:
        g = f["geometry"]
        lines = g["coordinates"] if g["type"] == "MultiLineString" else [g["coordinates"]]
        for line in lines:
            pts += [(float(p[0]), float(p[1])) for p in line]
    return [p for p in pts if -56 <= p[1] <= 72]


def draw_places(geojson: Path, n_mountain: int, n_coast: int, n_curated: int, n_rest: int,
                seed: int = 17501) -> list[Place]:
    rng = random.Random(seed)
    out: list[Place] = []
    areas = {k: min(40.0, (b[2] - b[0]) * (b[3] - b[1])) for k, b in RANGES.items()}
    total = sum(areas.values())
    for k, box in RANGES.items():
        for _ in range(max(10, round(n_mountain * areas[k] / total))):
            out.append(Place(*_uniform_in_box(rng, box), "mountain", k))
    verts = coast_vertices(geojson)
    for _ in range(n_coast):
        lon, lat = verts[rng.randrange(len(verts))]
        out.append(Place(*_jitter(rng, lon, lat, 1.0), "coast", region_of(lon, lat)))
    per = max(1, n_curated // len(CURATED))
    for k, (lon, lat, km) in CURATED.items():
        for _ in range(per):
            out.append(Place(*_jitter(rng, lon, lat, km), "curated", k))
    for _ in range(n_rest):
        lon, lat = _uniform_in_box(rng, (-180.0, -56.0, 180.0, 72.0))
        out.append(Place(lon, lat, "rest", region_of(lon, lat)))
    rng.shuffle(out)
    return out
