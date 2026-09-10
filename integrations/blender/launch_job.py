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
import secrets
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone

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


# ---------------------------------------------------------------- archive --
# Un run qui ne laisse rien derrière lui ne se compare pas.
#
# Le 2026-09-08, six exécutions de la même frame sur un pod ont donné cinq fois
# 106 tuiles et une fois 7 ; le pod a été supprimé et les 500 Mo de traces qui
# contenaient la réponse sont partis avec lui. Depuis, chaque run dépose sous
# un préfixe qui lui est propre :
#
#   renders/<run-id>/config.json     tout ce qui décide de l'image
#   renders/<run-id>/render.mp4      le résultat
#   renders/<run-id>/trace.tar.gz    la trace de déterminisme (avec --trace)
#
# Les identifiants ne quittent jamais le poste : le pod ne reçoit que des URL
# présignées, comme le fait déjà `JOB_UPLOAD_PUT_URL` (voir render_job.sh).
R2_BUCKET = os.environ.get("TUILE_R2_BUCKET", "stl-track-data")
R2_ACCOUNT = os.environ.get("TUILE_R2_ACCOUNT", "a6d1085ef359ebcbdc45526c710912a4")
R2_PREFIX = "renders"


def git_state():
    """Le commit de tuile et si l'arbre était sale — sans quoi `config.json`
    ne dit pas quel code a produit l'image."""
    root = pathlib.Path(__file__).resolve().parents[2]
    def git(*a):
        try:
            return subprocess.run(["git", "-C", str(root), *a],
                                  capture_output=True, text=True,
                                  timeout=10).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return ""
    return {"commit": git("rev-parse", "HEAD"),
            "short": git("rev-parse", "--short", "HEAD"),
            "branch": git("rev-parse", "--abbrev-ref", "HEAD"),
            "dirty": bool(git("status", "--porcelain"))}


def run_id(git_info):
    """Trié par ordre alphabétique = trié par date, et le commit se lit dans
    le nom. Le suffixe aléatoire sépare deux runs de la même seconde."""
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    sha = git_info.get("short") or "nogit"
    dirty = "-dirty" if git_info.get("dirty") else ""
    return f"{stamp}-{sha}{dirty}-{secrets.token_hex(2)}"


def image_digest(image):
    """Le digest de l'image, pas seulement son tag — un tag bouge, et deux runs
    du même tag peuvent être deux binaires. Au mieux : l'absence de digest
    n'empêche pas de lancer, elle est dite."""
    if ".dkr.ecr." not in image:
        return None
    repo, _, tag = image.partition(":")
    repo = repo.split("/", 1)[1] if "/" in repo else repo
    region = image.split(".dkr.ecr.", 1)[1].split(".", 1)[0]
    try:
        out = subprocess.run(
            ["aws", "ecr", "describe-images",
             "--profile", os.environ.get("TUILE_AWS_PROFILE", "sportstracklive"),
             "--region", region, "--repository-name", repo,
             "--image-ids", f"imageTag={tag or 'latest'}"],
            capture_output=True, text=True, timeout=30)
        if out.returncode != 0:
            return None
        return json.loads(out.stdout)["imageDetails"][0]["imageDigest"]
    except (OSError, subprocess.SubprocessError, ValueError, KeyError, IndexError):
        return None


def default_pulumi_dir():
    """Le checkout d'infra voisin, s'il est là.

    Lire les identifiants ne doit demander aucun export : un lancement qui
    exige qu'on pense à une variable est un lancement qu'on finit par faire
    sans elle — et c'est ce qui a coûté les 60 secondes de rendu, parties avec
    leur pod sans rien laisser."""
    here = pathlib.Path(__file__).resolve()
    for parent in here.parents:
        candidate = parent.parent / "sportstracklive-rails" / "infra"
        if candidate.is_dir():
            return str(candidate)
    return ""


