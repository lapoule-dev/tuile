# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Where samples are drawn, and which split each place belongs to.

The mix favours what the athletes fly and sail over: **mountains** (rock, scree, glaciers, alpine meadows, treeline,
mountain villages and roads, take-off and landing fields) and **coasts** (cliffs, beaches, rocky shores, surf,
shallow water, harbours and boats, dunes, estuaries, lagoons). The rest of the planet is a smaller diversity base.
The stratum written with each sample is not the one intended here but the one measured (``geo.classify``).

Splits are by **region**, never by sample: a test region is never seen in training, and ``bench.held_out`` keeps
every training sample at least ``bench.GUARD_M`` away from the benchmark blocks.

Samples of a site no longer listed here (``ign-semnoz``, ``ign-argentiere``: too close to the Annecy and Mont Blanc
test region) are dropped when the index is built.

- **test, mountain**: the central Pyrenees on both sides (benchmark blocks: Luchon, Oo, Aneto, Benasque), the Bernese
  Oberland (Interlaken, Lauterbrunnen), the Mont Blanc and Annecy blocks, the whole New Zealand West Coast survey
  (Westland: Southern Alps west face, glaciers), and the OpenAerialMap captures of Oceania;
- **test, coast**: Crozon (Brittany cliffs), Mallorca, the Auckland coast survey (5 cm), and the OpenAerialMap
  captures of the Caribbean and Central America;
- **test, biome never seen**: the OpenAerialMap captures of the Middle East (desert).
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Site:
    name: str
    source: str
    lon: float
    lat: float
    radius_km: float
    n: int
    split: str = "train"
    intent: str = "mountain"
    """What the site was chosen for; the measured stratum is recorded per sample."""
    zoom: int | None = None


M, C, O = "mountain", "coast", "other"
IGN, CH, ES, NL = "ign-bdortho-20cm", "swisstopo-swissimage", "ign-es-pnoa-ma", "pdok-luchtfoto-hr"

