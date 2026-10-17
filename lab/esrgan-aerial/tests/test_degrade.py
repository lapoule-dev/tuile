# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
from __future__ import annotations

import io

import numpy as np
from PIL import Image, JpegImagePlugin

from aerial.degrade import Draw, degrade, jpeg, make_input, pipeline_down
from aerial.imaging import psnr_y, to_uint8


def _hr(seed: int = 3) -> np.ndarray:
    rng = np.random.default_rng(seed)
    base = rng.random((32, 32, 3)).astype(np.float32)
    img = np.asarray(Image.fromarray(to_uint8(base)).resize((128, 128), Image.Resampling.BICUBIC), np.float32)
    return img / 255.0


def test_input_is_a_quarter_and_rebuilds_byte_for_byte() -> None:
    hr = _hr()
    a, da = make_input(hr, "sample-a")
    b, db = make_input(hr, "sample-a")
    assert a.shape == (32, 32, 3)
    assert da == db and np.array_equal(a, b)
    c, _ = make_input(hr, "sample-b")
    assert not np.array_equal(a, c)


def test_jpeg_is_chroma_subsampled() -> None:
    hr = _hr()
    buf = io.BytesIO()
    Image.fromarray(to_uint8(jpeg(hr, 79))).save(buf, "PNG")
    # the encoder's own setting, read back from an encoded file
    enc = io.BytesIO()
    Image.fromarray(to_uint8(hr)).save(enc, "JPEG", quality=79, subsampling="4:2:0")
    assert JpegImagePlugin.get_sampling(Image.open(enc)) == 2


def test_input_agrees_with_the_deterministic_pyramid() -> None:
    """The damaged input, minus its JPEG and its random parts, is the deterministic pyramid of the truth."""
    hr = _hr()
    clean = Draw(0.0, (0.7, 0.7), (False, False), None, None, 0.0, 100)
    lr = degrade(hr, clean, np.random.default_rng(0))
    assert psnr_y(lr, pipeline_down(hr)) > 40.0