def r2_credentials():
    """L'environnement d'abord, Pulumi ensuite — la même source que
    `sportstracklive-rails/infra/ansible/run.sh`. Rien n'est écrit ni affiché."""
    key = os.environ.get("R2_ACCESS_KEY_ID", "")
    secret = os.environ.get("R2_SECRET_ACCESS_KEY", "")
    if key and secret:
        return key, secret
    pulumi_dir = os.environ.get("TUILE_PULUMI_DIR", "") or default_pulumi_dir()
    if pulumi_dir:
        # The same three things `sportstracklive-rails/infra/ansible/run.sh`
        # needs, and all three are easy to get wrong on their own: the state
        # lives in an S3 backend that must be named, that backend is reached
        # through a specific AWS profile, and the keys carry the `stl:`
        # namespace. Missing any one of them answers "not found" rather than
        # "not authorised", which reads like the key does not exist.
        env = dict(os.environ)
        env.setdefault(
            "PULUMI_BACKEND_URL",
            "s3://sportstracklive-pulumi-state"
            "?region=eu-west-3&awssdk=v2&profile=sportstracklive")
        env.setdefault("AWS_PROFILE", os.environ.get(
            "TUILE_AWS_PROFILE", "sportstracklive"))

        def cfg(name):
            try:
                out = subprocess.run(
                    ["pulumi", "config", "get", "--stack", "prod",
                     "-C", pulumi_dir, name],
                    capture_output=True, text=True, timeout=60, env=env)
                return out.stdout.strip() if out.returncode == 0 else ""
            except (OSError, subprocess.SubprocessError):
                return ""
        key = key or cfg("stl:r2_access_key_id")
        secret = secret or cfg("stl:r2_secret_access_key")
    return key, secret


#: Ce qu'un run dépose, sans condition.
#
# Aucun de ces quatre n'est optionnel, et le plus important n'est pas la
# vidéo. Un run qui échoue est celui dont on a le plus besoin des journaux, et
# c'est précisément celui qui n'en déposait aucun : le stdout des processus
# n'était écrit dans aucun fichier, et le téléversement était gardé derrière la
# réussite. Trois mesures sont mortes avec leur machine en deux jours.
ARCHIVE_OBJECTS = ("render.mp4", "logs.tar.gz", "trace.tar.gz", "profile.tar.gz")


def archive_urls(run, segments=0):
    """Les URL présignées où le pod déposera, et le client pour y écrire
    nous-mêmes le manifeste.

    Échoue AVANT de créer le pod quand les identifiants manquent : payer une
    heure de GPU pour découvrir que le résultat ne peut pas être déposé est
    exactement ce que cette fonction existe pour empêcher."""
    try:
        import boto3
        from botocore.config import Config as BotoConfig
    except ImportError:
        raise SystemExit(
            "boto3 absent — il porte la présignature R2, et sans elle un run "
            "ne laisse rien derrière lui. `pip install boto3`.")
    key, secret = r2_credentials()
    if not key or not secret:
        raise SystemExit(
            "identifiants R2 introuvables — ni dans l'environnement "
            "(R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY) ni dans Pulumi. "
            "TUILE_PULUMI_DIR pointe le checkout d'infra ; par défaut c'est "
            "../sportstracklive-rails/infra à côté de ce dépôt.")
    client = boto3.client(
        "s3",
        endpoint_url=f"https://{R2_ACCOUNT}.r2.cloudflarestorage.com",
        aws_access_key_id=key, aws_secret_access_key=secret,
        region_name="auto", config=BotoConfig(signature_version="s3v4"))
    def put(name):
        return client.generate_presigned_url(
            "put_object",
            Params={"Bucket": R2_BUCKET, "Key": f"{R2_PREFIX}/{run}/{name}"},
            ExpiresIn=7 * 24 * 3600)
    # Les quatre, toujours. Une URL présignée ne coûte rien tant que personne
    # n'écrit dessus, et conditionner la trace à un drapeau signifiait que la
    # seule fois où on la voulait, elle n'existait pas.
    urls = {name: put(name) for name in ARCHIVE_OBJECTS}
    # Plus une par segment, pour qu'un segment parte dès qu'il existe au lieu
    # d'attendre le montage. Attendre, c'est ce qui a fait perdre 1440 frames
    # déjà rendues à un pod repris en cours de route.
    for i in range(segments):
        urls[f"seg{i}"] = put(f"seg{i}.mp4")
    return client, urls


def presigned_get(key, hours=24):
    """Une URL GET à durée limitée pour un objet R2 déjà déposé.

    Le pod lit le pack et n'écrit rien : il reçoit exactement ce droit-là, et
    pas les identifiants qui le donnent."""
    import boto3
    from botocore.config import Config as BotoConfig
    access, secret = r2_credentials()
    if not access or not secret:
        raise SystemExit("identifiants R2 introuvables — impossible de "
                         f"présigner la lecture de {key}")
    client = boto3.client(
        "s3",
        endpoint_url=f"https://{R2_ACCOUNT}.r2.cloudflarestorage.com",
        aws_access_key_id=access, aws_secret_access_key=secret,
        region_name="auto", config=BotoConfig(signature_version="s3v4"))
    return client.generate_presigned_url(
        "get_object", Params={"Bucket": R2_BUCKET, "Key": key},
        ExpiresIn=hours * 3600)


