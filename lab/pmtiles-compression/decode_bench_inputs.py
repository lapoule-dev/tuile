# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Inputs for the decode benchmark: the same N tiles of an imagery archive as
the source JPEG and as WebP under several encoder settings (cwebp), each set
in one file of length-prefixed records (u32 little-endian + bytes).

    python -I decode_bench_inputs.py SRC.pmtiles OUTDIR N
"""

from __future__ import annotations

import struct
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive  # noqa: E402

VARIANTS = {
    "webp-q90-m4": ["-q", "90", "-m", "4"],
    "webp-q90-m6": ["-q", "90", "-m", "6"],
    "webp-q90-m4-f0": ["-q", "90", "-m", "4", "-f", "0"],
    "webp-q90-m4-seg1": ["-q", "90", "-m", "4", "-segments", "1"],
    "webp-q90-m4-sharpyuv": ["-q", "90", "-m", "4", "-sharp_yuv"],
    "webp-q90-m4-nofilter-seg1": ["-q", "90", "-m", "4", "-f", "0", "-segments", "1"],
}


def main() -> None:
    src, out, n = Path(sys.argv[1]), Path(sys.argv[2]), int(sys.argv[3])
    a = Archive.open(src)
    tiles = list(a.tiles())
    step = max(1, len(tiles) // n)
    sample = [b for _, b in tiles[::step][:n]]
    out.mkdir(parents=True, exist_ok=True)
    with (out / "jpeg.bin").open("wb") as f:
        for b in sample:
            f.write(struct.pack("<I", len(b)) + b)
    with tempfile.TemporaryDirectory() as tmp:
        for name, args in VARIANTS.items():
            with (out / f"{name}.bin").open("wb") as f:
                for i, b in enumerate(sample):
                    j, w = Path(tmp) / "t.jpg", Path(tmp) / "t.webp"
                    j.write_bytes(b)
                    subprocess.run(["cwebp", "-quiet", *args, str(j), "-o", str(w)], check=True)
                    d = w.read_bytes()
                    f.write(struct.pack("<I", len(d)) + d)
            print(name, file=sys.stderr)
    print(len(sample), "tiles")


if __name__ == "__main__":
    main()
