# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Draw high-resolution samples from the open sources and write them, with what they are, to the dataset.

A sample is a 512 x 512 RGB image of real ground at a known ground sample distance (``gsd_m``), stored as PNG (the
source's own JPEG decoded once, never re-encoded), with one row in ``index.parquet``: where, from which source under
which licence, which split, and the measured place (``geo.classify``: stratum, elevation, relief, distance to the
coast, land-cover shares).

- **web-mercator sources**: 2 x 2 tiles at the truth zoom; a z20 sample whose detail is not native (``native_psnr``
  above ``UPSAMPLED_DB``) is fetched again at z19;
- **COG sources**: a window read through a web-mercator warp at a target distance (``target_gsd``), averaged from the
  file's overviews.

A sample is refused, never patched, when a tile is missing, when it is flat (open sea, cloud: ``MIN_TEXTURE``), when
it has nodata (``MAX_NODATA``) or, in training, when it falls near a benchmark block. Everything random draws from a
generator seeded by the site, so the dataset rebuilds the same.
"""

from __future__ import annotations

import hashlib
import io
import json
import math
import random
import re
import time
import xml.etree.ElementTree as ET
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import numpy.typing as npt
import rasterio
import rasterio.transform
import requests
from PIL import Image
from rasterio.enums import Resampling

from .bench import held_out, lonlat_to_tile
from .geo import Coastline, Place, classify
from .sites import LINZ_COLLECTIONS, OAM_TEST_REGIONS, OAM_VAL_SHARE, XYZ_SITES, Collection, Site
from .sources import LINZ, OAM, SOURCES

SIZE = 512
MIN_TEXTURE = 0.006
MAX_NODATA = 0.002
UPSAMPLED_DB = 39.0
"""A tile whose 2 x 2 reduction enlarged back by bicubic is this close to it carries no native detail at its zoom."""
MERCATOR_RES0 = 2 * math.pi * 6378137.0 / 256
UA = "tuile-image-lab/0.1 (research dataset; laurent@lapoule.dev)"

U8 = npt.NDArray[np.uint8]


def gsd_at(z: int, lat: float) -> float:
    return MERCATOR_RES0 / 2**z * math.cos(math.radians(lat))


def texture(img: U8) -> float:
    y = img.astype(np.float32) @ np.array([0.299, 0.587, 0.114], np.float32) / 255.0
    return float(np.abs(np.diff(y, axis=0)).mean())


def nodata_share(img: U8) -> float:
    """Share of pure black or pure white pixels: the fill of a COG's edges, never ground."""
    black = (img.max(axis=-1) == 0)
    white = (img.min(axis=-1) == 255)
    return float((black | white).mean())


def native_psnr(img: U8) -> float:
    """PSNR-Y of an image against its own 2 x 2 box reduction enlarged back by bicubic (borders left out)."""
    y = img.astype(np.float32) @ np.array([0.299, 0.587, 0.114], np.float32) / 255.0
    h, w = y.shape[0] // 2 * 2, y.shape[1] // 2 * 2
    d = y[:h, :w].reshape(h // 2, 2, w // 2, 2).mean(axis=(1, 3))
    up = np.asarray(Image.fromarray(d, "F").resize((w, h), Image.Resampling.BICUBIC))
    mse = float(((up - y[:h, :w])[8:-8, 8:-8] ** 2).mean())
    return 10.0 * math.log10(1.0 / max(mse, 1e-12))


@dataclass
class Sample:
    id: str
    source: str
    site: str
    split: str
    intent: str
    lon: float
    lat: float
    gsd_m: float
    zoom: int | None
    detail: str
    """What the pixels are: ``native`` (checked or by product) or the reason they were accepted otherwise."""
    native_db: float
    texture: float


class Writer:
    def __init__(self, root: Path, coast: Coastline, journal: Path | None = None) -> None:
        """``journal``: rows are appended there as JSON lines as they come, so a stopped harvest loses nothing."""
        self.root, self.coast, self.journal = root, coast, journal
        self.rows: list[dict[str, object]] = []
        self._place_cache: dict[tuple[int, int], Place] = {}

    def place(self, lon: float, lat: float) -> Place:
        key = (round(lon * 200), round(lat * 200))  # 500 m cells: relief is a 3 km measure
        if key not in self._place_cache:
            self._place_cache[key] = classify(lon, lat, self.coast)
        return self._place_cache[key]

    def add(self, s: Sample, img: U8) -> None:
        p = self.root / "hr" / s.source / s.site / f"{s.id}.png"
        p.parent.mkdir(parents=True, exist_ok=True)
        if not p.exists():
            Image.fromarray(img).save(p, optimize=False, compress_level=6)
        pl = self.place(s.lon, s.lat)
        row = s.__dict__ | {"path": str(p.relative_to(self.root)), "stratum": pl.stratum,
                                       "elevation": pl.elevation, "relief": pl.relief, "coast_m": pl.coast_m,
                                       "cover": json.dumps(pl.cover), "dominant": pl.dominant}
        self.rows.append(row)
        if self.journal is not None:
            with open(self.journal, "a") as f:
                f.write(json.dumps(row) + "\n")


def _id(*parts: object) -> str:
    return hashlib.sha1("/".join(map(str, parts)).encode()).hexdigest()[:12]


def _random_point(rng: random.Random, lon: float, lat: float, radius_km: float) -> tuple[float, float]:
    r = radius_km * 1000.0 * math.sqrt(rng.random())
    a = rng.random() * 2 * math.pi
    dlat = r * math.sin(a) / 111320.0
    dlon = r * math.cos(a) / (111320.0 * math.cos(math.radians(lat)))
    return lon + dlon, lat + dlat


def xyz_block(session: requests.Session, url: str, z: int, x: int, y: int, pause: float) -> U8 | None:
    out = np.empty((SIZE, SIZE, 3), np.uint8)
    for dy in (0, 1):
        for dx in (0, 1):
            r = session.get(url.format(z=z, x=x + dx, y=y + dy), timeout=60)
            time.sleep(pause)
            if r.status_code != 200 or not r.headers.get("Content-Type", "").startswith("image"):
                return None
            t = np.asarray(Image.open(io.BytesIO(r.content)).convert("RGB"))
            if t.shape != (256, 256, 3):
                return None
            out[dy * 256:(dy + 1) * 256, dx * 256:(dx + 1) * 256] = t
    return out


def harvest_site(site: Site, w: Writer, pause: float = 0.12, tries: int = 4,
                 log: Callable[[str], None] = print) -> int:
    src = SOURCES[site.source]
    rng = random.Random(f"{site.name}/17501")
    s = requests.Session()
    s.headers["User-Agent"] = UA
    got, attempts = 0, 0
    while got < site.n and attempts < site.n * tries:
        attempts += 1
        lon, lat = _random_point(rng, site.lon, site.lat, site.radius_km)
        z = site.zoom or src.truth_zoom
        x, y = lonlat_to_tile(lon, lat, z)
        x, y = x // 2 * 2, y // 2 * 2
        if site.split == "train" and held_out(z, x, y):
            continue
        img = xyz_block(s, src.url, z, x, y, pause)
        if img is None:
            continue
        db, detail = native_psnr(img), "native"
        if z == 20 and db > UPSAMPLED_DB:  # 10 cm promised, 25 cm delivered: take the native level instead
            z, x, y = 19, x // 2 // 2 * 2, y // 2 // 2 * 2
            img = xyz_block(s, src.url, z, x, y, pause)
            if img is None:
                continue
            db, detail = native_psnr(img), "native-after-fallback"
        tex = texture(img)
        if tex < MIN_TEXTURE or nodata_share(img) > MAX_NODATA:
            continue
        n = 2**z
        clon = (x + 1) / n * 360.0 - 180.0
        clat = math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * (y + 1) / n))))
        sid = _id(site.source, z, x, y)
        w.add(Sample(sid, site.source, site.name, site.split, site.intent, clon, clat, gsd_at(z, clat), z,
                     detail, db, tex), img)
        got += 1
        if got % 50 == 0:
            log(f"{site.name}: {got}/{site.n}")
    log(f"{site.name}: {got}/{site.n} in {attempts} draws")
    return got