SECRET_KEYS = ("TUILE_ION_TOKEN", "RUNPOD_API_KEY", "AWS_SECRET_ACCESS_KEY",
               "R2_SECRET_ACCESS_KEY")


def redacted(env):
    """L'archive est durable et partagée : un jeton qui y entre n'en sort plus.

    Les URL présignées sont expurgées aussi — elles portent une signature qui
    autorise l'écriture, donc ce sont des secrets à durée limitée."""
    out = {}
    for k, v in env.items():
        if k in SECRET_KEYS:
            out[k] = "<expurgé>"
        elif k.endswith("_PUT_URL") or k.endswith("_URL") and "X-Amz-Signature" in v:
            out[k] = v.split("?", 1)[0] + "?<signature expurgée>"
        else:
            out[k] = v
    return out


# Ce qu'un pod dit quand l'hôte, et non le job, est en cause.
#
# Sept pods ont été dépensés en un après-midi sur des hôtes où `cuInit` rend
# 999 : le conteneur reçoit un sous-ensemble des GPU de la machine sans procfs
# filtré, UVM refuse de s'ouvrir, et rien dans l'image ne peut y remédier. Ce
# n'est pas une impasse, c'est une loterie — un autre hôte marche. Reprendre à
# la main coûtait cinq minutes d'attention par essai.
BAD_HOST = ("GPU-HOST-BROKEN", "NO-GPU-BAIL")
#: Et ce qu'il dit quand il a démarré pour de bon.
STARTED = ("pack: ", "SOURCE ", "WALL:", "GPU-USE")


def pod_logs(key, pod_id, tail=400):
    """Le journal conteneur d'un pod, en clair.

    L'API le sert en Server-Sent Events ; on lit une tranche bornée et on rend
    les lignes. Pas de streaming ici : on veut décider, pas suivre."""
    req = urllib.request.Request(
        f"{API}/pods/{pod_id}/logs?source=container&tail={tail}",
        headers={"Authorization": f"Bearer {key}",
                 "Accept": "text/event-stream",
                 "User-Agent": "tuile-render-farm/0.1"})
    lines = []
    try:
        with urllib.request.urlopen(req, timeout=25) as r:
            for raw in r:
                raw = raw.decode(errors="replace")
                if not raw.startswith("data: "):
                    continue
                try:
                    lines.append(json.loads(raw[6:]).get("line", ""))
                except ValueError:
                    pass
    except (urllib.error.URLError, OSError, TimeoutError):
        pass
    return lines


