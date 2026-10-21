# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A minimal, typed PMTiles v3 reader: the header, the directories, the tiles.

Only what the study needs: walk every entry of an archive (root and leaves),
read the bytes of each distinct tile, and rebuild a directory under another
internal compression to compare sizes. Written from the v3 specification.
"""

from __future__ import annotations

import gzip
import struct
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

HEADER_LEN = 127
COMPRESSION = {0: "unknown", 1: "none", 2: "gzip", 3: "brotli", 4: "zstd"}
TILE_TYPE = {0: "unknown", 1: "mvt", 2: "png", 3: "jpeg", 4: "webp", 5: "avif"}


@dataclass(frozen=True)
class Header:
    root_offset: int
    root_length: int
    metadata_offset: int
    metadata_length: int
    leaf_offset: int
    leaf_length: int
    data_offset: int
    data_length: int
    addressed_tiles: int
    tile_entries: int
    tile_contents: int
    clustered: bool
    internal_compression: str
    tile_compression: str
    tile_type: str
    min_zoom: int
    max_zoom: int


@dataclass(frozen=True)
class Entry:
    tile_id: int
    offset: int
    length: int
    run_length: int  # 0: points to a leaf directory


def parse_header(b: bytes) -> Header:
    if b[:7] != b"PMTiles" or b[7] != 3:
        raise ValueError("not a PMTiles v3 archive")
    q = struct.unpack_from("<11Q", b, 8)
    return Header(
        root_offset=q[0],
        root_length=q[1],
        metadata_offset=q[2],
        metadata_length=q[3],
        leaf_offset=q[4],
        leaf_length=q[5],
        data_offset=q[6],
        data_length=q[7],
        addressed_tiles=q[8],
        tile_entries=q[9],
        tile_contents=q[10],
        clustered=bool(b[96]),
        internal_compression=COMPRESSION.get(b[97], "?"),
        tile_compression=COMPRESSION.get(b[98], "?"),
        tile_type=TILE_TYPE.get(b[99], "?"),
        min_zoom=b[100],
        max_zoom=b[101],
    )


def _varint(b: bytes, pos: int) -> tuple[int, int]:
    shift = result = 0
    while True:
        byte = b[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        if byte < 0x80:
            return result, pos
        shift += 7


def _put_varint(out: bytearray, v: int) -> None:
    while v >= 0x80:
        out.append((v & 0x7F) | 0x80)
        v >>= 7
    out.append(v)


def decompress(data: bytes, compression: str) -> bytes:
    if compression == "none":
        return data
    if compression == "gzip":
        return gzip.decompress(data)
    if compression == "zstd":
        import zstandard

        return zstandard.ZstdDecompressor().decompress(data, max_output_size=1 << 30)
    if compression == "brotli":
        import brotli

        return bytes(brotli.decompress(data))
    raise ValueError(f"compression {compression}")


def parse_directory(plain: bytes) -> list[Entry]:
    n, pos = _varint(plain, 0)
    ids, runs, lens, offs = [], [], [], []
    last = 0
    for _ in range(n):
        d, pos = _varint(plain, pos)
        last += d
        ids.append(last)
    for _ in range(n):
        v, pos = _varint(plain, pos)
        runs.append(v)
    for _ in range(n):
        v, pos = _varint(plain, pos)
        lens.append(v)
    for i in range(n):
        v, pos = _varint(plain, pos)
        offs.append(offs[i - 1] + lens[i - 1] if v == 0 and i > 0 else v - 1)
    return [Entry(ids[i], offs[i], lens[i], runs[i]) for i in range(n)]


def serialize_directory(entries: list[Entry]) -> bytes:
    """The uncompressed bytes of a directory, as the spec lays it out."""
    out = bytearray()
    _put_varint(out, len(entries))
    last = 0
    for e in entries:
        _put_varint(out, e.tile_id - last)
        last = e.tile_id
    for e in entries:
        _put_varint(out, e.run_length)
    for e in entries:
        _put_varint(out, e.length)
    for i, e in enumerate(entries):
        prev = entries[i - 1] if i else None
        if prev is not None and e.offset == prev.offset + prev.length:
            _put_varint(out, 0)
        else:
            _put_varint(out, e.offset + 1)
    return bytes(out)


@dataclass
class Archive:
    path: Path
    header: Header
    root: list[Entry]
    leaves: list[list[Entry]]
    root_bytes: bytes  # as stored
    leaf_bytes: list[bytes]  # as stored

    @classmethod
    def open(cls, path: Path) -> "Archive":
        raw = path.read_bytes()
        h = parse_header(raw)
        root_b = raw[h.root_offset : h.root_offset + h.root_length]
        root = parse_directory(decompress(root_b, h.internal_compression))
        leaves: list[list[Entry]] = []
        leaf_b: list[bytes] = []
        stack = [e for e in root if e.run_length == 0]
        while stack:
            e = stack.pop(0)
            b = raw[h.leaf_offset + e.offset : h.leaf_offset + e.offset + e.length]
            entries = parse_directory(decompress(b, h.internal_compression))
            leaves.append(entries)
            leaf_b.append(b)
            stack.extend(x for x in entries if x.run_length == 0)
        return cls(path, h, root, leaves, root_b, leaf_b)

    def tile_entries(self) -> list[Entry]:
        out = [e for e in self.root if e.run_length > 0]
        for leaf in self.leaves:
            out.extend(e for e in leaf if e.run_length > 0)
        out.sort(key=lambda e: e.tile_id)
        return out

    def tiles(self) -> Iterator[tuple[int, bytes]]:
        """(tile id, stored bytes) of every entry, once per entry."""
        with self.path.open("rb") as f:
            for e in self.tile_entries():
                f.seek(self.header.data_offset + e.offset)
                yield e.tile_id, f.read(e.length)


def zxy(tile_id: int) -> tuple[int, int, int]:
    """Hilbert tile id to (z, x, y), archive coordinates."""
    acc = 0
    z = 0
    while True:
        n = 1 << (2 * z)
        if acc + n > tile_id:
            break
        acc += n
        z += 1
    pos = tile_id - acc
    side = 1 << z
    x = y = 0
    s = 1
    t = pos
    while s < side:
        rx = 1 & (t // 2)
        ry = 1 & (t ^ rx)
        if ry == 0:
            if rx == 1:
                x = s - 1 - x
                y = s - 1 - y
            x, y = y, x
        x += s * rx
        y += s * ry
        t //= 4
        s *= 2
    return z, x, y
