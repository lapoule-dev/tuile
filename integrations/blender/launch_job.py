#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Launch a parametric render job on a RunPod GPU pod.

The seed of the farm submitter: one invocation = one pod = one video. Every
render parameter and every sizing knob is a flag; the job itself is
render_job.sh baked into the image, driven purely by environment.

    ./launch_job.py --stage gate-anim-60s.usda --frames 1:1440 \
        --gpu-type "NVIDIA GeForce RTX 5090" --gpu-count 4 \
        --samples 128 --threshold 0.05 --extra "--demo-fixups"

Credentials: RUNPOD_API_KEY from the environment or a .env file next to the
repo root. Never printed.
"""

import argparse
import base64
import gzip
import json
import os
import pathlib
import urllib.error
import urllib.request

API = "https://api.runpod.io/v2"


def api_key():
    if os.environ.get("RUNPOD_API_KEY"):
        return os.environ["RUNPOD_API_KEY"]
    root = pathlib.Path(__file__).resolve()
    for parent in root.parents:
        env = parent / ".env"
        if env.is_file():
            for line in env.read_text().splitlines():
                if line.startswith("RUNPOD_API_KEY="):
                    return line.split("=", 1)[1].strip()
    raise SystemExit("RUNPOD_API_KEY introuvable (env ou .env)")


def call(key, method, path, body=None):
    req = urllib.request.Request(
        f"{API}{path}",
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"Authorization": f"Bearer {key}",
                 "Content-Type": "application/json",
                 "User-Agent": "tuile-render-farm/0.1"})
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        raise SystemExit(f"{method} {path} -> {e.code}: {e.read().decode()[:300]}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--stage", required=True, help=".usda à rendre (embarquée gzip+b64)")
    p.add_argument("--frames", required=True, help="A:B inclus")
    p.add_argument("--name", default="render-job")
    p.add_argument("--image",
                   default="harbor.sportstracklive.com/stl/blender-render:5.1")
    p.add_argument("--registry-name", default="harbor-stl",
                   help="credential registre déjà enregistré chez RunPod")
    p.add_argument("--gpu-type", default="NVIDIA GeForce RTX 5090")
    p.add_argument("--gpu-count", type=int, default=1)
    p.add_argument("--procs-per-gpu", type=int, default=4)
    p.add_argument("--cuda-min", default="13.2",
                   help="versions CUDA hôte acceptées, à partir de celle-ci")
    p.add_argument("--cloud", default="COMMUNITY", choices=["COMMUNITY", "SECURE"])
    p.add_argument("--disk", type=int, default=30)
    p.add_argument("--engine", default="native", choices=["native", "hydra"],
                   help="hydra = manifeste + procéduraux (image blender-globe; "
                        "CESIUM_ION_TOKEN local requis, passé au pod sans "
                        "jamais être affiché)")
    p.add_argument("--tier", default="cycles", choices=["cycles", "eevee"])
    p.add_argument("--upload-url", default="",
                   help="URL PUT présignée : le pod y dépose la vidéo finie")
    p.add_argument("--width", type=int, default=1920)
    p.add_argument("--samples", type=int, default=128)
    p.add_argument("--threshold", type=float, default=0.05)
    p.add_argument("--batch-frames", type=int, default=60)
    p.add_argument("--extra", default="", help="args supplémentaires de render_usd.py")
    p.add_argument("--ssh-pubkey", default="",
                   help="clé publique à autoriser (rapatriement scp)")
    args = p.parse_args()
    if args.engine == "hydra" and args.image == p.get_default("image"):
        args.image = "harbor.sportstracklive.com/stl/blender-globe:5.1-su"

    key = api_key()
    regs = call(key, "GET", "/registries")
    reg = next((r for r in regs.get("registries", [])
                if r.get("name") == args.registry_name), None)
    if reg is None:
        raise SystemExit(f"credential registre '{args.registry_name}' absent — "
                         "à créer une fois (secret hors conversation)")

    stage_b64 = base64.b64encode(
        gzip.compress(pathlib.Path(args.stage).read_bytes())).decode()

    known = ["13.2", "13.3"]
    cuda = [v for v in known if v >= args.cuda_min] or [args.cuda_min]

    env = {
        "JOB_STAGE_B64_GZ": stage_b64,
        "JOB_FRAMES": args.frames,
        "JOB_ENGINE": args.engine,
        "JOB_TIER": args.tier,
        "JOB_WIDTH": str(args.width),
        "JOB_SAMPLES": str(args.samples),
        "JOB_THRESHOLD": str(args.threshold),
        "JOB_GPUS": str(args.gpu_count),
        "JOB_PROCS_PER_GPU": str(args.procs_per_gpu),
        "JOB_BATCH_FRAMES": str(args.batch_frames),
        "JOB_EXTRA_ARGS": args.extra,
        "JOB_OUT": "/out/render.mp4",
    }
    if args.ssh_pubkey:
        env["JOB_SSH_PUBKEY"] = args.ssh_pubkey
    if args.upload_url:
        env["JOB_UPLOAD_PUT_URL"] = args.upload_url
    if args.engine == "hydra":
        # The ion token travels env-to-env and is never printed; same .env
        # discipline as the API key.
        token = os.environ.get("CESIUM_ION_TOKEN", "")
        if not token:
            root = pathlib.Path(__file__).resolve()
            for parent in root.parents:
                dotenv = parent / ".env"
                if dotenv.is_file():
                    for line in dotenv.read_text().splitlines():
                        if line.startswith("CESIUM_ION_TOKEN="):
                            token = line.split("=", 1)[1].strip()
                    break
        if not token:
            raise SystemExit("CESIUM_ION_TOKEN introuvable (env ou .env) — "
                             "le moteur hydra ne cuit pas sans")
        env["TUILE_ION_TOKEN"] = token
        env["TUILE_CACHE_DIR"] = "/tmp/tuile-cache"

    pod = call(key, "POST", "/pods", {
        "name": args.name,
        "image": args.image,
        "registry": reg["id"],
        "gpu": {"id": args.gpu_type, "count": args.gpu_count,
                "allowedCudaVersions": cuda},
        "cloud": args.cloud,
        "disk": args.disk,
        "ports": ["22/tcp"] if args.ssh_pubkey else [],
        "env": env,
        "args": "bash /opt/render/render_job.sh",
    })
    print(f"pod: {pod['id']}  {args.gpu_count}x {args.gpu_type} ({args.cloud})"
          f"  {pod.get('cost')} $/h")


if __name__ == "__main__":
    main()
