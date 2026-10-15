# %% [markdown]
# # 01 — The dataset: build it, check it, look at it
#
# Runs here, on a laptop: no GPU needed. It fetches the zone's archives, builds the index (one row per tile of the
# deepest level that has its neighbours and its parent context), checks every parent/child pair, and opens the
# before/after browser. Then it pushes the index to the bucket for `02_inference` (GPU) to read.
#
# Two families of inputs on the same tiles:
#
# - **synthetic** — the tile damaged by us (reduction ×2, blur, noise, JPEG at the source's quality), the control;
# - **levels** — the real parent one zoom up, brought to the child's colours, then checked: same structure and
#   little remaining colour difference, or it is *another acquisition* and is flagged, never measured.
#
# Nothing image-sized is written: inputs are rebuilt on demand from the archives, deterministically.

# %%
from pathlib import Path

import matplotlib.pyplot as plt
import pandas as pd

from overzoom.archives import fetch_listed, load_zone
from overzoom.config import Lab
from overzoom.dataset import Dataset, DatasetSpec, build_index
from overzoom.view import browse

lab = Lab.load(Path("../lab.json"))
NAME = "luchon-z19-v1"
spec = DatasetSpec(zoom=19)
print(lab.data, "→", lab.remote)

# %% [markdown]
# ## 1. The zone's archives, merged newest first

# %%
archives = fetch_listed(lab.url_list, lab.data / "archives")
store, infos = load_zone(archives)
print(f"{len(archives)} archives, {len(store)} tiles")
pd.DataFrame([(i.key, i.tiles, i.min_zoom, i.max_zoom) for i in infos], columns=["archive", "tiles", "min z", "max z"])

# %% [markdown]
# ## 2. The index
#
# Every textured tile of z19 with its neighbours (for the halo) and its parent context, and the check of its pair.

# %%
index = build_index(store, spec)
ds = Dataset(store, spec, index)
print(f"{len(ds)} samples, {int(index.level_valid.sum())} with a valid z18 pair "
      f"({100 * index.level_valid.mean():.0f} %)")
print(f"{len(ds) * len(spec.inputs)} comparisons in all")

# %% [markdown]
# ## 3. Do the two levels match?
#
# Before and after colour equalisation, per pair: structure (SSIM on luminance) and colour (ΔE). The thresholds of the
# policy are the dashed lines; set them by looking at the rejected pairs in the browser below, not by eye on the
# histogram.

# %%
fig, ax = plt.subplots(1, 2, figsize=(12, 4))
ax[0].hist(index.level_ssim_y_raw, 60, alpha=0.5, label="as served")
ax[0].hist(index.level_ssim_y, 60, alpha=0.5, label="equalised")
ax[0].axvline(spec.policy.min_ssim_y, ls="--", c="k")
ax[0].set_title("SSIM-Y, parent vs reduced child", loc="left")
ax[0].legend()
ax[1].hist(index.level_delta_e_raw, 60, alpha=0.5, label="as served")
ax[1].hist(index.level_delta_e, 60, alpha=0.5, label="equalised")
ax[1].axvline(spec.policy.max_delta_e, ls="--", c="k")
ax[1].set_title("ΔE (CIEDE2000, low-passed)", loc="left")
ax[1].legend()
plt.show()

# %% [markdown]
# ## 4. Before / after
#
# Filter by input family and by level status; sort by texture or by pair score (the worst pairs first with
# `level_ssim_y`). *Blink* alternates before and after in place: the fastest way to see what changed.

# %%
browse(ds, limit=20000)

# %% [markdown]
# ## 5. Save and share
#
# The index and its manifest go to the bucket; the GPU machine fetches the same archives and rebuilds the same inputs.

# %%
out = lab.data / "datasets" / NAME
ds.save(out, [p.name for p in archives])
lab.push(out, f"datasets/{NAME}")
print(out, "→", f"{lab.remote}/datasets/{NAME}")
