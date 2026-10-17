# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Open orthophoto services, with their licence and the zoom where their pixels are real.

``truth_zoom`` is not the deepest level a service answers: it is the deepest level whose detail was measured to be
native rather than upsampled (``native_psnr``: a native level loses a lot when reduced 2x2 and enlarged back; an
upsampled one barely changes). Measured on 2026-10-17 over 12 tiles per level, median PSNR-Y:

| service    | z18     | z19     | z20                                      |
|------------|---------|---------|------------------------------------------|
| IGN 20 cm  | 28.4 dB | 33.6 dB | not served                               |
| SWISSIMAGE | 28.6 dB | 33.6 dB | 35.0 dB in the valleys, 42.8 dB high up   |
| PNOA       | 24.2 dB | 30.7 dB | 33.2 dB with blocky top-octave power      |

The commercial layer of the lab's zone gives 32.4 dB at z18 and **37.3 dB at z19**: its z19 holds less real detail
than its z18 does at its own scale, consistent with a z19 partly upsampled.

Evaluated and **not** used as high-resolution targets (recorded in the README): NAIP (0.6 m at best over the US
mountains and coasts probed), Japan GSI seamless photo (served to z18, about 0.5 m), NSW imagery (licence of the
web service not established), OpenAerialMap images under CC BY-NC or CC BY-SA, or without a licence, and its
satellite captures.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class OrthoSource:
    name: str
    kind: str
    """``xyz``: web-mercator tiles; ``cog``: cloud-optimised GeoTIFFs read by window."""
    url: str
    """For ``xyz``, a template with ``{z}``, ``{x}``, ``{y}`` (row from the top); for ``cog``, a description."""
    licence: str
    attribution: str
    native_m: float
    """Nominal ground sample distance of the product, metres (best case for a collection of products)."""
    truth_zoom: int = 0
    """``xyz`` only: deepest zoom whose detail is native (see the module's table)."""
    coverage: str = ""


IGN = OrthoSource(
    "ign-bdortho-20cm", "xyz",
    "https://data.geopf.fr/wmts?SERVICE=WMTS&REQUEST=GetTile&VERSION=1.0.0&LAYER=HR.ORTHOIMAGERY.ORTHOPHOTOS"
    "&STYLE=normal&TILEMATRIXSET=PM&TILEMATRIX={z}&TILEROW={y}&TILECOL={x}&FORMAT=image/jpeg",
    "Licence Ouverte / Open Licence 2.0 (Etalab), as listed for BD ORTHO on data.gouv.fr ('lov2')",
    "IGN, BD ORTHO", 0.20, 19,
    "France and overseas, 20 cm. The 5-10 cm layer (THR) exists only in a few cities (Paris yes; Luchon, "
    "Chamonix, Annecy, Toulouse no).",
)

SWISSIMAGE = OrthoSource(
    "swisstopo-swissimage", "xyz",
    "https://wmts.geo.admin.ch/1.0.0/ch.swisstopo.swissimage/default/current/3857/{z}/{x}/{y}.jpeg",
    "swisstopo terms of use for free geodata and geoservices (open government data): free use, commercial "
    "included, source mention mandatory",
    "swisstopo, SWISSIMAGE", 0.10, 20,
    "Switzerland: 10 cm in the plains and main valleys, 25 cm over the Alps (swisstopo; matches the measure: "
    "z20 native in Interlaken, upsampled at Zermatt and Grindelwald). Per-sample check decides z20 or z19.",
)

PNOA = OrthoSource(
    "ign-es-pnoa-ma", "xyz",
    "https://www.ign.es/wmts/pnoa-ma?SERVICE=WMTS&REQUEST=GetTile&VERSION=1.0.0&LAYER=OI.OrthoimageCoverage"
    "&STYLE=default&TILEMATRIXSET=GoogleMapsCompatible&TILEMATRIX={z}&TILEROW={y}&TILECOL={x}&FORMAT=image/jpeg",
    "CC BY 4.0 (IGN Espana / CNIG, as stated on scne.es)",
    "PNOA cedido por (c) Instituto Geografico Nacional", 0.25, 19,
    "Spain and the Canaries, 25 cm (15 cm in places).",
)

PDOK = OrthoSource(
    "pdok-luchtfoto-hr", "xyz",
    "https://service.pdok.nl/hwh/luchtfotorgb/wmts/v1_0/Actueel_orthoHR/EPSG:3857/{z}/{x}/{y}.jpeg",
    "Beeldmateriaal Nederland open data via PDOK (no fees, no access constraints in the service metadata; "
    "CC BY 4.0 per the publisher, to be re-checked before a release)",
    "Beeldmateriaal Nederland / PDOK", 0.075, 20,
    "Netherlands, 7.5 cm: coasts, dunes, harbours, polders, dense towns. No mountains.",
)

LINZ = OrthoSource(
    "linz-nz-imagery", "cog",
    "s3://nz-imagery (public bucket, anonymous), one STAC collection per survey, COGs in NZTM2000",
    "CC BY 4.0 (Toitu Te Whenua Land Information New Zealand)",
    "Sourced from the LINZ Data Service, CC BY 4.0", 0.05,
    coverage="New Zealand: 5-10 cm urban and coast surveys, 20-30 cm rural (Southern Alps included).",
)

OAM = OrthoSource(
    "openaerialmap", "cog",
    "https://api.openaerialmap.org/meta (catalogue), COGs on the public oin-hotosm buckets",
    "CC BY 4.0 per image as declared by its uploader (only those are used; NC, SA and undeclared are excluded)",
    "OpenAerialMap contributors (per image: provider)", 0.02,
    coverage="Worldwide drone and aircraft captures, 1-30 cm; strong in Africa, Latin America, South and "
    "South-East Asia, the US.",
)

SOURCES: dict[str, OrthoSource] = {s.name: s for s in (IGN, SWISSIMAGE, PNOA, PDOK, LINZ, OAM)}
