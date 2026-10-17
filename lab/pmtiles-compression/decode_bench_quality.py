# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Size and quality of each WebP encoder setting of the decode benchmark,
against the source JPEG's decode, on every `STRIDE`-th record.

    python -I decode_bench_quality.py DIR [STRIDE]
"""

from __future__ import annotations

import io
import statistics as st
import struct
import sys
from pathlib import Path

import numpy as np
from PIL import Image

sys.path.insert(0, str(Path(__file__).resolve().parent))
from decode_bench_inputs import VARIANTS  # noqa: E402
from imagery import psnr_y, ssimulacra2  # noqa: E402


def records(path: Path) -> list[bytes]:
    raw, out, at = path.read_bytes(), [], 0
    while at + 4 <= len(raw):
        (n,) = struct.unpack_from("<I", raw, at)
        out.append(raw[at + 4 : at + 4 + n])
        at += 4 + n
    return out


def main() -> None:
    d = Path(sys.argv[1])
    stride = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    work = d / "quality-work"
    work.mkdir(exist_ok=True)
    jpeg = records(d / "jpeg.bin")
    refs = {}
    for i in range(0, len(jpeg), stride):
        ref = np.asarray(Image.open(io.BytesIO(jpeg[i])).convert("RGB"))
        p = work / f"ref-{i}.png"
        Image.fromarray(ref).save(p)
        refs[i] = (ref, p)
    base = sum(map(len, records(d / "webp-q90-m4.bin")))
    for name in VARIANTS:
        recs = records(d / f"{name}.bin")
        ps, s2 = [], []
        for i, (ref, p) in refs.items():
            got = np.asarray(Image.open(io.BytesIO(recs[i])).convert("RGB"))
            ps.append(psnr_y(ref, got))
            s2.append(ssimulacra2(p, got, work))
        size = sum(map(len, recs))
        print(f"{name:28} size vs m4 {size / base:6.3f}  vs JPEG {size / sum(map(len, jpeg)):6.3f}  "
              f"PSNR-Y {st.median(ps):5.2f}  SSIMU2 {st.median(s2):5.1f} (n={len(refs)})")


if __name__ == "__main__":
    main()
