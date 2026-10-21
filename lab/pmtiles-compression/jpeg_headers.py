# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""How much of an imagery archive is JPEG headers (everything up to the end
of the first SOS segment), and how many distinct headers it holds.

    python -I jpeg_headers.py ARCHIVE.pmtiles
"""

from __future__ import annotations

import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive  # noqa: E402


def main() -> None:
    heads: Counter[bytes] = Counter()
    head_bytes = total = 0
    for _, b in Archive.open(Path(sys.argv[1])).tiles():
        i = b.find(b"\xff\xda")
        n = int.from_bytes(b[i + 2 : i + 4], "big")
        head = b[: i + 2 + n]
        heads[head] += 1
        head_bytes += len(head)
        total += len(b)
    print(f"header share {head_bytes / total:.4f}, mean {head_bytes / sum(heads.values()):.0f} B, distinct {len(heads)}")


if __name__ == "__main__":
    main()
