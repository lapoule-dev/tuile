# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""A run as a detached job on the GPU machine, which stops the machine when it ends.

    python -m overzoom.batch --remote s3://bucket/prefix --dataset NAME --run NAME [--trial N] [--stop]

Nothing on the laptop has to stay connected: the job is started detached (``start_new_session``), it writes its
progress and its log to ``<remote>/runs/<run>/status.json`` and ``job.log``, and with ``--stop`` it stops the notebook
instance it runs on in a ``finally`` — after success, failure or a spent budget alike.

While it runs, its pid (with the boot it belongs to) is in ``PIDFILE``: the instance's idle auto-stop leaves a machine
with a live job alone, and a pid left over from a previous boot never counts.

``--selftest SECONDS`` replaces the run by a wait of that length (``--fail`` makes it raise at the end): the whole
stop path, on any machine, without data or GPU.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
import traceback
from collections.abc import Callable
from pathlib import Path

PIDFILE = Path.home() / "SageMaker" / ".batch.pid"
LAST = PIDFILE.with_name(".batch.last")
"""Touched at every step and when the job ends: the idle auto-stop counts from it, not from the boot."""
METADATA = Path("/opt/ml/metadata/resource-metadata.json")


def boot_id() -> str:
    try:
        return Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    except OSError:
        return "unknown"


def aws() -> str:
    """The AWS CLI by absolute path: a job may run without the login shell's PATH."""
    return shutil.which("aws") or "/usr/local/bin/aws"


def machine() -> str:
    """This notebook instance's name, or the host's."""
    try:
        return str(json.loads(METADATA.read_text())["ResourceName"])
    except (OSError, ValueError, KeyError):
        return os.uname().nodename


def stop_instance(log: Status) -> None:
    meta = json.loads(METADATA.read_text())
    name, region = meta["ResourceName"], meta["ResourceArn"].split(":")[3]
    log.write(f"stopping notebook instance {name}")
    r = subprocess.run([aws(), "sagemaker", "stop-notebook-instance", "--notebook-instance-name", name,
                        "--region", region], check=False, capture_output=True, text=True)
    log.write(f"stop request: exit {r.returncode} {r.stderr.strip()}")