# --------------------------------------------------------------------------------------------------- COGs ------


def target_gsd(native: float, rng: random.Random) -> float:
    """The distance a COG sample is read at: its own when 7.5 to 30 cm; finer captures (drones) are brought to a
    random distance between 7.5 and 15 cm, the span of the z20-z21 targets; coarser ones are not used."""
    if native > 0.30:
        raise ValueError(f"{native} m is too coarse for a target")
    if native >= 0.075:
        return native
    return rng.uniform(0.075, 0.15)


def read_windows(url: str, rng: random.Random, k: int = 1,
                 gsd: float | None = None) -> list[tuple[U8, float, float, float]]:
    """Up to ``k`` SIZE x SIZE windows at random valid places of a COG, at ground distance ``gsd`` (``target_gsd``
    of the file's own when None). Each is (pixels, lon, lat, distance).

    Read in the file's own projection with a decimated read, so GDAL takes the nearest overview instead of the full
    detail (a 1 cm drone file read at 10 cm is a hundred times fewer bytes). A projected CRS is conformal: over a
    window of a hundred metres it differs from web-mercator by a rotation of a degree or so and a scale the distance
    already accounts for, which is nothing to a training sample. Places are drawn where a 128-pixel overview of the
    whole file shows ground under the whole window (no fill), so irregular footprints do not waste reads."""
    from pyproj import Transformer
    from rasterio.windows import Window
    from scipy.ndimage import binary_erosion

    out: list[tuple[U8, float, float, float]] = []
    with rasterio.Env(GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR", CPL_VSIL_CURL_ALLOWED_EXTENSIONS=".tif,.tiff",
                      GDAL_HTTP_MAX_RETRY="3", GDAL_HTTP_RETRY_DELAY="2"):
        with rasterio.open(url) as src:
            if src.count < 3 or src.crs is None:
                return out
            to_ll = Transformer.from_crs(src.crs, "EPSG:4326", always_xy=True)
            cx0, cy0 = src.xy(src.height // 2, src.width // 2)
            lon0, lat0 = to_ll.transform(cx0, cy0)
            # metres per source pixel
            unit = 111320.0 * (math.cos(math.radians(lat0)) if src.crs.is_geographic else 1.0)
            px_m = abs(src.res[0]) * (unit if src.crs.is_geographic else 1.0)
            t = 128
            th = np.moveaxis(src.read([1, 2, 3], out_shape=(3, t, t), resampling=Resampling.nearest), 0, -1)
            valid = ~((th.max(axis=-1) == 0) | (th.min(axis=-1) == 255))
            valid &= src.dataset_mask(out_shape=(t, t), resampling=Resampling.nearest) > 0  # alpha or nodata
            sx, sy = src.width / t, src.height / t  # source pixels per thumbnail cell
            for _ in range(k):
                g = gsd if gsd is not None else target_gsd(px_m, rng)
                span = SIZE * g / px_m  # window side, source pixels
                if span > src.width or span > src.height:
                    break
                rx, ry = int(math.ceil(span / 2 / sx)) + 1, int(math.ceil(span / 2 / sy)) + 1
                ok = binary_erosion(valid, structure=np.ones((2 * ry + 1, 2 * rx + 1), bool), border_value=0)
                cand = np.argwhere(ok)
                if len(cand) == 0:
                    break
                r, c = cand[rng.randrange(len(cand))]
                col = (c + rng.random()) * sx - span / 2
                row = (r + rng.random()) * sy - span / 2
                col = min(max(col, 0.0), src.width - span)
                row = min(max(row, 0.0), src.height - span)
                a = src.read([1, 2, 3], window=Window(col, row, span, span), out_shape=(3, SIZE, SIZE),
                             resampling=Resampling.average)
                img = np.ascontiguousarray(np.moveaxis(a, 0, -1)).astype(np.uint8)
                m = src.dataset_mask(window=Window(col, row, span, span), out_shape=(64, 64),
                                     resampling=Resampling.nearest)
                if (m == 0).any():
                    continue
                x, y = src.xy(row + span / 2, col + span / 2)
                lon, lat = to_ll.transform(x, y)
                out.append((img, float(lon), float(lat), g))
    return out


def linz_tiffs(prefix: str) -> list[str]:
    base = "https://nz-imagery.s3.ap-southeast-2.amazonaws.com/"
    keys: list[str] = []
    token: str | None = None
    while True:
        params = {"list-type": "2", "prefix": prefix} | ({"continuation-token": token} if token else {})
        r = requests.get(base, params=params, timeout=60)
        r.raise_for_status()
        root = ET.fromstring(r.content)
        ns = {"s": root.tag.split("}")[0].strip("{")}
        keys += [k.text for k in root.iterfind("s:Contents/s:Key", ns) if k.text and k.text.endswith(".tiff")]
        nxt = root.find("s:NextContinuationToken", ns)
        if nxt is None or not nxt.text:
            return [base + k for k in keys]
        token = nxt.text


def harvest_linz(c: Collection, w: Writer, tries: int = 4, log: Callable[[str], None] = print) -> int:
    rng = random.Random(f"{c.name}/17501")
    urls = linz_tiffs(c.prefix)
    native = float(re.search(r"_([0-9.]+)m/", c.prefix).group(1))  # type: ignore[union-attr]
    got, attempts = 0, 0
    while got < c.n and attempts < c.n * tries and urls:
        attempts += 1
        url = rng.choice(urls)
        try:
            got_w = read_windows(url, rng, 1, target_gsd(native, rng))
        except rasterio.errors.RasterioIOError as e:
            log(f"{c.name}: {url}: {e}")
            continue
        if not got_w:
            continue
        img, lon, lat, g = got_w[0]
        if c.split == "train" and held_out(20, *lonlat_to_tile(lon, lat, 20)):
            continue
        tex = texture(img)
        if tex < MIN_TEXTURE or nodata_share(img) > MAX_NODATA:
            continue
        w.add(Sample(_id(url, lon, lat), LINZ.name, c.name, c.split, c.intent, lon, lat, g, None, "native-product",
                     native_psnr(img), tex), img)
        got += 1
        if got % 50 == 0:
            log(f"{c.name}: {got}/{c.n}")
    log(f"{c.name}: {got}/{c.n} in {attempts} draws")
    return got


def oam_split(region: str, cell: tuple[int, int]) -> str:
    if region in OAM_TEST_REGIONS:
        return "test"
    h = int(hashlib.sha1(f"{cell}".encode()).hexdigest(), 16) % 1000
    return "val" if h < OAM_VAL_SHARE * 1000 else "train"


def oam_picks(catalogue: list[dict[str, object]], per_cell: int, rng: random.Random,
              max_bytes: float = 4e9) -> Iterator[dict[str, object]]:
    """At most ``per_cell`` images per 1-degree cell, so a few heavily flown cities do not drown the planet."""
    cells: dict[tuple[int, int], list[dict[str, object]]] = {}
    for im in catalogue:
        if float(im.get("file_size") or 0) > max_bytes:
            continue
        cells.setdefault((int(float(im["lon"]) // 1), int(float(im["lat"]) // 1)), []).append(im)  # type: ignore[arg-type]
    for cell in sorted(cells):
        ims = cells[cell]
        rng.shuffle(ims)
        for im in ims[:per_cell]:
            yield im | {"cell": cell, "split": oam_split(str(im["region"]), cell)}


def harvest_oam(catalogue: list[dict[str, object]], w: Writer, per_cell: int = 2, focus: int = 4, rest: int = 1,
                shard: tuple[int, int] = (0, 1), log: Callable[[str], None] = print) -> int:
    """``focus`` windows from an image over mountains or coasts, ``rest`` from any other; images ``shard[0]``,
    ``shard[0] + shard[1]``... of the picks only."""
    rng = random.Random("oam/17501")
    got = 0
    for k, im in enumerate(oam_picks(catalogue, per_cell, rng)):
        if k % shard[1] != shard[0]:
            continue
        url = str(im["uuid"])
        per_image = focus if w.place(float(im["lon"]), float(im["lat"])).stratum in ("mountain", "coast") else rest  # type: ignore[arg-type]
        try:
            windows = read_windows(url, rng, per_image)
        except Exception as e:  # a broken upload must not stop a harvest of thousands
            log(f"oam {im['id']}: {type(e).__name__}: {str(e)[:120]}")
            continue
        for img, lon, lat, g in windows:
            tex = texture(img)
            if tex < MIN_TEXTURE or nodata_share(img) > MAX_NODATA:
                continue
            if im["split"] == "train" and held_out(20, *lonlat_to_tile(lon, lat, 20)):
                continue
            w.add(Sample(_id(url, lon, lat), OAM.name, f"oam-{im['region']}", str(im["split"]), "any", lon, lat,
                         g, None, f"native-product:{im['platform']}:{im['provider']}", native_psnr(img), tex), img)
            got += 1
        if k % 50 == 0:
            log(f"oam: {k} images visited, {got} samples")
    return got


def harvest_all(root: Path, coast: Coastline, which: set[str], log: Callable[[str], None] = print) -> Writer:
    w = Writer(root, coast)
    if "xyz" in which:
        for site in XYZ_SITES:
            harvest_site(site, w, log=log)
    if "linz" in which:
        for c in LINZ_COLLECTIONS:
            harvest_linz(c, w, log=log)
    if "oam" in which:
        cat = json.loads((root.parent / "catalogues" / "oam-usable.json").read_text())
        harvest_oam(cat, w, log=log)
    return w
