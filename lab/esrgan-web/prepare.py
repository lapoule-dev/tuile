# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Everything the page loads, written to ``data/`` (gitignored): the weights, a real tile and the Python reference.

    ../overzoom/.venv/bin/python prepare.py --lab ../overzoom

- ``weights.bin`` + ``weights.json``: SRVGGNetCompact's tensors as raw little-endian f32, in forward order;
- ``input.rgb`` / ``input.png``: one z17 tile of the lab's zone, 256 x 256, raw RGB bytes and the same as PNG;
- ``truth.png``: the z19 ground under that tile, 1024 x 1024 — what a x4 should come close to;
- ``ref.f32``: the network's output on the input, from the PyTorch reference (spandrel, f32 on CPU), CHW, unclamped;
- ``meta.json``: the tile, the weights' digest, the reference's time.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import time
from pathlib import Path

import numpy as np
import pandas as pd
import torch
from PIL import Image
from spandrel import ModelLoader

from overzoom.archives import listed, load_zone
from overzoom.tiles import TILE, TileKey, to_uint8

WEIGHTS_URL = "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/realesr-general-x4v3.pth"
WEIGHTS_SHA256 = "8dc7edb9ac80ccdc30c3a5dca6616509367f05fbc184ad95b731f05bece96292"


def weights(out: Path) -> Path:
    path = out / "realesr-general-x4v3.pth"
    if not path.exists():
        torch.hub.download_url_to_file(WEIGHTS_URL, str(path))
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != WEIGHTS_SHA256:
        raise SystemExit(f"{path}: sha256 {digest}, expected {WEIGHTS_SHA256}")
    return path


def pick_tile(dataset: Path) -> TileKey:
    """The most textured z17 tile whose sixteen z19 tiles are all in the dataset, and which the z17 level agrees with."""
    ix = pd.read_parquet(dataset / "index.parquet")
    ix = ix.assign(px=ix.x // 4, py=ix.y // 4, same=ix.z17_class == "same")
    g = ix.groupby(["px", "py"]).agg(n=("x", "size"), texture=("texture", "mean"), same=("same", "all"))
    g = g[(g.n == 16) & g.same].sort_values("texture", ascending=False)
    px, py = g.index[0]
    return TileKey(17, int(px), int(py))


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--lab", type=Path, required=True, help="the overzoom lab directory (its data/)")
    p.add_argument("--dataset", default="luchon-z19-v3")
    p.add_argument("--out", type=Path, default=Path(__file__).parent / "data")
    args = p.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    model = ModelLoader().load_from_file(weights(args.out)).model.eval()
    tensors = [(n, t.detach().float().contiguous().numpy()) for n, t in model.state_dict().items()]
    blob, manifest, offset = bytearray(), [], 0
    for name, t in tensors:
        manifest.append({"name": name, "shape": list(t.shape), "offset": offset})
        blob += t.astype("<f4").tobytes()
        offset += t.size
    (args.out / "weights.bin").write_bytes(bytes(blob))
    (args.out / "weights.json").write_text(json.dumps(manifest, indent=1))

    key = pick_tile(args.lab / "data" / "datasets" / args.dataset)
    store, _ = load_zone(listed(args.lab / "data" / "archives"))
    tile = store.rgb(key)
    truth = store.window(19, key.x * 4 * TILE, key.y * 4 * TILE, 4 * TILE, 4 * TILE)
    assert tile is not None and truth is not None
    pixels = to_uint8(tile)
    (args.out / "input.rgb").write_bytes(pixels.tobytes())
    Image.fromarray(pixels).save(args.out / "input.png")
    Image.fromarray(to_uint8(truth)).save(args.out / "truth.png")

    # the network sees the bytes the page sees: u8 / 255
    x = torch.from_numpy(pixels.astype(np.float32) / 255.0).permute(2, 0, 1)[None]
    with torch.inference_mode():
        model(x)  # warm
        t0 = time.perf_counter()
        y = model(x)
        seconds = time.perf_counter() - t0
    out = y[0].numpy().astype("<f4")
    (args.out / "ref.f32").write_bytes(out.tobytes())
    Image.fromarray(to_uint8(np.clip(out.transpose(1, 2, 0), 0, 1))).save(args.out / "ref.png")
    (args.out / "meta.json").write_text(json.dumps({
        "tile": str(key), "dataset": args.dataset, "weights": WEIGHTS_URL, "sha256": WEIGHTS_SHA256,
        "params": int(sum(t.size for _, t in tensors)), "num_conv": int(model.num_conv), "scale": int(model.upscale),
        "reference": f"PyTorch {torch.__version__}, f32, CPU", "reference_seconds": round(seconds, 3),
    }, indent=1))
    print(f"tile {key}, {len(tensors)} tensors, {offset} params, reference {seconds:.2f} s")


if __name__ == "__main__":
    main()
