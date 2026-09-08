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


# Les pods sont aux États-Unis, le registre Harbor est en Europe. Mesuré sur un
# hôte US-IL froid : vingt et une minutes à tirer stl/blender-globe depuis
# Harbor, les mêmes couches recyclant sans jamais finir — un pod payé à ne rien
# faire. Le même contenu dans ECR us-west-1 est à une région des pods. Harbor
# reste la source de vérité de la pile ; ECR est le miroir de livraison, et
# c'est lui que tire un job.
ECR_HOST = "057321054380.dkr.ecr.us-west-1.amazonaws.com"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--stage", default="", help=".usda à rendre (embarquée gzip+b64)")
    p.add_argument("--stage-url", default="",
                   help="URL GET (présignée) du .usda — la voie des vraies "
                        "tracks enregistrées")
    p.add_argument("--trajectory", default="",
                   help="la voie générative : le pod fabrique son manifeste "
                        "(ex: orbit:1440:2.17:42.52:8000:5000, zoom:64)")
    p.add_argument("--sse", type=float, default=3.0)
    p.add_argument("--viewport", default="1280x960")
    p.add_argument("--frames", required=True, help="A:B inclus")
    p.add_argument("--name", default="render-job")
    p.add_argument("--image",
                   default="harbor.sportstracklive.com/stl/blender-render:5.1")
    p.add_argument("--registry-name", default="ecr-usw1-stl",
                   help="credential registre déjà enregistré chez RunPod")
    p.add_argument("--gpu-type", default="NVIDIA GeForce RTX 5090")
    p.add_argument("--gpu-count", type=int, default=1)
    p.add_argument("--procs-per-gpu", type=int, default=4)
    p.add_argument("--cuda-min", default="13.2",
                   help="versions CUDA hôte acceptées, à partir de celle-ci")
    p.add_argument("--cuda-any", action="store_true",
                   help="aucune contrainte CUDA (Storm/GL s'en moque) — "
                        "élargit les hôtes disponibles")
    p.add_argument("--cloud", default="COMMUNITY", choices=["COMMUNITY", "SECURE"])
    p.add_argument("--disk", type=int, default=30)
    p.add_argument("--engine", default="native", choices=["native", "hydra"],
                   help="hydra = manifeste + procéduraux (image blender-globe; "
                        "CESIUM_ION_TOKEN local requis, passé au pod sans "
                        "jamais être affiché)")
    p.add_argument("--tier", default="cycles", choices=["cycles", "eevee"])
    p.add_argument("--upload-url", default="",
                   help="URL PUT présignée : le pod y dépose la vidéo finie")
    p.add_argument("--env", action="append", default=[],
                   metavar="K=V",
                   help="variable d'env supplémentaire pour le pod "
                        "(répétable) — le réglage machine du chemin USD "
                        "(TUILE_IMAGERY_BOOST, TUILE_FETCHES, ...) passe ici")
    p.add_argument("--wait", action="store_true",
                   help="reboucle tant que RunPod n'a pas d'instance libre "
                        "(45 s entre essais) — la loterie des 4x5090 se gagne "
                        "en restant dans la file")
    p.add_argument("--resident-gb", type=int, default=12,
                   help="budget mémoire tuiles PAR PROCESSUS (GiB). Le défaut "
                        "du code est 4 — dimensionné laptop ; un pod de ferme "
                        "monte à RAM/(GPU×procs)")
    p.add_argument("--width", type=int, default=1920)
    p.add_argument("--samples", type=int, default=128)
    p.add_argument("--threshold", type=float, default=0.05)
    p.add_argument("--batch-frames", type=int, default=60)
    p.add_argument("--extra", default="", help="args supplémentaires de render_usd.py")
    p.add_argument("--ssh-pubkey", default="",
                   help="clé publique à autoriser (rapatriement scp)")
    args = p.parse_args()
    if args.engine == "hydra" and args.image == p.get_default("image"):
        args.image = ECR_HOST + "/stl/blender-globe:5.1-su"

    key = api_key()
    regs = call(key, "GET", "/registries")
    reg = next((r for r in regs.get("registries", [])
                if r.get("name") == args.registry_name), None)
    if reg is None:
        raise SystemExit(f"credential registre '{args.registry_name}' absent — "
                         "à créer une fois (secret hors conversation)")

    if not args.stage and not args.stage_url and not args.trajectory:
        raise SystemExit("--stage, --stage-url ou --trajectory requis")
    stage_b64 = ""
    if args.stage:
        stage_b64 = base64.b64encode(
            gzip.compress(pathlib.Path(args.stage).read_bytes())).decode()
        if len(stage_b64) > 48_000:
            raise SystemExit(
                f"manifeste trop gros pour l'env d'un pod ({len(stage_b64)} o "
                "en base64) — passe-le par --stage-url (URL présignée)")

    known = ["12.4", "12.8", "13.2", "13.3"]
    cuda = [v for v in known if v >= args.cuda_min] or [args.cuda_min]

    env = {
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
    if stage_b64:
        env["JOB_STAGE_B64_GZ"] = stage_b64
    if args.stage_url:
        env["JOB_STAGE_URL"] = args.stage_url
    if args.trajectory:
        env["JOB_TRAJECTORY"] = args.trajectory
        env["JOB_SSE"] = str(args.sse)
        env["JOB_VIEWPORT"] = args.viewport
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
        env["TUILE_RESIDENT_BUDGET_GB"] = str(args.resident_gb)
    for pair in args.env:
        k, _, v = pair.partition("=")
        if not k or not v:
            raise SystemExit(f"--env attend K=V, reçu: {pair}")
        env[k] = v

    gpu = {"id": args.gpu_type, "count": args.gpu_count}
    if not args.cuda_any:
        gpu["allowedCudaVersions"] = cuda
    body = {
        "name": args.name,
        "image": args.image,
        "registry": reg["id"],
        "gpu": gpu,
        "cloud": args.cloud,
        "disk": args.disk,
        "ports": ["22/tcp"] if args.ssh_pubkey else [],
        "env": env,
        "args": "bash /opt/render/render_job.sh",
    }
    attempt = 0
    while True:
        attempt += 1
        try:
            pod = call(key, "POST", "/pods", body)
            break
        except SystemExit as e:
            # The capacity lottery answers 400 "no instances available"; every
            # other error is real and must not be retried into a bill.
            if args.wait and "no longer any instances" in str(e):
                print(f"essai {attempt}: pas d'instance libre, on reste dans "
                      "la file (45 s)", flush=True)
                import time
                time.sleep(45)
                continue
            raise
    print(f"pod: {pod['id']}  {args.gpu_count}x {args.gpu_type} ({args.cloud})"
          f"  {pod.get('cost')} $/h")


if __name__ == "__main__":
    main()
