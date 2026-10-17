# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The automated judge: is a x4 output faithful to the input, close to the true ground, and realistic?

Every component, its direction and its weight are in ``COMPONENTS``. Per sample, each is put on one scale anchored on
two references of that same sample: **bicubic = 0** and **the true ground = 1** (``combine``). A score above 0 beats
what a client shows today; 1 is as good as the truth on that component. The overall score is the weighted mean,
clipped per component to [-2, 1.5] so that one wild component cannot carry a sample.

Components (all on the 256 x 256 output against the 256 x 256 truth unless said):

- **fidelity**: ``psnr_y`` (higher is better, capped at 45 dB), ``lpips`` (AlexNet, lower), ``dists`` (lower);
- **faithfulness**: ``consistency``, the PSNR-Y between the output reduced by the measured pyramid
  (``degrade.pipeline_down``) and the input, as a one-sided penalty against the truth's own (``hinge_db``); ``edge_precision``, the share of the
  output's edges that lie within 2 px of an edge of the truth (1 minus it is **invented structure**);
- **realism**: ``edge_recall``, the share of the truth's edges the output has (1 minus it is **omitted structure**);
  ``band_gap``, the absolute gap of the output's high-band power to the truth's, in log10 (an oversharpened output
  has too much, a blurred one too little: both are penalised); ``clip_iqa``, the CLIP-IQA method (one antonym pair: real photograph / painted
  image) on OpenCLIP ViT-B/32, which needs no truth and is the realism component that also runs where there is none.

Without truth (commercial tiles beyond the deepest level), the judge reports ``consistency`` against the input,
``clip_iqa`` and ``band_gap`` against the real high-resolution samples of the same stratum, and the share of each
method's samples it would rank above bicubic — never a fidelity it cannot measure.

Licences: LPIPS (BSD-2-Clause), DISTS (MIT), OpenCLIP (MIT; weights trained on LAION-2B, MIT as published). The
``pyiqa`` toolbox is **not** used: it is under the PolyForm Noncommercial licence.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

import numpy as np
import pandas as pd
from scipy.ndimage import distance_transform_edt
from skimage.feature import canny

from .degrade import pipeline_down
from .imaging import Rgb, gaussian_blur, luma, psnr_y

EDGE_SIGMA = 1.5
EDGE_TOL = 2.0
PSNR_CAP = 45.0


@dataclass(frozen=True)
class Component:
    name: str
    group: str
    weight: float
    lower_is_better: bool = False
    hinge_db: float = 0.0
    """When set, a one-sided penalty instead of the bicubic-to-truth scale: 0 at or above the truth's own value, -1
    per ``hinge_db`` below it. Agreeing with the input *more* than the truth does is not a merit (the oversharpened
    control does: 39.7 dB against the truth's 35.3), only contradicting it is a fault."""


COMPONENTS: tuple[Component, ...] = (
    Component("psnr_y", "fidelity", 0.10),
    Component("lpips", "fidelity", 0.15, lower_is_better=True),
    Component("dists", "fidelity", 0.15, lower_is_better=True),
    Component("consistency", "faithfulness", 0.15, hinge_db=3.0),
    Component("edge_precision", "faithfulness", 0.15),
    Component("edge_recall", "realism", 0.10),
    Component("band_gap", "realism", 0.10, lower_is_better=True),
    Component("clip_iqa", "realism", 0.10),
)
CLIP_LO, CLIP_HI = -2.0, 1.5


def unsharp(img: Rgb, amount: float, sigma: float = 1.0) -> Rgb:
    """Unsharp masking: the oversharpened control the judge must penalise."""
    return np.asarray(np.clip(img + amount * (img - gaussian_blur(img, sigma)), 0.0, 1.0), np.float32)


def edges(img: Rgb) -> np.ndarray:
    y = luma(img).astype(np.float64)
    return np.asarray(canny(y, sigma=EDGE_SIGMA, low_threshold=0.02, high_threshold=0.06), bool)


