# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A film pack (`.tuilepack`) read for the study: its table, and each block
of its blob region classified by what it carries.

The FlatBuffers accessors are generated from the schema at run time
(`flatc --python -o GEN crates/tuile-pack/schema/tuile_pack.fbs`) into a
scratch directory, never committed: pass that directory as `gen`.
"""

from __future__ import annotations

import struct
import sys
from dataclasses import dataclass
from pathlib import Path

MAGIC = b"TUILEPK\x00"
PREAMBLE = 16
KINDS = ("positions", "normals", "uvs", "indices", "texture")


@dataclass(frozen=True)
class PackBlock:
    kind: str
    offset: int  # from the start of the blob region
    stored: int
    raw: int
    lz4: bool  # mesh blocks are LZ4; a PNG texture is stored as produced


@dataclass
class PackFile:
    path: Path
    version: int
    content: int
    table_bytes: int
    blob_start: int
    blob_bytes: int
    tiles: int
    frames: int
    blocks: list[PackBlock]


def open_pack(path: Path, gen: Path) -> PackFile:
    sys.path.insert(0, str(gen))
    from tuile.pack.Pack import Pack  # type: ignore[import-not-found]

    raw = path.read_bytes()
    if raw[:8] != MAGIC:
        raise ValueError(f"{path}: not a pack")
    (table_len,) = struct.unpack_from("<Q", raw, 8)
    start = PREAMBLE + table_len
    root = Pack.GetRootAs(raw[PREAMBLE:start], 0)
    blocks: list[PackBlock] = []
    seen: set[int] = set()
    for i in range(root.TilesLength()):
        t = root.Tiles(i)
        for kind in KINDS:
            b = getattr(t, kind.capitalize())()
            if b is None or b.Stored() == 0 or b.Offset() in seen:
                continue
            seen.add(b.Offset())
            blocks.append(
                PackBlock(kind, b.Offset(), b.Stored(), b.Raw(), lz4=b.Stored() != b.Raw() or kind != "texture")
            )
    blocks.sort(key=lambda b: b.offset)
    return PackFile(
        path, root.Version(), root.Content(), table_len, start, len(raw) - start,
        root.TilesLength(), root.FramesLength(), blocks,
    )
