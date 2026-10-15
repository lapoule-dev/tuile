# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The methods under test, all behind one protocol: a batch of images in, the same batch enlarged out.

Needs the ``[infer]`` extra (torch, spandrel, huggingface_hub). Weights come from the Hugging Face Hub pinned to a
revision, so two runs load the same bytes; they are never stored with the code.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol

import torch
import torch.nn.functional as F


class Upscaler(Protocol):
    name: str
    scale: int

    def __call__(self, x: torch.Tensor) -> torch.Tensor:
        """``x``: (B, 3, H, W) float in [0, 1] on the method's device; returns (B, 3, H*scale, W*scale)."""
        ...


@dataclass(frozen=True)
class Weights:
    repo: str
    revision: str
    filename: str


# Two families and a floor: invention (GAN, trained on real-world damage) and fidelity (L1, clean reduction).
REGISTRY: dict[str, Weights] = {
    "realesrgan-x2": Weights("ai-forever/Real-ESRGAN", "8110204ebf8d25c031b66c26c2d1098aa831157e",
                             "RealESRGAN_x2.pth"),
    "swinir-classic-x2": Weights("licyk/sd-upscaler-models", "3a2094a57f456c41b79e5a4d196f2222c9940b89",
                                 "SwinIR/001_classicalSR_DF2K_s64w8_SwinIR-M_x2.pth"),
}


class Bicubic:
    def __init__(self, scale: int = 2) -> None:
        self.name, self.scale = "bicubic", scale

    def __call__(self, x: torch.Tensor) -> torch.Tensor:
        return F.interpolate(x, scale_factor=self.scale, mode="bicubic", align_corners=False).clamp(0, 1)


class Network:
    def __init__(self, name: str, weights: Weights, device: str, cache_dir: str | None = None) -> None:
        from huggingface_hub import hf_hub_download
        from spandrel import ModelLoader

        path = hf_hub_download(weights.repo, weights.filename, revision=weights.revision, cache_dir=cache_dir)
        self._model = ModelLoader().load_from_file(path).to(device).eval()
        self._half = device == "cuda" and bool(self._model.supports_half)
        if self._half:
            self._model = self._model.half()
        self.name, self.scale = name, int(self._model.scale)
        self._multiple = int(getattr(self._model.size_requirements, "multiple_of", 1) or 1)

    def __call__(self, x: torch.Tensor) -> torch.Tensor:
        h, w = x.shape[-2:]
        ph, pw = (-h) % self._multiple, (-w) % self._multiple
        with torch.no_grad():
            if ph or pw:
                x = F.pad(x, (0, pw, 0, ph), mode="reflect")
            y = self._model(x.half() if self._half else x).float()
            return y[..., :h * self.scale, :w * self.scale].clamp(0, 1)


class Consistent:
    """Wrap a x2 method so that its output, reduced 2x2, gives back its input exactly (CEM-style back-projection).

    It only removes what contradicts the input; it never adds detail.
    """

    def __init__(self, inner: Upscaler, iterations: int = 20) -> None:
        if inner.scale != 2:
            raise ValueError("consistency is defined for x2 only")
        self._inner, self._iterations = inner, iterations
        self.name, self.scale = f"{inner.name}+cem", 2

    def __call__(self, x: torch.Tensor) -> torch.Tensor:
        with torch.no_grad():
            y = self._inner(x)
            for _ in range(self._iterations):
                y = y + F.interpolate(x - F.avg_pool2d(y, 2), scale_factor=2, mode="bicubic", align_corners=False)
            return y.clamp(0, 1)


def build(names: list[str], device: str, cache_dir: str | None = None) -> list[Upscaler]:
    """Methods by name: ``bicubic``, a ``REGISTRY`` key, or either followed by ``+cem``."""
    out: list[Upscaler] = []
    for name in names:
        base, _, suffix = name.partition("+")
        m: Upscaler = Bicubic() if base == "bicubic" else Network(base, REGISTRY[base], device, cache_dir)
        out.append(Consistent(m) if suffix == "cem" else m)
    return out