class Status:
    """The job's log and state, local and mirrored to the bucket at every step."""

    def __init__(self, remote: str, run: str, home: Path, machine: str) -> None:
        self.remote, self.run = remote.rstrip("/"), run
        self.dir = home / "jobs" / run
        self.dir.mkdir(parents=True, exist_ok=True)
        # one log and one state per machine: several can share a run
        self.log_path, self.state_path = self.dir / f"job-{machine}.log", self.dir / f"status-{machine}.json"
        self.autostop = self.dir / f"autostop-{machine}.log"
        self.started = time.time()

    def write(self, msg: str) -> None:
        line = f"{time.strftime('%Y-%m-%dT%H:%M:%S%z')} {msg}"
        print(line, flush=True)
        with open(self.log_path, "a") as f:
            f.write(line + "\n")

    def state(self, phase: str, **extra: object) -> None:
        LAST.touch()
        self.state_path.write_text(json.dumps({"phase": phase, "updated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                                               "elapsed_s": round(time.time() - self.started), **extra}, indent=2))
        self.publish()

    def publish(self) -> None:
        autostop = PIDFILE.parent / "autostop.log"  # the instance's idle auto-stop, to read it after a stop
        if autostop.exists():
            shutil.copyfile(autostop, self.autostop)
        for p in (self.log_path, self.state_path, *([self.autostop] if autostop.exists() else [])):
            subprocess.run([aws(), "s3", "cp", str(p), f"{self.remote}/runs/{self.run}/{p.name}", "--only-show-errors"],
                           check=False)


def work(args: argparse.Namespace, status: Status) -> None:
    import torch

    from .archives import listed
    from .config import Lab
    from .dataset import Dataset
    from .infer import ToBucket, run_parallel
    from .runs import RunSpec

    lab = Lab(url_list=Path(), data=args.home / "data", remote=args.remote)
    status.state("fetching")
    lab.pull("archives", lab.data / "archives")
    lab.pull(f"datasets/{args.dataset}", lab.data / "datasets" / args.dataset)
    archives, dataset_dir = listed(lab.data / "archives"), lab.data / "datasets" / args.dataset
    inputs = tuple(Dataset.load_spec(dataset_dir)[0].inputs)
    gpus = torch.cuda.device_count()
    ranks = args.ranks or gpus
    workers = max(1, (os.cpu_count() or 4) // max(1, gpus) - 2)
    status.write(f"{gpus} GPUs (ranks {args.first_rank}..{args.first_rank + gpus - 1} of {ranks}), "
                 f"{workers} CPU workers each, {len(archives)} archives, {len(inputs)} inputs")

    def progress(phase: str) -> Callable[[str], None]:
        def report(msg: str) -> None:
            status.write(msg)
            status.state(phase, progress=msg)

        return report

    if args.trial:
        name = f"{args.run}-trial"
        trial = RunSpec(args.dataset, samples=args.trial * ranks, inputs=inputs, shard_tiles=16, budget=1e9)
        status.state("trial")
        run_parallel(trial, dataset_dir, archives, lab.data / "runs" / name, gpus, workers, ToBucket(lab, name),
                     every=20, progress=progress("trial"), first_rank=args.first_rank, ranks=ranks)
    run_dir = lab.data / "runs" / args.run
    lab.pull(f"runs/{args.run}", run_dir, include=["run.json", "index/*"])  # what a previous launch did
    spec = RunSpec(args.dataset, samples=None, inputs=inputs, shard_tiles=64, budget=args.budget * 3600)
    status.state("run")
    done = run_parallel(spec, dataset_dir, archives, run_dir, gpus, workers, ToBucket(lab, args.run), every=120,
                        progress=progress("run"), first_rank=args.first_rank, ranks=ranks)
    lab.put(run_dir / "run.json", f"runs/{args.run}/run.json")
    status.state("done", samples=int(done["sample"].nunique()), outputs=len(done))


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="overzoom.batch")
    p.add_argument("--remote", required=True)
    p.add_argument("--run", required=True)
    p.add_argument("--dataset")
    p.add_argument("--trial", type=int, default=0, help="tiles per GPU for a trial run first (0: none)")
    p.add_argument("--budget", type=float, default=4.0, help="hours; no batch starts past it")
    p.add_argument("--first-rank", type=int, default=0, help="with several machines: this one's first rank")
    p.add_argument("--ranks", type=int, default=0, help="with several machines: GPUs in all (0: this machine's)")
    p.add_argument("--home", type=Path, default=Path.home() / "SageMaker" / "overzoom")
    p.add_argument("--stop", action="store_true", help="stop this notebook instance when the job ends, whatever")
    p.add_argument("--selftest", type=float, default=0.0, help="wait that many seconds instead of running")
    p.add_argument("--fail", action="store_true", help="with --selftest: raise at the end")
    args = p.parse_args(argv)
    status = Status(args.remote, args.run, args.home, machine())
    PIDFILE.parent.mkdir(parents=True, exist_ok=True)
    PIDFILE.write_text(f"{os.getpid()} {boot_id()}\n")
    code = 0
    try:
        status.write(f"job started: {' '.join(sys.argv)}")
        if args.selftest:
            status.state("selftest", seconds=args.selftest)
            time.sleep(args.selftest)
            if args.fail:
                raise RuntimeError("selftest failure, as asked")
        else:
            work(args, status)
        status.state("done" if args.selftest else "finished")
    except BaseException as e:  # noqa: BLE001  (whatever ends the job, the machine stops)
        status.write(traceback.format_exc())
        status.state("failed", error=f"{type(e).__name__}: {e}")
        code = 1
    finally:
        PIDFILE.unlink(missing_ok=True)
        LAST.touch()
        if args.stop:
            try:
                stop_instance(status)
            except BaseException:  # noqa: BLE001  (logged; the idle auto-stop is the net)
                status.write(traceback.format_exc())
            status.publish()
    return code


if __name__ == "__main__":
    sys.exit(main())
