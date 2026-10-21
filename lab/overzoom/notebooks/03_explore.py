# %% [markdown]
# # 03 — Exploration: what the networks did, measured and looked at
#
# Runs here, on a laptop. It fetches a run's outputs from the bucket, adds the bicubic floor and the back-projected
# variants, measures everything against the true tile (cached in the run's directory), then shows it.
#
# Two questions, one per family of methods:
#
# - **fidelity** (SwinIR): does it get closer to the truth than bicubic, at ×2, ×4, ×8?
# - **invention** (Real-ESRGAN): does the detail it adds look like the ground (LPIPS), and does it still agree with
#   its input (`consistency`: the output reduced back to the input's size, against the input)?
#
# `other` comparisons (another acquisition at that level) are left out of the tables and kept in the browser.

# %%
%load_ext autoreload
%autoreload 2

from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np
import pandas as pd

from overzoom.archives import listed, load_zone
from overzoom.config import Lab
from overzoom.dataset import Dataset
from overzoom.metrics import radial_spectrum
from overzoom.runs import Run, results
from overzoom.score import lpips_metric, outputs, score
from overzoom.view import browse_run

lab = Lab.load(Path("../lab.json"))
DATASET, RUN = "luchon-z19-v3", "luchon-v3-all"
SHARDS = 2  # shards fetched and measured here (64 tiles each, spread over the zone); the rest stays in the bucket
run_dir = lab.data / "runs" / RUN
lab.pull(f"runs/{RUN}", run_dir, include=["run.json", "index/*"])
names = sorted(results(run_dir)["shard"].unique())
names = names[::max(1, len(names) // SHARDS)][:SHARDS]
lab.pull(f"runs/{RUN}/shards", run_dir / "shards", include=[f"{n}.parquet" for n in names])
store, _ = load_zone(listed(lab.data / "archives"))
ds = Dataset.load(lab.data / "datasets" / DATASET, store)
run = Run(run_dir)
spec = run.spec
print(f"{run.index['sample'].nunique()} samples in the run, {run.local()['sample'].nunique()} here")

# %% [markdown]
# ## 1. Measures
#
# Computed once, then read from `scores.parquet` until the run gets new outputs.

# %%
scores = score(ds, run, lpips_metric())
valid = scores[scores.kind != "other"]
print(f"{scores[['sample', 'input']].drop_duplicates().shape[0]} comparisons, {len(valid) // 5} valid")
order = [n for n in spec.inputs]
methods = ["bicubic", *[m for f in spec.families for m in (f, f + "+bp")]]
table = valid.groupby(["input", "method"])[["psnr_y", "ssim_y", "lpips", "delta_e", "consistency"]].median()
table.unstack("method").reindex(order).round(3)

# %% [markdown]
# ## 2. Distortion against perception
#
# One panel per input; each method at its median (PSNR-Y, LPIPS). Up and to the left is worse; the ideal is bottom
# right. A method that invents moves down (better LPIPS) and often left (worse PSNR); back-projection pulls it back
# towards its input.

# %%
med = valid.groupby(["input", "method"])[["psnr_y", "lpips"]].median()
cols = 5
fig, ax = plt.subplots(-(-len(order) // cols), cols, figsize=(3.4 * cols, 3.4 * -(-len(order) // cols)),
                       squeeze=False)
for k, name in enumerate(order):
    a = ax[k // cols, k % cols]
    for m in methods:
        p = med.loc[(name, m)]
        a.scatter(p.psnr_y, p.lpips, label=m, s=40)
    a.set_title(name)
    a.set_xlabel("PSNR-Y (dB)")
    a.set_ylabel("LPIPS")
ax[0, 0].legend(fontsize=8)
plt.tight_layout()
plt.show()

# %% [markdown]
# ## 3. Gain over bicubic, comparison by comparison
#
# The median hides the spread: how often does a network beat the floor, per input?

# %%
lp = valid.pivot_table(index=["sample", "input"], columns="method", values="lpips")
win = pd.DataFrame({m: (lp[m] < lp["bicubic"]).groupby("input").mean() for m in methods[1:]}).reindex(order)
win.style.format("{:.0%}").background_gradient(vmin=0, vmax=1, cmap="RdYlGn")

# %% [markdown]
# ## 4. Spectra
#
# Mean radial luminance spectrum per method, against the truth's. Detail a method adds shows as high-frequency
# power; whether it follows the truth's slope or bends away (noise, ringing, periodic patterns) is the question.

# %%
fig, ax = plt.subplots(-(-len(order) // cols), cols, figsize=(3.4 * cols, 3.2 * -(-len(order) // cols)),
                       squeeze=False)
picks = valid[["sample", "input"]].drop_duplicates()
for k, name in enumerate(order):
    samples = picks[picks.input == name]["sample"].head(20).tolist()
    curves: dict[str, list[np.ndarray]] = {}
    for i in samples:
        imgs = outputs(ds, run, int(i), name)
        imgs["truth"] = ds.truth(int(i))
        for m, img in imgs.items():
            f, p = radial_spectrum(img)
            curves.setdefault(m, []).append(p)
    for m, ps in curves.items():
        a = ax[k // cols, k % cols]
        a.plot(f, np.mean(ps, axis=0), label=m, lw=2.5 if m == "truth" else 1, c="k" if m == "truth" else None)
    ax[k // cols, k % cols].set_title(name)
    ax[k // cols, k % cols].set_xlabel("cycles / pixel")
ax[0, 0].set_ylabel("log power")
ax[0, 0].legend(fontsize=8)
plt.tight_layout()
plt.show()

# %% [markdown]
# ## 5. Look at them
#
# Sorted by default by the LPIPS gain of the last method over bicubic, best first; reverse the slider to see where it
# does worst. *Blink* alternates two methods, or one and the truth, in place.

# %%
browse_run(ds, run, scores)
