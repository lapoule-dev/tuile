# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A run: the networks on the dataset, on every GPU of the machine, within a time budget.

The GPU machine does nothing else: it rebuilds each input from the archives, runs every network on it, and stores the
output, cropped to the tile, as lossless PNG in the run's shards (see ``runs``). Measures, the bicubic floor and the
back-projection on the input are computed afterwards, elsewhere.

One process per GPU (``run_parallel``), each taking every n-th tile; in each, a pool of CPU workers rebuilds the
inputs of the next tiles while the GPU runs on the current ones. A shard is written every ``shard_tiles`` tiles and
handed to ``publish`` (to the bucket, after which it can leave the local disk); its index stays, so a run that stops —
budget spent, machine stopped — resumes after the last shard written.
"""

from __future__ import annotations

import multiprocessing as mp
import time
import traceback
import uuid
from collections.abc import Callable
from pathlib import Path

import numpy as np
import pandas as pd
import torch
from torch.utils.data import DataLoader
from torch.utils.data import Dataset as TorchDataset

from .config import Lab
from .dataset import Dataset
from .methods import Network, network
from .runs import RunSpec, ShardWriter, encode, results

Publish = Callable[[ShardWriter], None]


class ToBucket:
    """Publishes each shard and its index to ``<remote>/runs/<name>``, then frees the local disk of the shard (the
    index stays: it is what a resume reads). Picklable, so every rank's process can hold one."""

    def __init__(self, lab: Lab, name: str, keep: bool = False) -> None:
        self.lab, self.name, self.keep = lab, name, keep

    def __call__(self, writer: ShardWriter) -> None:
        self.lab.put(writer.path, f"runs/{self.name}/shards/{writer.path.name}")
        self.lab.put(writer.index_path, f"runs/{self.name}/index/{writer.index_path.name}")
        if not self.keep:
            writer.path.unlink()


class _Inputs(TorchDataset[tuple[int, dict[str, np.ndarray]]]):
    """Every input of a tile, rebuilt on a CPU worker."""

    def __init__(self, ds: Dataset, samples: list[int], inputs: tuple[str, ...]) -> None:
        self.ds, self.samples, self.inputs = ds, samples, inputs

    def __len__(self) -> int:
        return len(self.samples)

    def __getitem__(self, k: int) -> tuple[int, dict[str, np.ndarray]]:
        i = self.samples[k]
        return i, {name: self.ds.input(i, name) for name in self.inputs}


def todo(ds: Dataset, spec: RunSpec, run_dir: Path, rank: int = 0, ranks: int = 1) -> list[int]:
    """This rank's samples not yet in a shard, in run order. A rank's share is cut from the run's whole order, not
    from what is left, so ranks starting at different times never skip a sample."""
    done = set(results(run_dir)["sample"].astype(int))
    order = list(range(len(ds))) if spec.samples is None else ds.sample(spec.samples, spec.seed)
    return [i for i in order[rank::ranks] if i not in done]


