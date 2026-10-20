# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A blind A/B rating page, to calibrate the judge against a person's eye.

One self-contained HTML file (images inlined, nothing fetched, nothing sent). Each screen shows the low-resolution
input enlarged by nearest neighbour, then two x4 outputs of the same tile, A and B, method names hidden and sides
drawn at random. The question: *which is the more realistic and the more faithful to the input?* Keys: ``A``,
``B``, ``E`` (equal), ``←`` back. At the end the page offers the answers as a JSON file to save; ``key.json``, kept
apart, says which method each side was.

The pairs mix strata (mountain, coast) and pair types (network against network, against bicubic, against a sharpened
copy, against the truth), so that a fit of the judge's weights on the answers sees every kind of disagreement.
"""

from __future__ import annotations

import base64
import io
import json
import random
from pathlib import Path

import numpy as np
from PIL import Image

from .imaging import Rgb, to_uint8

PAIR_TYPES: tuple[tuple[str, str], ...] = (
    ("rrdb-base", "compact-base"), ("rrdb-base", "bicubic"), ("compact-base", "bicubic"),
    ("rrdb-base", "rrdb-base+sharp"), ("truth", "rrdb-base"), ("truth", "truth+sharp"),
    ("noise", "bicubic"), ("truth", "compact-base"),
)


def _b64(img: Rgb, size: int = 384) -> str:
    buf = io.BytesIO()
    Image.fromarray(to_uint8(img)).resize((size, size), Image.Resampling.NEAREST).save(buf, "JPEG", quality=92)
    return base64.b64encode(buf.getvalue()).decode()


def build(items: list[dict[str, object]], out: Path, seed: int = 17501) -> None:
    """``items``: one per pair, ``{"tile", "stratum", "input", "a_name", "a", "b_name", "b"}`` (images as Rgb)."""
    rng = random.Random(seed)
    screens, key = [], []
    for k, it in enumerate(items):
        flip = rng.random() < 0.5
        left, right = ("b", "a") if flip else ("a", "b")
        screens.append({"input": _b64(it["input"]), "A": _b64(it[left]), "B": _b64(it[right])})  # type: ignore[arg-type]
        key.append({"pair": k, "tile": it["tile"], "stratum": it["stratum"], "A": it[f"{left}_name"],
                    "B": it[f"{right}_name"]})
    out.mkdir(parents=True, exist_ok=True)
    (out / "key.json").write_text(json.dumps(key, indent=1))
    (out / "index.html").write_text(PAGE.replace("__SCREENS__", json.dumps(screens)))


PAGE = """<!doctype html>
<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->
<html lang="fr"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Notation A/B</title>
<style>
 :root{--bg:#f6f5f2;--fg:#1d1d1b;--dim:#6b6a66;--accent:#2f6fde;--line:#d9d7d1}
 @media (prefers-color-scheme: dark){:root:not([data-theme="light"]){--bg:#151514;--fg:#ecebe7;--dim:#9b9a95;--accent:#7aa7ff;--line:#33322f}}
 body{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}
 h1{font-size:18px;margin:0 0 4px} .dim{color:var(--dim)}
 .row{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:12px;margin-top:12px;max-width:1200px}
 figure{margin:0} img{width:100%;image-rendering:pixelated;display:block;border:1px solid var(--line)}
 figcaption{font-weight:600;margin-top:4px}
 .btns{display:flex;gap:8px;margin-top:14px;flex-wrap:wrap}
 button{font:inherit;padding:8px 16px;border:1px solid var(--line);background:transparent;color:var(--fg);border-radius:6px;cursor:pointer}
 button.primary{border-color:var(--accent);color:var(--accent)}
 @media (max-width:700px){.row{grid-template-columns:1fr}}
</style></head><body>
<h1>Laquelle est la plus réaliste et la plus fidèle à l'entrée ?</h1>
<div class="dim">Touches : <b>A</b>, <b>B</b>, <b>E</b> (égal), <b>←</b> retour. Ni plus spectaculaire, ni plus nette : plus vraie.
 <span id="pos"></span></div>
<div class="row" id="row">
 <figure><img id="in" alt="entrée"><figcaption>Entrée (basse résolution)</figcaption></figure>
 <figure><img id="a" alt="A"><figcaption>A</figcaption></figure>
 <figure><img id="b" alt="B"><figcaption>B</figcaption></figure>
</div>
<div class="btns"><button class="primary" data-v="A">A</button><button class="primary" data-v="B">B</button>
 <button data-v="E">Égal</button><button data-v="back">← Retour</button></div>
<div class="btns" id="done" hidden><button class="primary" id="save">Enregistrer les réponses (JSON)</button></div>
<script>
const S=__SCREENS__; const ans=new Array(S.length).fill(null); let i=0; const t0=Date.now();
const show=()=>{ if(i>=S.length){document.getElementById('row').hidden=true;document.getElementById('done').hidden=false;
  document.getElementById('pos').textContent=' · terminé';return;}
  document.getElementById('row').hidden=false;document.getElementById('done').hidden=true;
  for(const k of ['in','a','b']) document.getElementById(k).src='data:image/jpeg;base64,'+S[i][k==='in'?'input':k.toUpperCase()];
  document.getElementById('pos').textContent=` · ${i+1} / ${S.length}`; };
const vote=v=>{ if(v==='back'){i=Math.max(0,i-1);show();return;} if(i<S.length){ans[i]={pair:i,choice:v,ms:Date.now()-t0};i++;show();} };
document.querySelectorAll('button[data-v]').forEach(b=>b.onclick=()=>vote(b.dataset.v));
document.addEventListener('keydown',e=>{const k=e.key.toUpperCase(); if(k==='A'||k==='B'||k==='E')vote(k); if(e.key==='ArrowLeft')vote('back');});
document.getElementById('save').onclick=()=>{const blob=new Blob([JSON.stringify(ans,null,1)],{type:'application/json'});
  const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download='ab-answers.json';a.click();};
show();
</script></body></html>
"""


def items_from_bench(bench_out: Path, bench_root: Path, n_per_stratum: int = 30, seed: int = 17501
                     ) -> list[dict[str, object]]:
    """Pairs drawn from the benchmark's outputs: tiles of mountain and coast blocks, pair types in turn."""
    from .bench import BLOCKS
    from .bench_eval import block_pairs
    from .judge import unsharp
    from .report import stratum

    rng = random.Random(seed)
    tiles: dict[str, list[tuple[str, int, int]]] = {"mountain": [], "coast": []}
    for b in BLOCKS:
        for p in block_pairs(bench_root, b):
            tiles[stratum(b.name)].append((b.name, int(p["i"]), int(p["j"])))  # type: ignore[call-overload]
    items: list[dict[str, object]] = []
    for st, pool in tiles.items():
        rng.shuffle(pool)
        for k, (name, i, j) in enumerate(pool[:n_per_stratum]):
            pa, pb = PAIR_TYPES[k % len(PAIR_TYPES)]
            sl = np.s_[j * 256:(j + 1) * 256, i * 256:(i + 1) * 256]

            def get(m: str) -> Rgb:
                base = m.removesuffix("+sharp")
                f = bench_out / name / ("truth.png" if base == "truth" else f"{base}-halo.png")
                img = np.asarray(Image.open(f), np.float32)[sl] / 255.0
                return unsharp(img, 1.0, 1.0) if m.endswith("+sharp") else img

            blk = next(b for b in BLOCKS if b.name == name)
            given = next(p for p in block_pairs(bench_root, blk) if p["i"] == i and p["j"] == j)["given"]
            items.append({"tile": f"{name}/{i}/{j}", "stratum": st, "input": given, "a_name": pa, "a": get(pa),
                          "b_name": pb, "b": get(pb)})
    rng.shuffle(items)
    return items
