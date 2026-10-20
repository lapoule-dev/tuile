# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The dataset's index, built from the harvest journals: one row per sample, its licence, split, region and biome.

Rules applied here, once, for every source alike:

- a sample of a site no longer listed in ``sites`` is dropped (``ign-semnoz``, ``ign-argentiere``);
- a hand-chosen site (``sites``) keeps its split, except inside a test box (``world.TEST_BOXES``), which always
  wins; OpenAerialMap and the commercial layer take the planet-wide rule (``world.split_of``: test boxes, the
  Oceania, Caribbean and Middle East regions, a val share per 1-degree cell);
- the **biome** is read from the measured land cover, latitude and stratum (``biome_of``);
- the licence and attribution of the sample's source are copied into the row.
"""

from __future__ import annotations

import glob
import json
from pathlib import Path

import pandas as pd

from .sites import LINZ_COLLECTIONS, XYZ_SITES
from .sources import SOURCES
from .world import in_test_box, region_of, split_of

COMMERCIAL = {"commercial-ion-asset-2": ("commercial imagery through ion asset 2; terms accepted by the project owner "
                                         "for this work (low-resolution, unlabelled and anchor use only)",
                                         "imagery provider, via Cesium ion")}


def biome_of(dominant: str, lat: float, stratum: str) -> str:
    tropical = abs(lat) < 23.5
    return {
        "tree": "tropical-forest" if tropical else ("boreal-forest" if abs(lat) > 55 else "temperate-forest"),
        "shrub": "shrubland", "grass": "grassland-steppe", "crop": "cropland", "built": "settlement",
        "bare": "rock-scree" if stratum == "mountain" else "desert-bare", "snow": "snow-ice", "water": "water",
        "wetland": "wetland", "mangrove": "wetland", "moss": "tundra",
    }.get(dominant, "unknown")


def load(journals: str) -> pd.DataFrame:
    rows = []
    for f in sorted(glob.glob(journals)):
        rows += [json.loads(line) for line in open(f)]
    return pd.DataFrame(rows).drop_duplicates("id", keep="last") if rows else pd.DataFrame()


def build(data: Path) -> pd.DataFrame:
    hr = load(str(data / "dataset" / "journal" / "*.jsonl"))
    hr = hr[hr["source"].isin(list(SOURCES))]  # open sources only: the anchors and the commercial rows apart
    listed = {s.name for s in XYZ_SITES} | {c.name for c in LINZ_COLLECTIONS}
    hr = hr[hr.site.isin(listed) | hr.site.str.startswith("oam-", na=False)].copy()
    hr["role"] = "hr-target"
    bing = load(str(data / "dataset" / "journal" / "bing-*.jsonl"))
    if len(bing):
        bing["role"] = "lr-unlabelled"
        bing["path"] = bing.apply(lambda r: f"bing/tiles/{r.zoom}/{r.x}/{r.y}.jpg", axis=1)
    df = pd.concat([hr, bing], ignore_index=True)
    designed = {s.name: s.split for s in XYZ_SITES} | {c.name: c.split for c in LINZ_COLLECTIONS}

    def split(site: str, lon: float, lat: float) -> str:
        if site in designed:  # a site chosen by hand keeps its split; a test box still wins
            return "test" if in_test_box(lon, lat) else designed[site]
        return split_of(lon, lat)  # OpenAerialMap and the commercial layer: the planet-wide rule

    df["split"] = [split(str(si), lo, la) for si, lo, la in zip(df.site, df.lon, df.lat)]
    df["region"] = [region_of(lo, la) for lo, la in zip(df.lon, df.lat)]
    df["biome"] = [biome_of(str(d), float(la), str(s)) for d, la, s in zip(df.dominant, df.lat, df.stratum)]
    lic = {n: (s.licence, s.attribution) for n, s in SOURCES.items()} | COMMERCIAL
    df["licence"] = df.source.map(lambda s: lic[s][0])
    df["attribution"] = df.source.map(lambda s: lic[s][1])
    return df.reset_index(drop=True)


def coverage(df: pd.DataFrame) -> pd.DataFrame:
    return pd.crosstab([df.role, df.region], df.biome, margins=True)
