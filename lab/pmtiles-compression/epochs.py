# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Duplicate payloads *across* the archives of one zone (epochs, appends):
what PMTiles' per-archive dedup cannot see, and a compaction could.

    python -I epochs.py OUT.json ARCHIVE.pmtiles...
"""

from __future__ import annotations

import hashlib
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pmtstudy.pmtiles_v3 import Archive  # noqa: E402


def main() -> None:
    out = Path(sys.argv[1])
    by_digest: Counter[str] = Counter()
    size_of: dict[str, int] = {}
    ids_of: dict[int, set[str]] = defaultdict(set)
    total = n = 0
    for p in sys.argv[2:]:
        for tid, b in Archive.open(Path(p)).tiles():
            d = hashlib.blake2b(b, digest_size=16).hexdigest()
            by_digest[d] += 1
            size_of[d] = len(b)
            ids_of[tid].add(d)
            total += len(b)
            n += 1
    distinct_bytes = sum(size_of.values())
    same_id_many = sum(1 for s in ids_of.values() if len(s) > 1)
    same_id_repeat = sum(1 for tid, s in ids_of.items() if len(s) == 1)
    r = {
        "archives": len(sys.argv) - 2,
        "entries": n,
        "bytes": total,
        "distinct_payloads": len(by_digest),
        "distinct_bytes": distinct_bytes,
        "saving_if_deduplicated": 1 - distinct_bytes / total,
        "tile_ids": len(ids_of),
        "tile_ids_with_one_payload": same_id_repeat,
        "tile_ids_with_several_payloads": same_id_many,
        "smallest_tile_bytes": min(size_of.values()),
    }
    out.write_text(json.dumps(r, indent=1))
    print(json.dumps(r, indent=1))


if __name__ == "__main__":
    main()