def wait_until_it_renders(key, pod_id, patience=900):
    """Regarde un pod démarrer et dit ce qui s'est passé.

    Rend "started", "bad-host" ou "timeout". La patience par défaut couvre le
    tirage de l'image, qui a été mesuré à plus de dix minutes sur un hôte
    froid — un pod lent n'est pas un pod cassé."""
    import time
    deadline = time.time() + patience
    while time.time() < deadline:
        lines = pod_logs(key, pod_id)
        joined = "\n".join(lines)
        for line in lines:
            if any(m in line for m in BAD_HOST):
                return "bad-host", joined
        if any(m in joined for m in STARTED):
            return "started", joined
        time.sleep(15)
    return "timeout", ""


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
    p.add_argument("--gpu-type", action="append", default=[],
                   metavar="NAME",
                   help="répétable, par ordre de préférence. La capacité est "
                        "une loterie et une seule carte demandée, c'est une "
                        "file d'attente ; plusieurs, c'est un choix. "
                        "ATTENTION : les noyaux Cycles de l'image sont "
                        "compilés pour sm_120 seulement, donc toute carte "
                        "listée doit être Blackwell (5080, 5090, PRO 5000/6000, "
                        "B200). Une 4090 démarrerait et ne trouverait aucun "
                        "noyau à charger. Défaut : 5080 puis 5090 — la 5080 "
                        "est à 0,39 $/h contre 0,69, et 16 Go suffisent à ce "
                        "que ce job rend.")
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
    p.add_argument("--delegate", default="cycles", choices=["storm", "cycles"],
                   help="le délégué Hydra (--engine hydra). storm est un "
                        "rasteriseur OpenGL : il n'appelle jamais CUDA, donc "
                        "CUDA_VISIBLE_DEVICES ne pilote rien et quatre "
                        "processus ont été mesurés sur un seul GPU. cycles "
                        "passe par OptiX.")
    p.add_argument("--upload-url", default="",
                   help="URL PUT présignée : le pod y dépose la vidéo finie")
    p.add_argument("--env", action="append", default=[],
                   metavar="K=V",
                   help="variable d'env supplémentaire pour le pod "
                        "(répétable) — le réglage machine du chemin USD "
                        "(TUILE_IMAGERY_BOOST, TUILE_FETCHES, ...) passe ici")
    p.add_argument("--retries", type=int, default=0, metavar="N",
                   help="jusqu'à N hôtes de plus si celui qu'on obtient ne "
                        "sait pas démarrer CUDA. Le pod est supprimé avant "
                        "d'en reprendre un autre — un hôte où cuInit échoue "
                        "n'est pas une impasse, c'est une loterie, et "
                        "recommencer à la main coûtait cinq minutes "
                        "d'attention par essai.")
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
    p.add_argument("--trace", action="store_true",
                   help="active la trace de déterminisme sur le pod — 250-355 "
                        "Mo par frame en clair, donc un instrument de "
                        "diagnostic et non un réglage de production. "
                        "L'archive, elle, est déposée dans tous les cas.")
    p.add_argument("--pack", default="",
                   help="clé R2 d'un pack pré-cuit (ex: "
                        "packs/<scene>/1-48.tuilepack) — le pod le télécharge "
                        "une fois et rend sans jeton, sans réseau et sans "
                        "traversée. C'est la forme normale d'un rendu ; le "
                        "chemin streaming est ce qui tourne quand personne "
                        "n'a cuit.")
    p.add_argument("--scene", default="",
                   help="le digest de scène que le pack doit annoncer. Un "
                        "pack d'un autre tournage rend le mauvais sol et "
                        "déclare une réussite : c'est la seule panne que le "
                        "découpage en deux jobs ajoute.")
    p.add_argument("--profile", default="",
                   help="TUILE_PROFILE du pod (ex: cpu,heap) — les "
                        "flamegraphs remontent dans profile.tar.gz")
    args = p.parse_args()
    if not args.gpu_type:
        args.gpu_type = ["NVIDIA GeForce RTX 5080", "NVIDIA GeForce RTX 5090"]
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
        "JOB_DELEGATE": args.delegate,
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
    if args.trace:
        # Champs structurés + formateur JSON : voir tuile_core::determinism.
        env["TUILE_LOG"] = "tuile_det=info"
        env["TUILE_LOG_FORMAT"] = "json"
    if args.engine == "hydra" and args.pack:
        # A pack carries the whole scene. Nothing here needs a token, and the
        # pod is deliberately given none: a credential that never travels is
        # the only one that cannot leak from a machine somebody else rents out
        # after us.
        env["JOB_PACK_URL"] = presigned_get(args.pack)
        if args.scene:
            env["JOB_SCENE"] = args.scene
        env["TUILE_RESIDENT_BUDGET_GB"] = str(args.resident_gb)
    elif args.engine == "hydra":
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

    # --- l'archive, avant toute création de pod ---------------------------
    #
    # Sans condition et sans drapeau pour l'éteindre. `--no-archive` a existé,
    # et c'est lui qui a coûté un rendu de 60 secondes : le pod a été repris
    # quand le solde s'est épuisé, et 1440 frames sont parties avec lui. Un run
    # qui ne laisse rien n'a pas de raison d'exister.
    git_info = git_state()
    run = run_id(git_info)
    segments = args.gpu_count * args.procs_per_gpu
    manifest_client, urls = archive_urls(run, segments)
    if not args.upload_url:
        env["JOB_UPLOAD_PUT_URL"] = urls["render.mp4"]
    env["JOB_LOGS_PUT_URL"] = urls["logs.tar.gz"]
    env["JOB_TRACE_PUT_URL"] = urls["trace.tar.gz"]
    env["JOB_PROFILE_PUT_URL"] = urls["profile.tar.gz"]
    for i in range(segments):
        env[f"JOB_SEG_PUT_URL_{i}"] = urls[f"seg{i}"]
    if args.profile:
        env["TUILE_PROFILE"] = args.profile
        env["TUILE_PROFILE_DIR"] = "/out/profile"
    manifest = {
        "run_id": run,
        "launched_utc": datetime.now(timezone.utc).isoformat(),
        "argv": sys.argv[1:],
        "git": git_info,
        "image": args.image,
        "image_digest": image_digest(args.image),
        "pod_env": redacted(env),
        "params": {k: v for k, v in vars(args).items()
                   if k not in ("ssh_pubkey",)},
    }

    def gpu_spec(kind):
        gpu = {"id": kind, "count": args.gpu_count}
        if not args.cuda_any:
            gpu["allowedCudaVersions"] = cuda
        return gpu

    body = {
        "name": args.name,
        "image": args.image,
        "registry": reg["id"],
        "gpu": gpu_spec(args.gpu_type[0]),
        "cloud": args.cloud,
        "disk": args.disk,
        "ports": ["22/tcp"] if args.ssh_pubkey else [],
        "env": env,
        "args": "bash /opt/render/render_job.sh",
    }
    def take_a_pod():
        """Un pod, en essayant chaque carte demandée avant de patienter.

        L'ordre compte : la liste est par préférence, et on ne s'endort que
        lorsque AUCUNE d'elles n'est libre. Une seule carte demandée, c'est
        une file d'attente ; plusieurs, c'est un choix — et ce soir la 4×5090
        a fait attendre sept essais pendant que d'autres Blackwell étaient
        disponibles."""
        attempt = 0
        while True:
            attempt += 1
            unavailable = []
            for kind in args.gpu_type:
                body["gpu"] = gpu_spec(kind)
                try:
                    pod = call(key, "POST", "/pods", body)
                    body["gpu"] = gpu_spec(kind)  # celle qu'on a vraiment eue
                    return pod, kind
                except SystemExit as e:
                    # The capacity lottery answers 400 "no instances
                    # available"; every other error is real and must not be
                    # retried into a bill.
                    if "no longer any instances" in str(e):
                        unavailable.append(kind)
                        continue
                    raise
            if not args.wait:
                raise SystemExit(
                    "aucune des cartes demandées n'est libre : "
                    + ", ".join(unavailable))
            print(f"essai {attempt}: rien de libre en {', '.join(unavailable)}"
                  " — on reste dans la file (45 s)", flush=True)
            import time
            time.sleep(45)

    pod, kind = take_a_pod()
    print(f"pod: {pod['id']}  {args.gpu_count}x {kind} ({args.cloud})"
          f"  {pod.get('cost')} $/h", flush=True)

    # Un hôte qui ne sait pas démarrer CUDA n'est pas une impasse : c'est une
    # loterie, et le pod suivant marche. Ce qu'il ne faut surtout pas, c'est
    # le laisser tourner — il facture en dormant dans son `sleep` de bail.
    for left in range(args.retries, 0, -1):
        verdict, log = wait_until_it_renders(key, pod["id"])
        if verdict == "started":
            print("le pod rend.", flush=True)
            break
        for line in log.splitlines():
            if any(m in line for m in BAD_HOST) or line.startswith(("cuda:", "probe:")):
                print(f"  {line}", flush=True)
        call(key, "DELETE", f"/pods/{pod['id']}")
        print(f"hôte écarté ({verdict}), {left - 1} essai(s) restant(s)",
              flush=True)
        if left == 1:
            raise SystemExit("aucun hôte utilisable — relance plus tard, ou "
                             "essaie --cloud SECURE / --gpu-count 1")
        pod, kind = take_a_pod()
        print(f"pod: {pod['id']}  {args.gpu_count}x {kind} "
              f"({args.cloud})  {pod.get('cost')} $/h", flush=True)

    # Écrit APRÈS la création du pod, pour porter son identité : sans elle on
    # ne peut pas relier une archive aux journaux du pod.
    manifest["pod"] = {"id": pod["id"], "cost_per_hour": pod.get("cost"),
                       "cloud": args.cloud}
    manifest_client.put_object(
        Bucket=R2_BUCKET, Key=f"{R2_PREFIX}/{run}/config.json",
        Body=json.dumps(manifest, indent=2, sort_keys=True).encode(),
        ContentType="application/json")
    print(f"archive: s3://{R2_BUCKET}/{R2_PREFIX}/{run}/")


if __name__ == "__main__":
    main()