def edge_scores(out: Rgb, truth: Rgb, tol: float = EDGE_TOL) -> tuple[float, float]:
    """(precision, recall) of the output's edges against the truth's, within ``tol`` pixels, borders left out."""
    eo, et = edges(out), edges(truth)
    m = 4
    eo, et = eo[m:-m, m:-m], et[m:-m, m:-m]
    if not et.any() and not eo.any():
        return 1.0, 1.0
    dt_t = distance_transform_edt(~et) if et.any() else np.full(et.shape, np.inf)
    dt_o = distance_transform_edt(~eo) if eo.any() else np.full(eo.shape, np.inf)
    precision = float((dt_t[eo] <= tol).mean()) if eo.any() else 1.0
    recall = float((dt_o[et] <= tol).mean()) if et.any() else 1.0
    return precision, recall


def _band_power(img: Rgb, lo: float = 0.25, hi: float = 0.5) -> float:
    y = luma(img).astype(np.float64)
    y = y - y.mean()
    h, w = y.shape
    p = np.abs(np.fft.fft2(y * np.outer(np.hanning(h), np.hanning(w)))) ** 2
    fy, fx = np.fft.fftfreq(h)[:, None], np.fft.fftfreq(w)[None, :]
    r = np.sqrt(fx**2 + fy**2)
    band = (r >= lo) & (r < hi)
    return float(np.log10(p[band].mean() + 1e-12))


def high_band_gap(out: Rgb, ref: Rgb) -> float:
    """log10 of the output's power in the top octave (0.25 to 0.5 cycles a pixel) over the reference's: above 0 the
    output has more fine detail than the reference (oversharpening, noise), below 0 less (blur)."""
    return _band_power(out) - _band_power(ref)


