# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Archive order, the run's sample, budget and resume, and its scoring — with a stand-in for the networks."""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
import pytest
import torch
import torch.nn.functional as F
from conftest import ground

from overzoom import infer
from overzoom.archives import listed
from overzoom.dataset import Dataset, DatasetSpec, build_index
from overzoom.metrics import psnr_y
from overzoom.runs import Run, RunSpec, results
from overzoom.score import back_project, bicubic, reduce, score
from overzoom.tiles import TileStore


def test_archives_merge_newest_first(tmp_path: Path) -> None:
    # The zone manifest lists archives oldest first; the last one wins, so they merge in reverse.
    zdir = tmp_path / "a" / "zones"
    zdir.mkdir(parents=True)
    keys = [f"a/zones/{n}.pmtiles" for n in ("old", "mid", "new")]
    for k in keys:
        (tmp_path / k).write_bytes(b"x")
    (tmp_path / "a" / "zones" / "stray.pmtiles").write_bytes(b"x")
    (zdir / "manifest.json").write_text(json.dumps({"archives": [{"key": k} for k in keys]}))
    assert [p.stem for p in listed(tmp_path)] == ["new", "mid", "old", "stray"]


def test_back_projection_gives_the_input_back() -> None:
    truth = ground(64)
    given = reduce(truth, 4)
    wrong = np.asarray(bicubic(given, 4) + 0.08, np.float32)  # too bright: an error the reduction keeps
    assert psnr_y(reduce(wrong, 4), given) < 30
    fixed = back_project(wrong, given, 4)
    assert psnr_y(reduce(fixed, 4), given) > 45
    # it removes the contradiction, nothing else: closer to the truth than before
    assert psnr_y(fixed, truth) > psnr_y(wrong, truth)


class Stretch:
    """Stands in for a network: bicubic, at the asked factor."""

    def __init__(self, family: str, scale: int, device: str, cache_dir: str | None = None) -> None:
        self.name, self.scale = family, scale

    def __call__(self, x: torch.Tensor) -> torch.Tensor:
        return F.interpolate(x, scale_factor=self.scale, mode="bicubic", align_corners=False).clamp(0, 1)


@pytest.fixture()
def ds(zone: tuple[TileStore, np.ndarray]) -> Dataset:
    store, _ = zone
    spec = DatasetSpec(min_texture=0.0)
    return Dataset(store, spec, build_index(store, spec, progress=False))


def test_sample_is_proportional_and_shuffled(ds: Dataset) -> None:
    s = ds.sample(20, seed=1)
    assert len(s) == len(set(s)) == 20 and s != sorted(s)
    assert ds.sample(20, seed=1) == s


def test_run_shards_stops_on_budget_and_resumes(ds: Dataset, tmp_path: Path,
                                                 monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(infer, "network", Stretch)
    # family "a" sees a narrower halo than the dataset's: its output is still the tile, in place
    spec = RunSpec("synthetic", samples=6, inputs=("x2", "x8+all", "z17-eq"), families=("a", "b"), batch=2,
                   shard_tiles=2, budget=0.0, net_halo={"a": 8})
    published: list[str] = []
    first = infer.run(ds, spec, tmp_path, device="cpu", publish=lambda w: published.append(w.name), log=lambda _: None)
    # a spent budget still lets the first batch through, then stops; its shard is published
    assert len(first) == 2 * 3 * 2 and len(published) == 1
    whole = RunSpec(**{**spec.__dict__, "budget": 1e9})
    # two ranks share what is left, each with its own CPU workers
    for rank in (0, 1):
        infer.run(ds, whole, tmp_path, device="cpu", rank=rank, ranks=2, workers=2, log=lambda _: None)
    done = results(tmp_path)
    assert len(done) == 6 * 3 * 2 and not done.duplicated(["sample", "input", "family"]).any()
    assert sorted(set(done["sample"])) == sorted(ds.sample(6, spec.seed))
    # every output reads back from its shard, cropped to the tile
    run = Run(tmp_path)
    i = int(done["sample"].iloc[0])
    img = run.output("a", "x8+all", i)
    assert img.shape == (256, 256, 3)
    assert psnr_y(img, ds.crop(bicubic(ds.input(i, "x8+all"), 8), 8)) > 35
    assert psnr_y(run.output("b", "x8+all", i), img) > 35  # same tile, whatever the halo
    scores = score(ds, run, progress=False)
    assert len(scores) == 6 * 3 * (1 + 2 * 2)
    m = scores.groupby(["input", "method"]).psnr_y.median()
    # the stand-in is bicubic: its outputs match the bicubic floor, and back-projection agrees with the input
    assert abs(m["x2", "a"] - m["x2", "bicubic"]) < 0.5
    c = scores.groupby("method").consistency.median()
    assert c["a+bp"] > c["a"] and c["a+bp"] > 40
    # a shard left in the bucket is not scored
    (tmp_path / "shards" / f"{run.index['shard'].iloc[0]}.parquet").unlink()
    assert len(Run(tmp_path).local()) < len(done)


def test_batch_job_always_reaches_its_stop(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # Whatever ends the job — success or failure — its pid leaves the pid file and the stop is asked for.
    from overzoom import batch

    stops: list[str] = []
    monkeypatch.setattr(batch, "PIDFILE", tmp_path / ".batch.pid")
    monkeypatch.setattr(batch, "LAST", tmp_path / ".batch.last")
    monkeypatch.setattr(batch, "stop_instance", lambda status: stops.append(status.run))
    monkeypatch.setattr(batch.Status, "publish", lambda self: None)
    common = ["--remote", "s3://nowhere", "--home", str(tmp_path), "--selftest", "0.01", "--stop"]
    assert batch.main(["--run", "ok", *common]) == 0
    assert batch.main(["--run", "ko", "--fail", *common]) == 1
    assert stops == ["ok", "ko"] and not (tmp_path / ".batch.pid").exists()
    assert (tmp_path / ".batch.last").exists()  # the idle auto-stop counts from the job's end
    state = json.loads((tmp_path / "jobs" / "ko" / f"status-{batch.machine()}.json").read_text())
    assert state["phase"] == "failed" and "selftest failure" in state["error"]
    # without --stop, nothing is stopped
    assert batch.main(["--run", "keep", *common[:-1]]) == 0 and stops == ["ok", "ko"]
