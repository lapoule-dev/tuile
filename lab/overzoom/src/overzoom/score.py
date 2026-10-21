# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A run's outputs, measured against the truth, on the laptop.

The GPU only ran the networks. Here, for every (sample, input) of a run, five methods are compared with the true tile:

- ``bicubic`` — the floor: what a client shows when it has nothing finer;
- ``swinir``, ``realesrgan`` — the networks' outputs, read back from the run;
- ``swinir+bp``, ``realesrgan+bp`` — the same, back-projected on the input: corrected until, reduced, they give the
  input back. It only removes what contradicts the input, it never adds detail.

Measures, each on the 256 x 256 tile:

- ``psnr_y``, ``ssim_y`` — fidelity of the luminance (higher is better);
- ``lpips`` — perceptual distance (lower is better), when torch and lpips are installed;
- ``delta_e`` — colour difference, detail left out (lower is better);
- ``consistency`` — PSNR-Y between the output reduced to the input's size and the input: does the method agree with
  what it was given? (higher is better; a method that invents ground the input does not show scores low).

Level inputs whose pair is ``other`` (another acquisition) are measured too, and flagged by ``kind``: a method fed
another acquisition cannot be expected to find the truth.
"""

from __future__ import annotations

import time
from collections.abc import Callable

import numpy as np
import pandas as pd
from PIL import Image

from .dataset import Dataset
from .metrics import box_down2, delta_e, psnr_y, ssim_y
from .runs import Run
from .tiles import Rgb

Perceptual = Callable[[Rgb, Rgb], float]


def bicubic(img: Rgb, scale: int) -> Rgb:
    """Bicubic enlargement in floating point, channel by channel."""
    if scale == 1:
        return img
    h, w = img.shape[:2]
    chans = [np.asarray(Image.fromarray(img[..., c], "F").resize((w * scale, h * scale), Image.Resampling.BICUBIC))
             for c in range(3)]
    return np.asarray(np.stack(chans, axis=-1), np.float32)


def reduce(img: Rgb, scale: int) -> Rgb:
    while scale > 1:
        img, scale = box_down2(img), scale // 2
    return img


def back_project(out: Rgb, given: Rgb, scale: int, iterations: int = 20) -> Rgb:
    """Correct ``out`` until its ``scale`` reduction is ``given``: iterated back-projection of the residual."""
    y = out.astype(np.float32)
    for _ in range(iterations):
        y = y + bicubic(given - reduce(y, scale), scale)
    return np.asarray(np.clip(y, 0.0, 1.0), np.float32)


def lpips_metric(device: str = "cpu") -> Perceptual:
    """LPIPS (AlexNet), as a function of two images. Needs torch and lpips (the ``[score]`` extra)."""
    import lpips
    import torch

    net = lpips.LPIPS(net="alex", verbose=False).to(device).eval()

    def distance(a: Rgb, b: Rgb) -> float:
        ta, tb = (torch.from_numpy(x).permute(2, 0, 1)[None].to(device) * 2 - 1 for x in (a, b))
        with torch.no_grad():
            return float(net(ta, tb).item())

    return distance


METHODS = ("bicubic", "swinir", "realesrgan", "swinir+bp", "realesrgan+bp")


def outputs(ds: Dataset, run: Run, sample: int, name: str) -> dict[str, Rgb]:
    """Every method's output for one comparison, 256 x 256."""
    scale = ds.spec.scale(name)
    inp = ds.input(sample, name)
    given = ds.crop(inp)
    out = {"bicubic": ds.crop(bicubic(inp, scale), scale)}
    for family in run.spec.families:
        img = run.output(family, name, sample)
        out[family] = img
        out[f"{family}+bp"] = back_project(img, given, scale)
    return out


def score(ds: Dataset, run: Run, perceptual: Perceptual | None = None, limit: int | None = None,
          progress: bool = True) -> pd.DataFrame:
    """One row per (sample, input, method), over the comparisons whose shard is on disk (the first ``limit``
    samples of them); cached in the run's directory until that set changes."""
    cache = run.dir / "scores.parquet"
    here = run.local()
    samples = list(dict.fromkeys(here["sample"].astype(int)))[:limit]
    pairs = here[here["sample"].isin(samples)].groupby(["sample", "input"]).size().index.tolist()
    if cache.exists():
        cached = pd.read_parquet(cache)
        same = set(map(tuple, cached[["sample", "input"]].drop_duplicates().itertuples(index=False))) == set(pairs)
        if same and ("lpips" in cached) == (perceptual is not None):
            return cached
    rows: list[dict[str, object]] = []
    t0 = time.time()
    for n, (sample, name) in enumerate(pairs):
        if progress and n and n % 100 == 0:
            print(f"{n}/{len(pairs)} comparisons, {time.time() - t0:.0f} s", flush=True)
        sample, scale = int(sample), ds.spec.scale(name)
        truth, given = ds.truth(sample), ds.crop(ds.input(sample, name))
        kind = ds.kind(sample, name)
        for method, img in outputs(ds, run, sample, name).items():
            row: dict[str, object] = {
                "sample": sample, "input": name, "scale": scale, "kind": kind or "synthetic", "method": method,
                "psnr_y": psnr_y(img, truth), "ssim_y": ssim_y(img, truth), "delta_e": delta_e(img, truth),
                "consistency": psnr_y(reduce(img, scale), given)}
            if perceptual is not None:
                row["lpips"] = perceptual(img, truth)
            rows.append(row)
    frame: pd.DataFrame = pd.DataFrame.from_records(rows)
    frame.to_parquet(cache)
    return frame