def run(ds: Dataset, spec: RunSpec, run_dir: Path, device: str = "cuda", rank: int = 0, ranks: int = 1,
        workers: int = 0, publish: Publish | None = None, log: Callable[[str], None] = print) -> pd.DataFrame:
    """This rank's share of the run: every ``ranks``-th sample still to do, starting at ``rank``."""
    spec.save(run_dir)
    if device.startswith("cuda"):
        torch.backends.cuda.matmul.allow_tf32 = True  # fp32 matrix products on the tensor cores
        torch.backends.cudnn.benchmark = True  # input sizes are fixed per input: pick the fastest convolutions once
    mine = todo(ds, spec, run_dir, rank, ranks)
    log(f"rank {rank}: {len(mine)} samples to go")
    nets: dict[tuple[str, int], Network] = {}
    loader = DataLoader(_Inputs(ds, mine, spec.inputs), batch_size=spec.batch, num_workers=workers,
                        multiprocessing_context="fork" if workers else None, prefetch_factor=4 if workers else None)
    stamp = f"{time.strftime('%Y%m%d%H%M%S')}-{uuid.uuid4().hex[:6]}"  # two launches never share a shard name
    shards = 0
    writer = ShardWriter(run_dir, f"{stamp}-r{rank}-{shards:05d}")
    t0, n = time.time(), 0
    for ids, batch in loader:
        if n and time.time() - t0 > spec.budget:
            log(f"rank {rank}: budget of {spec.budget:.0f} s spent, {len(mine) - n} samples left")
            break
        chunk = [int(i) for i in ids]
        rows: dict[int, list[dict[str, object]]] = {i: [] for i in chunk}
        images: dict[int, list[bytes]] = {i: [] for i in chunk}
        for name in spec.inputs:
            scale = ds.spec.scale(name)
            full = batch[name].permute(0, 3, 1, 2).to(device)
            for family in spec.families:
                halo = min(ds.spec.halo, spec.net_halo.get(family, ds.spec.halo))
                c, margin = ds.spec.halo - halo, halo * scale
                x = full[..., c:full.shape[-2] - c, c:full.shape[-1] - c]
                if (family, scale) not in nets:
                    nets[family, scale] = network(family, scale, device)
                if device.startswith("cuda"):
                    torch.cuda.synchronize(device)
                t = time.time()
                y = nets[family, scale](x)
                if device.startswith("cuda"):
                    torch.cuda.synchronize(device)
                seconds = (time.time() - t) / len(chunk)
                out = y.permute(0, 2, 3, 1).cpu().numpy()
                for i, img in zip(chunk, out):
                    key = ds.key(i)
                    rows[i].append({"sample": i, "z": key.z, "x": key.x, "y": key.y, "input": name,
                                    "family": family, "scale": scale, "seconds": seconds})
                    images[i].append(encode(img[margin:img.shape[0] - margin, margin:img.shape[1] - margin]))
        for i in chunk:
            writer.add(rows[i], images[i])
        n += len(chunk)
        if writer.tiles >= spec.shard_tiles:
            writer.close()
            if publish is not None:
                publish(writer)
            shards += 1
            writer = ShardWriter(run_dir, f"{stamp}-r{rank}-{shards:05d}")
            log(f"rank {rank}: {n}/{len(mine)} samples, {time.time() - t0:.0f} s")
    writer.close()
    if publish is not None and writer.tiles:
        publish(writer)
    log(f"rank {rank}: {n}/{len(mine)} samples, {time.time() - t0:.0f} s")
    return results(run_dir)


def _rank_main(rank: int, ranks: int, gpu: int, spec: RunSpec, dataset_dir: Path, archives: list[Path],
               run_dir: Path, workers: int, publish: Publish | None) -> None:
    from .archives import load_zone

    log_path = run_dir / "logs" / f"rank{rank}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with open(log_path, "a") as f:

        def log(msg: str) -> None:
            f.write(f"{time.strftime('%H:%M:%S')} {msg}\n")
            f.flush()

        try:
            store, _ = load_zone(archives)
            ds = Dataset.load(dataset_dir, store)
            run(ds, spec, run_dir, f"cuda:{gpu}", rank, ranks, workers, publish, log)
            log("done")
        except BaseException:
            log(traceback.format_exc())
            raise


def run_parallel(spec: RunSpec, dataset_dir: Path, archives: list[Path], run_dir: Path, gpus: int, workers: int,
                 publish: Publish | None = None, every: float = 60.0,
                 progress: Callable[[str], None] = print, first_rank: int = 0,
                 ranks: int | None = None) -> pd.DataFrame:
    """The run on this machine's ``gpus`` GPUs at once, one process each with ``workers`` CPU workers; reports
    progress every ``every`` seconds from the shards' indexes. Raises if a rank fails (its traceback is in
    ``logs/rank<n>.log``).

    Several machines share a run by rank: with ``ranks`` in all, this one runs ranks ``first_rank`` to
    ``first_rank + gpus - 1``; each machine sees, and reports, only its own share."""
    ranks = ranks or gpus
    every_sample = len(pd.read_parquet(dataset_dir / "index.parquet")) if spec.samples is None else spec.samples
    total = -(-every_sample * gpus // ranks)
    spec.save(run_dir)
    ctx = mp.get_context("spawn")
    procs = [ctx.Process(target=_rank_main,
                         args=(first_rank + g, ranks, g, spec, dataset_dir, archives, run_dir, workers, publish))
             for g in range(gpus)]
    for p in procs:
        p.start()
    t0, start = time.time(), results(run_dir)["sample"].nunique()
    while any(p.is_alive() for p in procs):
        time.sleep(every)
        done = results(run_dir)["sample"].nunique()
        rate = (done - start) / max(1.0, time.time() - t0)
        eta = f", {(total - done) / rate / 60:.0f} min left" if rate > 0 else ""
        progress(f"{done}/{total} samples, {time.time() - t0:.0f} s{eta}")
        failed = [p for p in procs if not p.is_alive() and p.exitcode]
        if failed:
            for q in procs:
                q.terminate()
            raise RuntimeError(f"a rank failed (exit {failed[0].exitcode}): see {run_dir / 'logs'}")
    return results(run_dir)
