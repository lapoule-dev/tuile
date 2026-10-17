# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""One part of the harvest, as a process: ``python -m aerial.harvest_run <part> [shard] [shards]``.

Parts: ``xyz:<source>`` (every site of that source, one request at a time), ``linz``, ``oam`` (sharded). Rows go to
``data/dataset/journal/<part>.jsonl``; images under ``data/dataset/hr``. A part run again redraws the same samples and
skips the images already on disk.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

from .geo import Coastline
from .harvest import Writer, harvest_linz, harvest_oam, harvest_site
from .sites import LINZ_COLLECTIONS, XYZ_SITES

DATA = Path(__file__).resolve().parents[2] / "data"


def main() -> None:
    part = sys.argv[1]
    shard = (int(sys.argv[2]), int(sys.argv[3])) if len(sys.argv) > 3 else (0, 1)
    root = DATA / "dataset"
    tag = part.replace(":", "-") + (f"-{shard[0]}of{shard[1]}" if shard[1] > 1 else "")
    (root / "journal").mkdir(parents=True, exist_ok=True)
    journal = root / "journal" / f"{tag}.jsonl"
    journal.unlink(missing_ok=True)
    w = Writer(root, Coastline(DATA / "geo" / "ne_10m_coastline.geojson"), journal)

    def log(msg: str) -> None:
        print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)

    if part.startswith("xyz:"):
        for site in XYZ_SITES:
            if site.source == part[4:]:
                harvest_site(site, w, log=log)
    elif part == "linz":
        for c in LINZ_COLLECTIONS:
            harvest_linz(c, w, log=log)
    elif part == "oam":
        cat = json.loads((DATA / "catalogues" / "oam-usable.json").read_text())
        harvest_oam(cat, w, shard=shard, log=log)
    log(f"done: {len(w.rows)} samples")


if __name__ == "__main__":
    main()
