#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Contact sheets of the discs' acceptance scenes, before and after a change.

    scripts/discs-sheet.py <before dir> <after dir> <out dir>

Each dir is what `scripts/discs-scenes.sh` writes (film/<scene>/frame-*.png,
viewer/<scene>/NNN.png). For each scene: one row an instant, four pictures —
the viewer before and after, the film before and after — and, under the
film's, the disc instrument's ratio (light measured over expected) from its
discs.parquet (read with pyarrow).
"""
import sys
from pathlib import Path

import pyarrow.parquet as pq
from PIL import Image, ImageDraw

SCENES = ["pic-du-midi-sunset", "full-moon-rise", "crescent-at-dusk", "sun-behind-ridge"]


def frames(root: Path, kind: str, scene: str):
    d = root / kind / scene
    pattern = "frame-*.png" if kind == "film" else "[0-9][0-9][0-9].png"
    return sorted(d.glob(pattern))


def ratios(root: Path, scene: str):
    path = root / "film" / scene / "discs.parquet"
    out = {}
    if path.exists():
        for row in pq.read_table(path).to_pylist():
            if not row["cut"]:
                out.setdefault(row["frame"], []).append(f"{row['body']} {row['ratio']:.3f}")
    return out


def main(before: Path, after: Path, out: Path):
    out.mkdir(parents=True, exist_ok=True)
    for scene in SCENES:
        cols = [frames(before, "viewer", scene), frames(after, "viewer", scene),
                frames(before, "film", scene), frames(after, "film", scene)]
        n = max((len(c) for c in cols), default=0)
        if n == 0:
            continue
        r_before, r_after = ratios(before, scene), ratios(after, scene)
        w, h, label = 480, 270, 18
        sheet = Image.new("RGB", (4 * w, n * (h + label)), "black")
        draw = ImageDraw.Draw(sheet)
        names = ["viewer before", "viewer after", "film before", "film after"]
        for i in range(n):
            for j, col in enumerate(cols):
                if i >= len(col):
                    continue
                im = Image.open(col[i]).convert("RGB").resize((w, h), Image.LANCZOS)
                x, y = j * w, i * (h + label)
                sheet.paste(im, (x, y))
                text = names[j]
                if j >= 2:
                    rs = (r_before if j == 2 else r_after).get(i + 1, [])
                    text += "  " + ", ".join(rs)
                draw.text((x + 4, y + h + 2), text, fill="white")
        sheet.save(out / f"{scene}.png")
        print(out / f"{scene}.png")


if __name__ == "__main__":
    main(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]))
