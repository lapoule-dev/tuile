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
    done: dict[str, dict[str, object]] = {}
    if journal.exists():
        for line in journal.read_text().splitlines():
            row = json.loads(line)
            done[str(row.get("id"))] = row
        journal.rename(journal.with_suffix(f".{int(time.time())}.prev"))
    w = Writer(root, Coastline(DATA / "geo" / "ne_10m_coastline.geojson"), journal)
    w.done = done

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
    elif part == "bing":
        from .harvest import harvest_bing
        from .world import draw_places

        places = draw_places(DATA / "geo" / "ne_10m_coastline.geojson", n_mountain=2400, n_coast=2000,
                             n_curated=330, n_rest=1200)
        harvest_bing(places, DATA / "bing" / "tiles", journal, w.coast, shard=shard, log=log)
    elif part == "anchors":
        import glob

        from .harvest import harvest_anchors

        rows = [json.loads(line) for f in glob.glob(str(root / "journal" / "xyz-*.jsonl"))
                for line in open(f)]
        harvest_anchors(rows, DATA / "bing" / "tiles", journal, n=2500, log=log)
    log(f"done: {len(w.rows)} samples")


if __name__ == "__main__":
    main()
