# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Tables from `imagery.py`'s output: per archive and level, per variant,
medians of size relative to the source, quality and times. Each level is its
own table; ratios are relative to the source tile of the same level.

    python -I summarize_imagery.py IN.json [--by-level]
"""

from __future__ import annotations

import json
import statistics as st
import sys
from collections import defaultdict
from pathlib import Path


def main() -> None:
    rows = json.loads(Path(sys.argv[1]).read_text())
    by_level = "--by-level" in sys.argv
    groups: dict[tuple[str, int], list[dict]] = defaultdict(list)
    for r in rows:
        groups[(r["archive"], r["zxy"][0])].append(r)
    q = [r["quality_estimate"] for r in rows]
    print(f"tiles={len(rows)} source quality estimate median={st.median(q):.0f} "
          f"min={min(q):.0f} max={max(q):.0f}; subsampling={sorted({r['subsampling'] for r in rows})}")
    # Per-variant medians of the per-level medians: one number per level
    # first, so a level with more tiles does not weigh more.
    per_variant: dict[str, dict[str, list[float]]] = defaultdict(lambda: defaultdict(list))
    for (arch, z), rs in sorted(groups.items()):
        src = {id(r): r["source_bytes"] for r in rs}
        names = [v["name"] for v in rs[0]["variants"]]
        if by_level:
            print(f"\n## {arch} level {z} ({len(rs)} tiles, source median {st.median(src.values()) / 1024:.1f} KiB)")
            print(f"{'variant':26} {'size/src':>8} {'PSNR-Y':>7} {'SSIM-Y':>7} {'SSIMU2':>7} {'dec ms':>7} {'enc ms':>8}")
        for name in names:
            vs = [(r, next(v for v in r["variants"] if v["name"] == name)) for r in rs]
            m = {
                "ratio": st.median(v["bytes"] / r["source_bytes"] for r, v in vs),
                "psnr": st.median(v["psnr_y"] for _, v in vs),
                "ssim": st.median(v.get("ssim_y", 1.0) for _, v in vs),
                "s2": st.median(v["ssimulacra2"] for _, v in vs),
                "dec": st.median(v["dec_ms"] for _, v in vs),
                "enc": st.median(v["enc_ms"] for _, v in vs),
            }
            for k, x in m.items():
                per_variant[name][k].append(x)
            for k in ("transcode_ms_BC7_RGBA", "transcode_ms_ASTC_LDR_4X4_RGBA", "transcode_ms_ETC1_RGB", "transcode_ms_BC1_RGB"):
                xs = [v["extra"][k] for _, v in vs if k in v.get("extra", {})]
                if xs:
                    per_variant[name][k].append(st.median(xs))
            if by_level:
                print(f"{name:26} {m['ratio']:8.3f} {m['psnr']:7.1f} {m['ssim']:7.4f} {m['s2']:7.1f} {m['dec']:7.2f} {m['enc']:8.1f}")
    print(f"\n## median over the {len(groups)} (archive, level) groups of each group's median")
    print(f"{'variant':26} {'size/src':>8} {'range':>13} {'PSNR-Y':>7} {'SSIM-Y':>7} {'SSIMU2':>7} {'dec ms':>7} {'enc ms':>8}  transcode ms")
    for name, m in per_variant.items():
        rng = f"{min(m['ratio']):.2f}-{max(m['ratio']):.2f}"
        tc = " ".join(f"{k.removeprefix('transcode_ms_')}={st.median(v):.2f}" for k, v in m.items() if k.startswith("transcode"))
        print(f"{name:26} {st.median(m['ratio']):8.3f} {rng:>13} {st.median(m['psnr']):7.1f} {st.median(m['ssim']):7.4f} "
              f"{st.median(m['s2']):7.1f} {st.median(m['dec']):7.2f} {st.median(m['enc']):8.1f}  {tc}")


if __name__ == "__main__":
    main()
