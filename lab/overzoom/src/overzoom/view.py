# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Before / after, in a notebook: browse the dataset's comparisons one by one, at full resolution.

"Before" is the input as a client would see it stretched to the tile's size (bicubic), "after" is the truth. Three
ways to look at a pair:

- **side by side** — with, for the levels family, the parent before colour equalisation;
- **blink** — before and after stacked and alternating, which is where a difference jumps to the eye;
- **difference** — the absolute luminance difference, amplified.

Needs the ``[explore]`` extra (ipywidgets). Images are rendered on demand; nothing is written to disk.
"""

from __future__ import annotations

import base64
import io
from dataclasses import dataclass

import numpy as np
import pandas as pd
from PIL import Image

from .dataset import LEVELS, Dataset
from .metrics import luma
from .tiles import Rgb, to_uint8


def stretch(img: Rgb, scale: int) -> Rgb:
    """Bicubic enlargement: what a client shows when it has nothing finer."""
    if scale == 1:
        return img
    h, w = img.shape[:2]
    out = Image.fromarray(to_uint8(img)).resize((w * scale, h * scale), Image.Resampling.BICUBIC)
    return np.asarray(out, np.float32) / 255.0


def png(img: Rgb, zoom: int = 1) -> str:
    im = Image.fromarray(to_uint8(img))
    if zoom > 1:
        im = im.resize((im.width * zoom, im.height * zoom), Image.Resampling.NEAREST)
    buf = io.BytesIO()
    im.save(buf, "PNG")
    return "data:image/png;base64," + base64.b64encode(buf.getvalue()).decode()


def figure(src: str, caption: str) -> str:
    return (f'<figure style="margin:4px;display:inline-block;vertical-align:top">'
            f'<figcaption style="font:12px sans-serif;margin-bottom:2px">{caption}</figcaption>'
            f'<img style="image-rendering:pixelated;display:block" src="{src}"></figure>')


def blink(before: str, after: str, size: int, period: float = 1.2) -> str:
    """Two images stacked, the top one fading in and out: no script, a CSS animation."""
    return (f'<style>@keyframes ozblink{{0%,45%{{opacity:0}}50%,95%{{opacity:1}}100%{{opacity:0}}}}</style>'
            f'<figure style="margin:4px;display:inline-block">'
            f'<figcaption style="font:12px sans-serif;margin-bottom:2px">blink: before ↔ after</figcaption>'
            f'<div style="position:relative;width:{size}px;height:{size}px">'
            f'<img style="position:absolute;image-rendering:pixelated" src="{before}">'
            f'<img style="position:absolute;image-rendering:pixelated;'
            f'animation:ozblink {period}s infinite" src="{after}"></div></figure>')


@dataclass(frozen=True)
class Comparison:
    sample: int
    input: str


def comparisons(ds: Dataset, inputs: list[str], levels: str, sort: str, limit: int) -> list[Comparison]:
    """The comparisons to browse. ``levels``: "all", "valid" or "rejected"; ``sort``: an index column, or "index"."""
    idx: pd.DataFrame = ds.index
    if sort != "index":
        idx = idx.sort_values(sort, ascending=sort.startswith("level_ssim"))
    out: list[Comparison] = []
    for i, row in idx.iterrows():
        for name in inputs:
            if name == LEVELS and levels != "all" and bool(row["level_valid"]) != (levels == "valid"):
                continue
            out.append(Comparison(int(str(i)), name))
            if len(out) >= limit:
                return out
    return out


def render(ds: Dataset, c: Comparison, mode: str, zoom: int) -> str:
    row = ds.index.iloc[c.sample]
    scale = ds.spec.scale(c.input)
    before = stretch(ds.crop(ds.input(c.sample, c.input)), scale)
    after = ds.truth(c.sample)
    head = (f"<div style='font:13px sans-serif;margin:4px'><b>{ds.key(c.sample)}</b> · <b>{c.input}</b> · "
            f"texture {row.texture:.3f} · JPEG q{int(row.quality)}")
    if c.input == LEVELS:
        state = "same acquisition" if bool(row.level_valid) else "<b style='color:#b91c1c'>another acquisition</b>"
        head += (f" · SSIM-Y {row.level_ssim_y_raw:.2f}→{row.level_ssim_y:.2f}"
                 f" · ΔE {row.level_delta_e_raw:.1f}→{row.level_delta_e:.1f} · {state}")
    head += "</div>"
    size = after.shape[0] * zoom
    if mode == "blink":
        body = blink(png(before, zoom), png(after, zoom), size)
    elif mode == "difference":
        d = np.abs(luma(after) - luma(before))
        heat = np.clip(d * 8.0, 0, 1)
        rgb = np.stack([heat, heat * 0.3, 1.0 - heat], axis=-1).astype(np.float32)
        body = (figure(png(before, zoom), "before") + figure(png(after, zoom), "after")
                + figure(png(rgb, zoom), f"|ΔY| × 8 (mean {float(d.mean()):.3f})"))
    else:
        body = figure(png(before, zoom), f"before ({c.input}, ×{scale} bicubic)")
        if c.input == LEVELS:
            raw = stretch(ds.crop(ds.parent(c.sample, equalised=False)), scale)
            body = figure(png(raw, zoom), "parent as served (no equalisation)") + body
        body += figure(png(after, zoom), "after (the true tile)")
    return head + body


def browse(ds: Dataset, limit: int = 20000) -> object:
    """An interactive before/after browser over at most ``limit`` comparisons."""
    import ipywidgets as w
    from IPython.display import HTML, display

    names = ds.spec.inputs
    w_inputs = w.SelectMultiple(options=names, value=tuple(names), description="inputs", rows=len(names))
    w_levels = w.ToggleButtons(options=["all", "valid", "rejected"], value="all", description="levels")
    w_sort = w.Dropdown(options=["index", "texture", "level_ssim_y", "level_delta_e"], value="index",
                        description="sort")
    w_mode = w.ToggleButtons(options=["side by side", "blink", "difference"], value="side by side",
                             description="view")
    w_zoom = w.ToggleButtons(options=[1, 2], value=1, description="zoom")
    w_pos = w.IntSlider(0, 0, 0, description="#", continuous_update=False, layout=w.Layout(width="60%"))
    w_prev, w_next = w.Button(description="◀"), w.Button(description="▶")
    w_count = w.Label()
    out = w.Output()
    state: dict[str, list[Comparison]] = {"items": []}

    def refilter(_: object = None) -> None:
        state["items"] = comparisons(ds, list(w_inputs.value), w_levels.value, w_sort.value, limit)
        w_pos.max = max(0, len(state["items"]) - 1)
        w_pos.value = 0
        show()

    def show(_: object = None) -> None:
        items = state["items"]
        w_count.value = f"{len(items)} comparisons"
        out.clear_output(wait=True)
        with out:
            if items:
                display(HTML(render(ds, items[w_pos.value], w_mode.value, w_zoom.value)))

    def step(delta: int) -> None:
        w_pos.value = int(np.clip(w_pos.value + delta, 0, w_pos.max))

    w_prev.on_click(lambda _: step(-1))
    w_next.on_click(lambda _: step(1))
    for x in (w_inputs, w_levels, w_sort):
        x.observe(refilter, names="value")
    for x in (w_pos, w_mode, w_zoom):
        x.observe(show, names="value")
    ui = w.VBox([w.HBox([w_inputs, w.VBox([w_levels, w_sort, w_mode, w_zoom])]),
                 w.HBox([w_prev, w_pos, w_next, w_count]), out])
    refilter()
    return ui