def consistency(out: Rgb, given: Rgb) -> float:
    return psnr_y(pipeline_down(out, out.shape[0] // given.shape[0]), given)


class Perceptual:
    """LPIPS, DISTS and CLIP-IQA on one device, loaded once."""

    def __init__(self, device: str = "cpu") -> None:
        import lpips
        import open_clip
        import torch
        from DISTS_pytorch import DISTS

        self.torch, self.device = torch, device
        self.lpips = lpips.LPIPS(net="alex", verbose=False).to(device).eval()
        self.dists = DISTS().to(device).eval()
        model, _, _ = open_clip.create_model_and_transforms("ViT-B-32", pretrained="laion2b_s34b_b79k")
        self.clip = model.to(device).eval()
        tok = open_clip.get_tokenizer("ViT-B-32")
        # One antonym pair only. Measured on the benchmark (2026-10-17): "sharp / blurry" rewards unsharp masking
        # (truth 0.44 -> 0.63 once sharpened) and "natural / oversharpened with halos" moves the same way (0.86 ->
        # 0.87): CLIP has no notion of oversharpening. "real photograph / painted image" holds still under sharpening
        # (0.79 -> 0.81) and orders truth 0.79 > RRDB 0.65 > bicubic 0.61 > compact 0.27.
        prompts = [("a real aerial photograph of the ground", "an artificial, painted image")]
        with torch.no_grad():
            t = self.clip.encode_text(tok([p for pair in prompts for p in pair]).to(device))
        self.text = (t / t.norm(dim=-1, keepdim=True)).reshape(len(prompts), 2, -1)
        self.mean = torch.tensor([0.48145466, 0.4578275, 0.40821073], device=device).view(1, 3, 1, 1)
        self.std = torch.tensor([0.26862954, 0.26130258, 0.27577711], device=device).view(1, 3, 1, 1)

    def _t(self, imgs: Sequence[Rgb]):  # type: ignore[no-untyped-def]
        return self.torch.stack([self.torch.from_numpy(np.ascontiguousarray(i)).permute(2, 0, 1) for i in imgs]).to(
            self.device)

    def pair(self, outs: Sequence[Rgb], truths: Sequence[Rgb]) -> tuple[list[float], list[float]]:
        a, b = self._t(outs), self._t(truths)
        with self.torch.no_grad():
            lp = self.lpips(a * 2 - 1, b * 2 - 1).flatten().tolist()
            ds = self.dists(a, b, batch_average=False).flatten().tolist()
        return [float(v) for v in lp], [float(v) for v in ds]

    def clip_iqa(self, imgs: Sequence[Rgb]) -> list[float]:
        """Mean probability of the positive prompt of each antonym pair (CLIP-IQA's method), 224 x 224 centre."""
        x = self._t(imgs)
        x = self.torch.nn.functional.interpolate(x, size=(224, 224), mode="bicubic", align_corners=False,
                                                 antialias=True)
        x = (x - self.mean) / self.std
        with self.torch.no_grad():
            f = self.clip.encode_image(x)
            f = f / f.norm(dim=-1, keepdim=True)
            logits = 100.0 * self.torch.einsum("bd,pkd->bpk", f, self.text)
            p = logits.softmax(dim=-1)[..., 0].mean(dim=-1)
        return [float(v) for v in p.tolist()]


def measure(outs: Sequence[Rgb], truths: Sequence[Rgb], givens: Sequence[Rgb], perc: Perceptual) -> pd.DataFrame:
    """Every component for each (output, truth, input) triple."""
    lp, ds = perc.pair(outs, truths)
    iqa = perc.clip_iqa(outs)
    rows = []
    for k, (o, t, g) in enumerate(zip(outs, truths, givens)):
        p, r = edge_scores(o, t)
        rows.append({"psnr_y": min(PSNR_CAP, psnr_y(o, t)), "lpips": lp[k], "dists": ds[k],
                     "consistency": min(PSNR_CAP, consistency(o, g)), "edge_precision": p, "edge_recall": r,
                     "band_gap": abs(high_band_gap(o, t)), "clip_iqa": iqa[k]})
    return pd.DataFrame(rows)


def combine(table: pd.DataFrame, by: str = "sample", low: str = "bicubic", high: str = "truth") -> pd.DataFrame:
    """Each component of each row on the sample's scale (``low`` = 0, ``high`` = 1), and the ``score``.

    The score is the weighted mean of the scaled components, plus the hinge components' penalties weighted on the same
    footing (they add nothing at or above the truth, so the truth still scores 1 and bicubic, which agrees with its
    input, still 0)."""
    out = table.copy()
    lo = table[table.method == low].set_index(by)
    hi = table[table.method == high].set_index(by)
    total = np.zeros(len(out))
    wsum = sum(c.weight for c in COMPONENTS if c.name in table and not c.hinge_db)
    for c in COMPONENTS:
        if c.name not in table:
            continue
        a, b = out[by].map(lo[c.name]), out[by].map(hi[c.name])
        if c.hinge_db:
            n = (-((b - out[c.name]) / c.hinge_db).clip(lower=0.0)).clip(CLIP_LO, 0.0).fillna(0.0)
        else:
            span = (b - a).where((b - a).abs() > 1e-9, np.nan)
            n = ((out[c.name] - a) / span).clip(CLIP_LO, CLIP_HI).fillna(0.0)
        out[f"n_{c.name}"] = n
        total = total + c.weight * n.to_numpy()
    out["score"] = total / wsum
    return out


def bootstrap_ci(values: np.ndarray, groups: np.ndarray, n: int = 2000, seed: int = 0) -> tuple[float, float, float]:
    """Mean and 95 % interval, resampling whole groups (spatial blocks) so neighbours never count as independent."""
    rng = np.random.default_rng(seed)
    uniq = np.unique(groups)
    by = {g: values[groups == g] for g in uniq}
    means = []
    for _ in range(n):
        pick = rng.choice(uniq, len(uniq), replace=True)
        means.append(np.concatenate([by[g] for g in pick]).mean())
    return float(values.mean()), float(np.percentile(means, 2.5)), float(np.percentile(means, 97.5))
