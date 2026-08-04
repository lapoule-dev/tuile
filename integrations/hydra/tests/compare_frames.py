#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Measure two rendered frames, and fail unless both actually show something.

`cmp -s` was the first version of this check and it was worthless: two blank
white images differing by three anti-aliased pixels satisfy "not identical", so
the test passed while the render showed nothing at all. A visual assertion has
to measure coverage, not inequality.

The PNG decoder below is written out rather than imported because the render
image carries no third-party Python, and adding one to a farm image to run an
assertion would be a poor trade. PNG is only zlib over filtered scanlines; the
five filter types are the whole format for our purposes.
"""

import struct
import sys
import zlib


def _unfilter(raw, width, height, channels):
    """Undo PNG's per-scanline filters. Returns a flat bytearray of samples."""
    stride = width * channels
    out = bytearray()
    previous = bytearray(stride)
    pos = 0
    for _ in range(height):
        filter_type = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        if filter_type == 1:  # Sub
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif filter_type == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + previous[i]) & 0xFF
        elif filter_type == 3:  # Average
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif filter_type == 4:  # Paeth
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                up = previous[i]
                upleft = previous[i - channels] if i >= channels else 0
                p = left + up - upleft
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - upleft)
                if pa <= pb and pa <= pc:
                    pred = left
                elif pb <= pc:
                    pred = up
                else:
                    pred = upleft
                line[i] = (line[i] + pred) & 0xFF
        elif filter_type != 0:
            raise ValueError(f"unknown PNG filter {filter_type}")
        out += line
        previous = line
    return out


def read_png(path):
    """Returns (width, height, channels, samples)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    pos, idat, header = 8, bytearray(), None
    while pos < len(data):
        length, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat += body
        elif kind == b"IEND":
            break
        pos += 12 + length  # length + type + body + CRC

    if header is None:
        raise ValueError(f"{path} has no IHDR chunk")
    width, height, depth, colour, _, _, interlace = header
    if depth != 8 or interlace != 0:
        raise ValueError("only 8-bit non-interlaced PNGs are handled")
    channels = {0: 1, 2: 3, 4: 2, 6: 4}[colour]
    return width, height, channels, _unfilter(zlib.decompress(bytes(idat)),
                                              width, height, channels)


def coverage(width, height, channels, samples):
    """Fraction of pixels showing geometry rather than background.

    Alpha decides it when there is one, and there is: usdrecord writes RGBA
    with an unwritten background of (0,0,0,0). That matters because a viewer
    composites transparency over white, so the frame *looks* white while every
    background pixel reads as pure black — measure the colour and you conclude
    the image is entirely covered when it is entirely empty.

    Without alpha, fall back to "darker than near-white", which is the right
    reading for a flattened render.
    """
    has_alpha = channels in (2, 4)
    colour_channels = min(channels, 3)
    lit = 0
    for i in range(0, len(samples), channels):
        if has_alpha:
            if samples[i + channels - 1] > 16:
                lit += 1
        elif any(samples[i + c] < 250 for c in range(colour_channels)):
            lit += 1
    return lit / float(width * height)


def difference(a, b, channels_a, channels_b, tolerance=8):
    """Fraction of pixels differing by more than `tolerance` in any channel."""
    n = min(len(a) // channels_a, len(b) // channels_b)
    colour_channels = min(channels_a, channels_b, 3)
    differing = 0
    for p in range(n):
        ia, ib = p * channels_a, p * channels_b
        if any(abs(a[ia + c] - b[ib + c]) > tolerance
               for c in range(colour_channels)):
            differing += 1
    return differing / float(n)


def main():
    if len(sys.argv) < 3:
        print("usage: compare_frames.py FRAME_A FRAME_B "
              "[MIN_COVERAGE] [MIN_DIFFERENCE]", file=sys.stderr)
        return 2

    path_a, path_b = sys.argv[1], sys.argv[2]
    min_coverage = float(sys.argv[3]) if len(sys.argv) > 3 else 0.02
    min_difference = float(sys.argv[4]) if len(sys.argv) > 4 else 0.01

    wa, ha, ca, sa = read_png(path_a)
    wb, hb, cb, sb = read_png(path_b)

    cov_a = coverage(wa, ha, ca, sa)
    cov_b = coverage(wb, hb, cb, sb)
    diff = difference(sa, sb, ca, cb)

    print(f"{path_a}: {wa}x{ha}, {cov_a:.1%} of pixels show geometry")
    print(f"{path_b}: {wb}x{hb}, {cov_b:.1%} of pixels show geometry")
    print(f"difference: {diff:.1%} of pixels")

    failures = []
    # Both frames must show geometry. A blank frame means the procedural
    # produced nothing, and comparing two blanks proves only that noise exists.
    if cov_a < min_coverage:
        failures.append(f"{path_a} is blank ({cov_a:.1%} < {min_coverage:.1%})")
    if cov_b < min_coverage:
        failures.append(f"{path_b} is blank ({cov_b:.1%} < {min_coverage:.1%})")
    # And they must differ substantially: the camera moved, the refinement
    # changed, and that has to be visible rather than a few stray pixels.
    if diff < min_difference:
        failures.append(
            f"frames are too alike ({diff:.1%} < {min_difference:.1%}) — "
            "the procedural was probably not re-cooked when the camera moved")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        return 1

    print("PASS: both frames show geometry, and they differ substantially")
    return 0


if __name__ == "__main__":
    sys.exit(main())