XYZ_SITES: tuple[Site, ...] = (
    # France, mountains
    Site("ign-vanoise", IGN, 6.80, 45.40, 8, 380),
    Site("ign-ecrins", IGN, 6.35, 44.92, 8, 380),
    Site("ign-briancon", IGN, 6.64, 44.90, 5, 300),
    Site("ign-maurienne", IGN, 6.45, 45.25, 6, 250),
    Site("ign-st-hilaire", IGN, 5.88, 45.31, 4, 250),
    Site("ign-beaufortain", IGN, 6.60, 45.70, 5, 300),
    Site("ign-vercors", IGN, 5.45, 44.95, 8, 300),
    Site("ign-gavarnie", IGN, -0.01, 42.73, 6, 350),
    Site("ign-pic-du-midi", IGN, 0.14, 42.94, 5, 300),
    Site("ign-ax", IGN, 1.84, 42.72, 6, 300),
    Site("ign-canigou", IGN, 2.45, 42.52, 6, 300),
    Site("ign-puy-de-dome", IGN, 2.96, 45.77, 5, 250),
    Site("ign-corsica-mountains", IGN, 9.05, 42.25, 10, 300),
    Site("ign-queyras", IGN, 6.85, 44.75, 6, 150, "val"),
    # France, coasts
    Site("ign-etretat", IGN, 0.20, 49.70, 6, 300, intent=C),
    Site("ign-quiberon", IGN, -3.12, 47.48, 6, 300, intent=C),
    Site("ign-calanques", IGN, 5.45, 43.21, 6, 300, intent=C),
    Site("ign-antibes", IGN, 7.12, 43.58, 4, 250, intent=C),
    Site("ign-arcachon", IGN, -1.20, 44.62, 8, 300, intent=C),
    Site("ign-bonifacio", IGN, 9.16, 41.39, 5, 300, intent=C),
    Site("ign-gironde", IGN, -0.95, 45.55, 8, 250, intent=C),
    Site("ign-mont-st-michel", IGN, -1.51, 48.64, 6, 250, intent=C),
    Site("ign-la-rochelle", IGN, -1.17, 46.15, 5, 250, intent=C),
    Site("ign-camargue", IGN, 4.45, 43.45, 10, 250, intent=C),
    Site("ign-belle-ile", IGN, -3.17, 47.33, 5, 150, "val", intent=C),
    Site("ign-crozon", IGN, -4.55, 48.25, 6, 250, "test", intent=C),
    # France, the rest
    Site("ign-beauce", IGN, 1.60, 48.30, 10, 120, intent=O),
    Site("ign-paris", IGN, 2.35, 48.86, 5, 120, intent=O),
    Site("ign-lyon", IGN, 4.84, 45.76, 4, 100, intent=O),
    Site("ign-landes-forest", IGN, -0.90, 44.20, 10, 100, intent=O),
    Site("ign-bordeaux-vineyards", IGN, -0.20, 44.90, 8, 100, intent=O),
    # Switzerland (z20 where the 10 cm is native, else z19: checked per sample)
    Site("ch-zermatt", CH, 7.75, 46.02, 6, 350, zoom=19),
    Site("ch-aletsch", CH, 8.03, 46.44, 6, 300, zoom=19),
    Site("ch-engadin", CH, 9.84, 46.50, 8, 350),
    Site("ch-davos", CH, 9.83, 46.80, 6, 300),
    Site("ch-valais-sion", CH, 7.36, 46.23, 8, 350),
    Site("ch-pilatus", CH, 8.25, 46.98, 5, 300),
    Site("ch-ticino-locarno", CH, 8.79, 46.17, 6, 300),
    Site("ch-gstaad", CH, 7.28, 46.47, 5, 150, "val"),
    Site("ch-geneva-lakeshore", CH, 6.60, 46.50, 6, 120, intent=O),
    # Spain
    Site("es-sierra-nevada", ES, -3.30, 37.05, 10, 300),
    Site("es-picos", ES, -4.85, 43.20, 8, 300),
    Site("es-ordesa", ES, -0.05, 42.65, 6, 300),
    Site("es-montserrat", ES, 1.83, 41.60, 4, 200),
    Site("es-teide", ES, -16.64, 28.27, 8, 250),
    Site("es-gredos", ES, -5.20, 40.25, 8, 150, "val"),
    Site("es-costa-brava", ES, 3.20, 41.90, 8, 300, intent=C),
    Site("es-cadiz", ES, -6.20, 36.45, 8, 300, intent=C),
    Site("es-galicia-rias", ES, -8.80, 42.40, 8, 300, intent=C),
    Site("es-donana", ES, -6.45, 36.90, 8, 200, intent=C),
    Site("es-lanzarote", ES, -13.65, 29.00, 8, 250, intent=C),
    Site("es-albufera", ES, -0.33, 39.33, 6, 200, intent=C),
    Site("es-mallorca", ES, 2.65, 39.80, 15, 250, "test", intent=C),
    Site("es-castilla-fields", ES, -4.50, 41.50, 10, 100, intent=O),
    Site("es-madrid", ES, -3.70, 40.42, 4, 100, intent=O),
    Site("es-andalucia-olives", ES, -3.80, 37.80, 8, 100, intent=O),
    # Netherlands (7.5 cm: z20)
    Site("nl-texel", NL, 4.80, 53.05, 8, 300, intent=C),
    Site("nl-scheveningen", NL, 4.27, 52.10, 4, 250, intent=C),
    Site("nl-zeeland", NL, 3.80, 51.55, 10, 300, intent=C),
    Site("nl-ijmuiden", NL, 4.60, 52.46, 4, 200, intent=C),
    Site("nl-rotterdam-port", NL, 4.10, 51.95, 5, 120, "val", intent=C),
    Site("nl-amsterdam", NL, 4.89, 52.37, 3, 100, intent=O),
    Site("nl-polders", NL, 5.40, 52.50, 10, 100, intent=O),
)


@dataclass(frozen=True)
class Collection:
    """A LINZ survey: every COG under ``prefix`` in the public bucket."""

    name: str
    prefix: str
    n: int
    split: str = "train"
    intent: str = "mountain"


LINZ_COLLECTIONS: tuple[Collection, ...] = (
    Collection("nz-otago-0.2", "otago/otago_2023-2024_0.2m/rgb/2193/", 600),
    Collection("nz-canterbury-0.2", "canterbury/canterbury_2024_0.2m/rgb/2193/", 600),
    Collection("nz-cardrona-0.05", "otago/cardrona_2025_0.05m/rgb/2193/", 250),
    Collection("nz-marlborough-0.075", "marlborough/marlborough_2023_0.075m/rgb/2193/", 400, intent="coast"),
    Collection("nz-banks-peninsula-0.075", "canterbury/banks-peninsula_2025_0.075m/rgb/2193/", 400, intent="coast"),
    Collection("nz-buller-0.075", "west-coast/buller_2024-2025_0.075m/rgb/2193/", 300, intent="coast"),
    Collection("nz-tasman-0.075", "tasman/tasman_2023_0.075m/rgb/2193/", 150, "val", intent="coast"),
    Collection("nz-ashburton-0.075", "canterbury/ashburton_2025_0.075m/rgb/2193/", 150, intent="other"),
    Collection("nz-westland-0.2", "west-coast/west-coast_2024-2026_0.2m/rgb/2193/", 300, "test"),
    Collection("nz-auckland-coast-0.05", "auckland/auckland-coast_2023_0.05m/rgb/2193/", 250, "test", intent="coast"),
)

OAM_TEST_REGIONS = {"oceania", "caribbean-centralam", "middle-east"}
OAM_VAL_SHARE = 0.08
"""Share of OpenAerialMap 1-degree cells (outside the test regions) kept for validation."""
