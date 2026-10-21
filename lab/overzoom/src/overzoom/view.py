# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Before / after, in a notebook: browse the dataset's comparisons, or a run's outputs, one by one, at full resolution.

"Before" is the input as a client would see it stretched to the tile's size (bicubic), "after" is the truth. Three
ways to look at a pair:

- **side by side** — with, for a level input, the ancestor as served, before colour equalisation;
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

from .dataset import Dataset
from .metrics import luma
from .runs import Run
from .score import METHODS, outputs
from .tiles import TILE, Rgb, to_uint8


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
    """The comparisons to browse. ``levels``: "all", or a pair class ("same", "tinted", "other") — then only level
    inputs whose pair has that class, synthetic ones left out; ``sort``: an index column, or "index"."""
    idx: pd.DataFrame = ds.index
    if sort != "index":
        idx = idx.sort_values(sort, ascending=sort.endswith("_structure"))
    out: list[Comparison] = []
    for i in idx.index:
        for name in inputs:
            if levels != "all" and ds.kind(int(i), name) != levels:
                continue
            out.append(Comparison(int(i), name))
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
    depth = ds.spec.depth(c.input)
    if depth is not None:
        lv = ds.spec.level(depth)
        kind = str(row[f"{lv}_class"])
        state = {"same": "same acquisition", "tinted": "<b style='color:#b45309'>same acquisition, other tint</b>",
                 "other": "<b style='color:#b91c1c'>another acquisition</b>"}[kind]
        head += (f" · detail corr {row[f'{lv}_structure']:.2f} · tint ΔE {row[f'{lv}_cast_raw']:.1f}"
                 f" · SSIM-Y {row[f'{lv}_ssim_y_raw']:.2f}→{row[f'{lv}_ssim_y']:.2f}"
                 f" · ΔE {row[f'{lv}_delta_e_raw']:.1f}→{row[f'{lv}_delta_e']:.1f} · {state}")
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
        if depth is not None:
            raw = stretch(ds.crop(ds.ancestor(c.sample, depth, equalised=False)), scale)
            if c.input != lv:
                body = figure(png(raw, zoom), f"{lv} as served") + body
        body += figure(png(after, zoom), "after (the true tile)")
    return head + body


def browse(ds: Dataset, limit: int = 20000) -> object:
    """An interactive before/after browser over at most ``limit`` comparisons."""
    import ipywidgets as w
    from IPython.display import HTML, display

    names = ds.spec.inputs
    w_inputs = w.SelectMultiple(options=names, value=tuple(names), description="inputs", rows=len(names))
    w_levels = w.ToggleButtons(options=["all", "same", "tinted", "other"], value="all", description="levels")
    scores = [c for c in ds.index.columns if c.endswith(("_structure", "_cast_raw", "_delta_e_raw", "_gain_max"))]
    w_sort = w.Dropdown(options=["index", "texture", *scores], value="index", description="sort")
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


def render_run(ds: Dataset, run: Run, scores: pd.DataFrame, sample: int, name: str, mode: str, zoom: int,
               a: str, b: str) -> str:
    """One comparison of a run: every method, or a blink between two of them (or one and the truth)."""
    out = outputs(ds, run, sample, name)
    out["truth"] = ds.truth(sample)
    rows = scores[(scores["sample"] == sample) & (scores["input"] == name)].set_index("method")
    kind = str(rows["kind"].iloc[0])
    head = (f"<div style='font:13px sans-serif;margin:4px'><b>{ds.key(sample)}</b> · <b>{name}</b> "
            f"(×{ds.spec.scale(name)}) · {kind}</div>")

    def caption(m: str) -> str:
        if m not in rows.index:
            return m
        r = rows.loc[m]
        lp = f" · LPIPS {r.lpips:.3f}" if "lpips" in rows else ""
        return f"{m}<br>PSNR-Y {r.psnr_y:.1f}{lp} · agrees {r.consistency:.1f}"

    if mode == "blink":
        body = blink(png(out[a], zoom), png(out[b], zoom), TILE * zoom).replace("before ↔ after", f"{a} ↔ {b}")
        body += figure(png(out[a], zoom), caption(a)) + figure(png(out[b], zoom), caption(b))
    else:
        given = stretch(ds.crop(ds.input(sample, name)), ds.spec.scale(name))
        body = figure(png(given, zoom), "input, pixels enlarged<br>&nbsp;")
        body += "".join(figure(png(img, zoom), caption(m)) for m, img in out.items())
    return head + body


