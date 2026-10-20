# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The networks under test, behind one protocol: a batch of images in, the same batch enlarged out.

Only what needs a GPU lives here; the bicubic floor and the back-projection on the input are computed elsewhere
(``score``), from the networks' saved outputs. Needs the ``[infer]`` extra (torch, spandrel, huggingface_hub).
Weights come from the Hugging Face Hub pinned to a revision, so two runs load the same bytes; they are never stored
with the code.
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


_ESRGAN = ("ai-forever/Real-ESRGAN", "8110204ebf8d25c031b66c26c2d1098aa831157e")
_UPSCALERS = ("licyk/sd-upscaler-models", "3a2094a57f456c41b79e5a4d196f2222c9940b89")

# Two families, each with a native model per factor: fidelity (SwinIR, L1 on a clean bicubic reduction: it does not
# invent) and invention (Real-ESRGAN, a GAN trained on simulated real-world damage: blur, noise, JPEG).
REGISTRY: dict[str, dict[int, Weights]] = {
    "swinir": {s: Weights(*_UPSCALERS, f"SwinIR/001_classicalSR_DF2K_s64w8_SwinIR-M_x{s}.pth") for s in (2, 4, 8)},
    "realesrgan": {s: Weights(*_ESRGAN, f"RealESRGAN_x{s}.pth") for s in (2, 4, 8)},
}


class Network:
    def __init__(self, name: str, weights: Weights, device: str, cache_dir: str | None = None) -> None:
        from huggingface_hub import hf_hub_download
        from spandrel import ModelLoader

        path = hf_hub_download(weights.repo, weights.filename, revision=weights.revision, cache_dir=cache_dir)
        self._model = ModelLoader().load_from_file(path).to(device).eval()
        cuda = device.startswith("cuda")
        self._half = cuda and bool(self._model.supports_half)
        # without fp16 (SwinIR overflows in it), bf16 autocast: the tensor cores, at 0.1 of a level on average
        self._bf16 = cuda and not self._half and bool(self._model.supports_bfloat16)
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
            with torch.autocast("cuda", dtype=torch.bfloat16, enabled=self._bf16):
                y: torch.Tensor = self._model(x.half() if self._half else x).float()
            return y[..., :h * self.scale, :w * self.scale].clamp(0, 1)


def network(family: str, scale: int, device: str, cache_dir: str | None = None) -> Network:
    """The ``REGISTRY`` model of ``family`` for ``scale``, loaded on ``device``."""
    net = Network(family, REGISTRY[family][scale], device, cache_dir)
    if net.scale != scale:
        raise ValueError(f"{family} x{scale}: the weights are x{net.scale}")
    return net
