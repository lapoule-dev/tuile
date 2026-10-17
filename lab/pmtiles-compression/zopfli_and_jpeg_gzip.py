# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Two side questions, cheaply: how much zopfli (gzip-compatible) gains on
terrain over gzip-9, and what gzip does to JPEG tiles (nothing, expectedly).

    venv/bin/python -I zopfli_and_jpeg_gzip.py TERRAIN.pmtiles IMAGERY.pmtiles
"""

from __future__ import annotations

import gzip
import sys
import time
from pathlib import Path

import zopfli.gzip

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive  # noqa: E402


def main() -> None:
    terrain = [b for _, b in Archive.open(Path(sys.argv[1])).tiles()]
    raw = sum(map(len, terrain))
    g9 = sum(len(gzip.compress(t, 9, mtime=0)) for t in terrain)
    t0 = time.perf_counter()
    zp = sum(len(zopfli.gzip.compress(t, numiterations=5)) for t in terrain)
    dt = (time.perf_counter() - t0) / len(terrain) * 1e3
    print(f"terrain {len(terrain)} tiles: gzip-9 {g9 / raw:.3f}, zopfli-5 {zp / raw:.3f} ({dt:.0f} ms/tile)")
    jpegs = [b for i, (_, b) in enumerate(Archive.open(Path(sys.argv[2])).tiles()) if i < 1000]
    jr = sum(map(len, jpegs))
    print(f"jpeg {len(jpegs)} tiles: gzip-9 {sum(len(gzip.compress(j, 9, mtime=0)) for j in jpegs) / jr:.3f}")


if __name__ == "__main__":
    main()