def browse_run(ds: Dataset, run: Run, scores: pd.DataFrame) -> object:
    """An interactive browser over a run's comparisons, sortable by any measure of any method."""
    import ipywidgets as w
    from IPython.display import HTML, display

    methods = [*METHODS[:1], *sorted(set(scores.method) - {"bicubic"}), "truth"]
    measures = [c for c in ("lpips", "psnr_y", "ssim_y", "delta_e", "consistency") if c in scores]
    w_input = w.Dropdown(options=["all", *sorted(set(scores.input))], value="all", description="input")
    w_kind = w.Dropdown(options=["all", *sorted(set(scores.kind))], value="all", description="kind")
    w_method = w.Dropdown(options=methods[:-1], value=methods[-2], description="sort by")
    w_measure = w.Dropdown(options=[f"{m} gain over bicubic" for m in measures] + measures,
                           value=f"{measures[0]} gain over bicubic", description="measure")
    w_mode = w.ToggleButtons(options=["all methods", "blink"], value="all methods", description="view")
    w_a = w.Dropdown(options=methods, value="bicubic", description="blink a")
    w_b = w.Dropdown(options=methods, value=methods[-2], description="blink b")
    w_zoom = w.ToggleButtons(options=[1, 2], value=1, description="zoom")
    w_pos = w.IntSlider(0, 0, 0, description="#", continuous_update=False, layout=w.Layout(width="60%"))
    w_prev, w_next = w.Button(description="◀"), w.Button(description="▶")
    w_count = w.Label()
    out = w.Output()
    state: dict[str, list[tuple[int, str]]] = {"items": []}

    def refilter(_: object = None) -> None:
        s = scores
        if w_input.value != "all":
            s = s[s.input == w_input.value]
        if w_kind.value != "all":
            s = s[s.kind == w_kind.value]
        measure = str(w_measure.value).removesuffix(" gain over bicubic")
        v = s.pivot_table(index=["sample", "input"], columns="method", values=measure)
        key = v[w_method.value] - v["bicubic"] if w_measure.value.endswith("bicubic") else v[w_method.value]
        # best first: lower is better for LPIPS and ΔE
        key = key.sort_values(ascending=measure in ("lpips", "delta_e"))
        state["items"] = [(int(i), str(n)) for i, n in key.index]
        w_pos.max = max(0, len(state["items"]) - 1)
        w_pos.value = 0
        show()

    def show(_: object = None) -> None:
        items = state["items"]
        w_count.value = f"{len(items)} comparisons"
        out.clear_output(wait=True)
        with out:
            if items:
                i, n = items[w_pos.value]
                display(HTML(render_run(ds, run, scores, i, n, w_mode.value, w_zoom.value, w_a.value,
                                        w_b.value)))

    def step(delta: int) -> None:
        w_pos.value = int(np.clip(w_pos.value + delta, 0, w_pos.max))

    w_prev.on_click(lambda _: step(-1))
    w_next.on_click(lambda _: step(1))
    for x in (w_input, w_kind, w_method, w_measure):
        x.observe(refilter, names="value")
    for x in (w_pos, w_mode, w_a, w_b, w_zoom):
        x.observe(show, names="value")
    ui = w.VBox([w.HBox([w.VBox([w_input, w_kind]), w.VBox([w_method, w_measure]), w.VBox([w_mode, w_zoom]),
                         w.VBox([w_a, w_b])]),
                 w.HBox([w_prev, w_pos, w_next, w_count]), out])
    refilter()
    return ui
