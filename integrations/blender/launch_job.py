#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Launch a parametric render job on rented GPUs.

The farm submitter: one invocation = one render. Every render parameter and
every sizing knob is a flag; the job itself is render_job.sh baked into the
image, driven purely by environment.

Two backends, and the difference between them is where the work is placed:

**gcp** (default) — Cloud Run Jobs, N tasks, one whole L4 per task. The job is
a durable resource described by Pulumi in `sportstracklive-rails/infra/
render_farm.py`; this script only asks for *executions* of it. Chosen after
RunPod handed out four hosts in a row with a partial machine — a driver procfs
advertising GPUs the container had not been given, `cuInit` answering 999, and
no line of ours able to cause it or cure it. Cloud Run's documentation is
explicit where it matters: *"the GPU can only be attached to one container"*.

    ./launch_job.py --frames 1:48 --engine hydra --tasks 3 \
        --pack packs/019210eb75587aea/1-48.tuilepack --scene <digest>
    ./launch_job.py --assemble <run-id>       # le montage, après coup

**runpod** — one pod, N GPUs, M processes per GPU. Kept because it works, it
is tested, and it becomes useful again the day their capacity and their GPU
partitioning change.

    ./launch_job.py --backend runpod --stage gate-anim-60s.usda \
        --frames 1:1440 --gpu-type "NVIDIA GeForce RTX 5090" --gpu-count 4

Credentials: RUNPOD_API_KEY from the environment or a .env file next to the
repo root; `gcloud auth login` for the Google side. Never printed.
"""

import argparse
import base64
import gzip
import hashlib
import json
import os
import pathlib
import re
import secrets
import subprocess
import sys
import tempfile
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
            # A DELETE answers 204 with no body, and json.load on nothing
            # raises — which is how `--kill-all`, the one verb that has to work
            # when everything else is broken, crashed on its first real use
            # after successfully deleting the pod.
            body = r.read()
            return json.loads(body) if body.strip() else {}
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


def _gcloud_env():
    """L'environnement d'un appel gcloud, avec un python qui existe.

    Un terminal ouvert avant une mise à jour de Homebrew exporte encore un
    `CLOUDSDK_PYTHON` supprimé depuis, et gcloud meurt sur
    `exec: cannot execute` — une phrase qui ne parle pas d'authentification et
    qu'on met dix minutes à relier à la bonne cause."""
    env = dict(os.environ)
    cloudsdk = env.get("CLOUDSDK_PYTHON", "")
    if not cloudsdk or not os.access(cloudsdk, os.X_OK):
        for candidate in ("/opt/homebrew/bin/python3.13", sys.executable):
            if candidate and os.access(candidate, os.X_OK):
                env["CLOUDSDK_PYTHON"] = candidate
                break
    return env


def image_digest(image):
    """Le digest de l'image, pas seulement son tag — un tag bouge, et deux runs
    du même tag peuvent être deux binaires. Au mieux : l'absence de digest
    n'empêche pas de lancer, elle est dite."""
    if "-docker.pkg.dev/" in image:
        # Artifact Registry. Même raison qu'en dessous : le tag `5.1-su` a
        # déjà désigné trois binaires différents cette semaine.
        try:
            out = subprocess.run(
                ["gcloud", "artifacts", "docker", "images", "describe", image,
                 "--format=value(image_summary.digest)"],
                capture_output=True, text=True, timeout=60,
                env=_gcloud_env())
            digest = out.stdout.strip()
            return digest if out.returncode == 0 and digest else None
        except (OSError, subprocess.SubprocessError):
            return None
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


def archive_urls(run, segments=0, tasks=1):
    """Les URL présignées où le job déposera, et le client pour y écrire
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
    # Et une archive de journaux par tâche. Sans ça, N tâches écrivent sur la
    # même clé et la survivante est celle qui a fini en dernier — jamais celle
    # qui a échoué, qui est pourtant la seule qu'on voulait lire.
    for t in range(tasks if tasks > 1 else 0):
        for kind in ("logs", "trace", "profile"):
            urls[f"{kind}-t{t}"] = put(f"{kind}-t{t}.tar.gz")
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
# Un pod qui reçoit une PART d'une machine voit tous ses GPU dans procfs et
# n'en obtient qu'une partie dans /dev ; UVM refuse de s'ouvrir et cuInit rend
# 999. Mesuré sur quatre hôtes : 8/1, 5/4, 5/1, 5/1. Ce n'est donc pas une
# loterie d'hôtes cassés — c'est la forme d'un partage — et réessayer ailleurs
# ne converge pas. Le retry reste utile pour les pannes qui, elles, sont
# passagères, mais la sortie est de demander la machine entière.
BAD_HOST = ("GPU-PARTIAL-HOST", "NO-GPU-BAIL")
#: Et ce qu'il dit quand il a démarré pour de bon.
STARTED = ("pack: ", "SOURCE ", "WALL:", "GPU-USE")


def available_cards(key, count=1):
    """Les cartes réellement libres, du moins cher au plus cher.

    Demander vaut mieux que deviner : un balayage aveugle POSTe douze cartes
    sur deux clouds pour apprendre ce qu'une requête dit d'un coup. Et la
    disponibilité est volatile — « LOW » veut dire qu'elle peut disparaître
    entre la question et la réponse — donc on prend tout de suite ce qu'on
    apprend, sans repasser par la case attente."""
    out = []
    for cloud in ("COMMUNITY", "SECURE"):
        data = call(key, "GET",
                    f"/catalog/gpus?include=AVAILABILITY&product=POD"
                    f"&count={count}&cloud={cloud}")
        for g in data.get("gpus", []):
            if g.get("availability") in (None, "NONE"):
                continue
            price = (g.get("price") or {}).get(cloud.lower()) or 99
            out.append((price, cloud, g.get("id"), g.get("memory") or 0))
    return sorted(out)


def all_pods(key):
    """Tous les pods, ou une exception — jamais une liste vide par erreur.

    L'API rend `{"pods": [...]}` ici et `{"items": [...]}` ailleurs, et un
    parseur qui cherchait `items` et se rabattait sur `[]` a répondu « aucun
    pod » pendant que six tournaient. Six fois 0,69 $/h, invisibles, parce
    qu'une lecture ratée avait exactement la même tête qu'un compte à zéro.

    Alors on ne se rabat sur rien : une réponse d'une forme inconnue est une
    erreur, pas un vide."""
    data = call(key, "GET", "/pods")
    for field in ("pods", "items", "data"):
        if isinstance(data.get(field), list):
            return data[field]
    if isinstance(data, list):
        return data
    raise SystemExit(
        f"réponse /pods de forme inattendue (clés: {sorted(data)}) — "
        "refus de conclure qu'il n'y a aucun pod")


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


# -------------------------------------------------------------------- gcp --
#
# Cloud Run Jobs. Le job est une ressource durable, décrite par Pulumi
# (`sportstracklive-rails/infra/render_farm.py`) ; ce qui suit n'en demande que
# des *exécutions*. C'est le modèle natif du service, et il place la frontière
# au bon endroit : Pulumi possède la forme d'une machine à rendre, le lanceur
# en demande une instance avec l'environnement d'un run particulier.
#
# Par REST plutôt que par `gcloud run jobs execute` : le drapeau
# `--update-env-vars` sépare ses variables par des virgules. Nos URL présignées
# n'en contiennent pas aujourd'hui — R2 signe en hexadécimal — mais faire
# dépendre un lancement de cette propriété-là est le genre de pari qui se perd
# un mardi. Ce fichier parle déjà REST.

GCP_PROJECT = os.environ.get("TUILE_GCP_PROJECT", "first-parser-498510-a4")
GCP_REGION = os.environ.get("TUILE_GCP_REGION", "europe-west1")
GCP_JOB = os.environ.get("TUILE_GCP_JOB", "tuile-render")
GCP_BAKE_JOB = os.environ.get("TUILE_GCP_BAKE_JOB", "tuile-bake")
RUN_API = "https://run.googleapis.com/v2"


def gcp_token():
    """Le jeton d'accès de l'utilisateur, par gcloud.

    Plutôt que `google-auth` : rien à installer, et le compte est déjà celui
    qui s'est authentifié dans un navigateur. Le jeton vit une heure, n'est
    stocké nulle part et ne traverse jamais l'environnement d'une tâche —
    Cloud Run tire l'image avec l'identité du job, pas avec celle-ci.

    `CLOUDSDK_PYTHON` est forcé quand celui de l'environnement ne s'exécute
    pas : un terminal ouvert avant une mise à jour de Homebrew exporte encore
    un python qui n'existe plus, et gcloud meurt sur `exec: cannot execute`
    dans une phrase qui ne parle pas d'authentification.
    """
    try:
        out = subprocess.run(["gcloud", "auth", "print-access-token"],
                             capture_output=True, text=True, timeout=60,
                             env=_gcloud_env())
    except (OSError, subprocess.SubprocessError) as e:
        raise SystemExit(f"gcloud injoignable: {e}")
    if out.returncode != 0:
        raise SystemExit(
            "gcloud ne rend pas de jeton — `gcloud auth login` puis "
            f"`gcloud config set project {GCP_PROJECT}`.\n"
            + out.stderr.strip())
    return out.stdout.strip()


def gcp_call(method, path, body=None, token=None, _retried=False):
    """Un appel à l'API Cloud Run. Même discipline que `call` côté RunPod :
    une erreur est une phrase lisible, pas une trace.

    Un 401 est réessayé **une fois**, avec un jeton neuf. `gcp_watch` en
    renouvelle un toutes les 45 minutes, ce qui suppose que celui de départ en
    avait 60 — faux dès qu'il vient d'un cache déjà entamé. Mesuré le
    19 septembre 2026 : le lanceur est mort en dix minutes sur
    `401: Request had invalid authentication credentials`. Sur une cuisson
    c'est bénin, le job dépose son pack tout seul ; sur un rendu le lanceur est
    ce qui **monte les segments**, et sa mort laisse douze morceaux sur R2 et
    aucun film.

    Une fois, pas en boucle : un jeton qu'on vient de renouveler et qui est
    refusé à nouveau, c'est un problème de droits, et le réessayer ne ferait que
    le rendre illisible.
    """
    minted = token or gcp_token()
    url = path if path.startswith("http") else f"{RUN_API}/{path.lstrip('/')}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        url, data=data, method=method,
        headers={"Authorization": f"Bearer {minted}",
                 "Content-Type": "application/json",
                 "User-Agent": "tuile-launch-job/1.0"})
    try:
        with urllib.request.urlopen(req) as r:
            raw = r.read()
    except urllib.error.HTTPError as e:
        if e.code == 401 and not _retried:
            return gcp_call(method, path, body, token=gcp_token(), _retried=True)
        detail = e.read().decode(errors="replace")
        try:
            detail = json.loads(detail)["error"]["message"]
        except (ValueError, KeyError, TypeError):
            pass
        raise SystemExit(f"{method} {url} → {e.code}: {detail}")
    # Un 200 sans corps est une réussite, pas un JSON tronqué : `:cancel`
    # répond parfois vide, et json.loads("") a déjà fait passer un
    # `--kill-all` réussi pour une panne.
    return json.loads(raw) if raw.strip() else {}


def gcp_job_path(job=None):
    job = job or GCP_JOB
    return f"projects/{GCP_PROJECT}/locations/{GCP_REGION}/jobs/{job}"


def gcp_image_of_job(token=None):
    """L'image que le job lancera vraiment.

    Demandée au job plutôt que reconstruite ici. Le manifeste d'un run doit
    dire ce qui a tourné, et une chaîne fabriquée côté lanceur dit seulement ce
    que le lanceur croyait. Les deux ont divergé le jour où le tag a changé
    dans Pulumi et pas dans le script."""
    job = gcp_call("GET", gcp_job_path(), token=token)
    containers = (((job.get("template") or {}).get("template") or {})
                  .get("containers") or [])
    if not containers or not containers[0].get("image"):
        raise SystemExit(
            f"le job {GCP_JOB} ne déclare pas d'image — "
            "`pulumi up` dans sportstracklive-rails/infra a-t-il abouti ?")
    return containers[0]["image"]


def gcp_executions(token=None, job=None):
    """Les exécutions du job, de la plus récente à la plus ancienne.

    Lève sur une forme inattendue plutôt que de rendre une liste vide. Un
    `--list` qui a répondu « aucun pod » pendant que six tournaient a coûté
    dix-huit dollars et une nuit : une réponse qu'on ne sait pas lire n'est
    pas une absence."""
    data = gcp_call("GET", f"{gcp_job_path(job)}/executions?pageSize=50", token=token)
    if not isinstance(data, dict):
        raise SystemExit(f"réponse inattendue de executions.list: {data!r:.200}")
    if "executions" not in data:
        # Une liste vide est légitime ; une clé absente peut l'être aussi
        # (l'API omet le champ quand il n'y a rien). On distingue les deux par
        # la présence d'autre chose que la pagination.
        unexpected = set(data) - {"nextPageToken"}
        if unexpected:
            raise SystemExit(
                "executions.list a répondu une forme inconnue "
                f"({sorted(data)}) — refus de conclure au vide")
        return []
    rows = data["executions"]
    if not isinstance(rows, list):
        raise SystemExit(f"executions n'est pas une liste: {type(rows)}")
    return rows


def gcp_execution_line(ex):
    """Une exécution en une ligne lisible."""
    short = ex.get("name", "?").rsplit("/", 1)[-1]
    done = ex.get("completionTime")
    state = "terminée" if done else ("en cours" if ex.get("runningCount")
                                     else "en attente")
    return (f"{short}  {state}  "
            f"{ex.get('succeededCount', 0)}✓ "
            f"{ex.get('failedCount', 0)}✗ "
            f"{ex.get('runningCount', 0)}⟳ "
            f"de {ex.get('taskCount', '?')}  "
            f"{ex.get('createTime', '')[:19]}")


def gcp_run(env, tasks, dry_run=False, token=None, job=None):
    """Demande une exécution du job, avec l'environnement de ce run.

    `env` est fusionné avec celui de l'image, pas substitué — le job n'en
    définit aucun, donc c'est équivalent, mais c'est la sémantique de l'API et
    il vaut mieux la connaître que la découvrir.

    Toutes les tâches reçoivent le MÊME environnement : l'API n'a pas de
    surcharge par tâche. C'est pour ça que `render_job.sh` calcule sa tranche
    à partir de `CLOUD_RUN_TASK_INDEX` au lieu de la recevoir."""
    body = {
        "overrides": {
            "containerOverrides": [
                {"env": [{"name": k, "value": str(v)} for k, v in env.items()]}
            ],
            "taskCount": tasks,
        }
    }
    if dry_run:
        body["validateOnly"] = True
    op = gcp_call("POST", f"{gcp_job_path(job)}:run", body, token=token)
    if dry_run:
        return None
    # La réponse est une Operation ; l'exécution en cours de création est dans
    # ses métadonnées. On ne l'attend pas : elle existe déjà, et ce qu'on veut
    # est son nom pour la suivre.
    name = (op.get("metadata") or {}).get("name") or op.get("name", "")
    if not name:
        raise SystemExit(f"executions:run n'a pas nommé d'exécution: {op!r:.300}")
    return name


def gcp_watch(execution, token=None, every=20):
    """Suit une exécution jusqu'à ce qu'elle finisse, en disant ce qui bouge.

    Ne rend la main que sur un état terminal : une tâche qui tourne encore est
    une tâche qui facture, et sortir d'ici sans le dire laisserait croire que
    c'est fini."""
    import time
    last = ""
    minted = time.time()
    while True:
        # Le jeton vit une heure, un rendu peut vivre plus longtemps.
        #
        # Il était pris une fois au lancement et réutilisé pour tout le suivi.
        # Mesuré le 16 septembre 2026 : au bout d'une heure, « 401: Request had
        # invalid authentication credentials » a tué le lanceur en plein
        # rendu — et avec lui le montage qui devait suivre, alors que les
        # trois tâches travaillaient encore.
        if time.time() - minted > 45 * 60:
            token = gcp_token()
            minted = time.time()
        ex = gcp_call("GET", execution, token=token)
        line = gcp_execution_line(ex)
        if line != last:
            print(f"  {line}", flush=True)
            last = line
        if ex.get("completionTime"):
            ok = ex.get("succeededCount", 0)
            bad = ex.get("failedCount", 0) + ex.get("cancelledCount", 0)
            if ex.get("logUri"):
                print(f"  journaux: {ex['logUri']}", flush=True)
            return ok, bad
        time.sleep(every)


def pack_key(args):
    """Où le pack sera déposé, calculé AVANT de lancer quoi que ce soit.

    Le nom vient des **paramètres de cuisson** — la trajectoire, la plage, le
    viewport, la tolérance — et de rien d'autre. Ils sont connus du lanceur et
    du job, à l'identique, avant que le premier octet soit tiré.

    # Pourquoi pas le digest de scène

    Le digest que `tuile-bake` calcule décrit les poses **et les réglages
    résolus**, donc il n'existe qu'une fois le job démarré. Nommer l'objet avec
    lui obligeait à déposer ailleurs, lire `BAKE-KEY` dans les journaux, puis
    copier — trois étapes dont les deux dernières vivaient dans le processus
    du lanceur. Mesuré le 16 septembre : le lanceur s'est arrêté entre la
    cuisson et la copie, et un pack de deux gigaoctets parfaitement valide est
    resté sous une clef que personne ne cherche. Un rangement qui dépend qu'une
    fenêtre de terminal reste ouverte n'est pas un rangement.

    Le digest ne disparaît pas pour autant : il reste **dans** le pack, et
    c'est lui que `--scene` vérifie à l'ouverture. Le nom dit où ranger, le
    digest dit ce que c'est — deux questions, deux réponses.
    """
    first, _, last = args.frames.partition(":")
    canonical = "\n".join([
        f"trajectory={args.trajectory}",
        f"frames={args.frames}",
        f"viewport={args.viewport}",
        f"sse={args.sse}",
        # Le boost d'imagerie change le CONTENU cuit — il décide de combien de
        # niveaux l'imagerie peut descendre sous le terrain — donc il doit
        # changer le nom. Une clé qui ignore un paramètre de cuisson finit par
        # servir un pack pour un autre.
        f"imagery_boost={args.imagery_boost}",
        # Les sources. Deux packs de la même trajectoire drapés d'imageries
        # différentes ne sont pas le même pack, et doivent porter deux noms.
        f"imagery={args.imagery}",
        f"terrain={args.terrain}",
    ])
    name = hashlib.sha256(canonical.encode()).hexdigest()[:16]
    return f"packs/{name}/{first}-{last}.tuilepack"


def pack_exists(key):
    """Un pack déjà cuit sous cette clef ?"""
    import boto3
    from botocore.config import Config as BotoConfig
    access, secret = r2_credentials()
    if not access or not secret:
        return False
    client = boto3.client(
        "s3", endpoint_url=f"https://{R2_ACCOUNT}.r2.cloudflarestorage.com",
        aws_access_key_id=access, aws_secret_access_key=secret,
        region_name="auto", config=BotoConfig(signature_version="s3v4"))
    try:
        client.head_object(Bucket=R2_BUCKET, Key=key)
        return True
    except Exception:
        return False


def scene_digest_of(key):
    """Le digest de scène déposé par la cuisson, à côté du pack.

    Il ne se déduit pas des paramètres : il couvre aussi les réglages de
    traversée **résolus**, que seul le job qui cuit connaît. Sans lui, un rendu
    ne peut pas vérifier qu'on lui donne le bon globe — et c'est la seule
    barrière entre un pack et une autre scène."""
    import boto3
    from botocore.config import Config as BotoConfig
    access, secret = r2_credentials()
    if not access or not secret:
        return ""
    client = boto3.client(
        "s3", endpoint_url=f"https://{R2_ACCOUNT}.r2.cloudflarestorage.com",
        aws_access_key_id=access, aws_secret_access_key=secret,
        region_name="auto", config=BotoConfig(signature_version="s3v4"))
    try:
        body = client.get_object(Bucket=R2_BUCKET, Key=key + ".scene")["Body"]
        return body.read().decode().strip()
    except Exception:
        return ""


def bake_env(args, ion, pack_url, scene_url, logs_url):
    """Tout ce que le job de cuisson lit dans son environnement.

    Une fonction de ses arguments, parce que l'alternative est que le seul
    moyen de voir ce qu'on dit à une cuisson soit d'en lancer une.
    `--resident-gb` était accepté, recopié dans le manifeste, et abandonné
    en silence sur ce chemin — précisément pour cette raison : rien ne
    pouvait regarder.
    """
    return {
        "JOB_FRAMES": args.frames,
        "JOB_TRAJECTORY": args.trajectory,
        "JOB_VIEWPORT": args.viewport,
        "JOB_SSE": str(args.sse),
        "TUILE_IMAGERY_BOOST": str(args.imagery_boost),
        "JOB_IMAGERY_ASSET": str(args.imagery) if args.imagery else "",
        "JOB_TERRAIN_ASSET": str(args.terrain) if args.terrain else "",
        "JOB_PACK_PUT_URL": pack_url,
        # Le digest de scène, déposé à côté du pack par le job.
        #
        # Il ne se déduit pas des paramètres : il porte aussi les réglages de
        # traversée résolus, que seul le job connaît. Sans ce fichier, un rendu
        # ne pourrait pas passer `--scene`, et on perdrait la seule barrière
        # entre un pack et le mauvais globe.
        "JOB_SCENE_PUT_URL": scene_url,
        "JOB_LOGS_PUT_URL": logs_url,
        "TUILE_ION_TOKEN": ion,
        # Le budget de tuiles résidentes, que la cuisson ne recevait pas.
        #
        # `--resident-gb` n'était posé que dans les deux branches `--engine
        # hydra`, c'est-à-dire pour le rendu : une cuisson tournait donc au
        # défaut du code, 4 GiB, dans un conteneur qui en a 32. Mesuré le
        # 19 septembre 2026 — `resident_gib=3.76` collé au plafond pendant que
        # `loads_started` montait à 134 720 pour 234 tuiles sélectionnées. Le
        # cache évinçait ce dont la frame avait besoin et la traversée le
        # redemandait, quarante-cinq fois par tuile, sans jamais converger.
        "TUILE_RESIDENT_BUDGET_GB": str(args.resident_gb),
        # La cadence, sur les deux jobs et pas seulement sur le rendu.
        #
        # La cuisson en a besoin pour fabriquer sa bande : une cuisson et un
        # rendu qui n'échantillonnent pas la même trajectoire décrivent deux
        # tournages, et le pack répond alors à des caméras que le rendu ne
        # demandera jamais. Elle la dérivait de la chaîne, le rendu la
        # recevait ici : deux chemins pour un même nombre.
        "JOB_FPS": str(fps_of(args.trajectory)),
    }

def bake(args, token=None):
    """Lance le job A : cuire une trajectoire en un pack, déposé sur R2.

    La moitié qui produit l'archive. Elle porte le jeton, parle au réseau, et
    n'a aucun usage d'un GPU — c'est pour ça qu'elle est un job à part, sans
    carte, et non une phase du rendu.

    Le pack est déposé par le job lui-même, sur une URL présignée calculée
    d'avance : aucun identifiant R2 n'atteint jamais la machine, et rien ne
    reste à faire une fois la tâche finie. Le lanceur peut mourir à tout
    moment après le lancement sans que le résultat en souffre.
    """
    token = token or gcp_token()

    # Le jeton ion, depuis l'environnement ou le .env du dépôt. Il voyage
    # d'environnement à environnement et n'est jamais imprimé — même
    # discipline que partout ailleurs ici.
    ion = os.environ.get("CESIUM_ION_TOKEN", "")
    if not ion:
        root = pathlib.Path(__file__).resolve()
        for parent in root.parents:
            dotenv = parent / ".env"
            if dotenv.is_file():
                for line in dotenv.read_text().splitlines():
                    if line.startswith("CESIUM_ION_TOKEN="):
                        ion = line.split("=", 1)[1].strip()
                break
    if not ion:
        raise SystemExit("CESIUM_ION_TOKEN introuvable (env ou .env) — "
                         "une cuisson ne cuit pas sans")

    git_info = git_state()
    run = run_id(git_info)
    client, urls = archive_urls(run, 0, 1)
    key = pack_key(args)

    def put(name):
        return client.generate_presigned_url(
            "put_object", Params={"Bucket": R2_BUCKET, "Key": name},
            ExpiresIn=7 * 24 * 3600)

    env = bake_env(args, ion, put(key), put(key + ".scene"),
                   urls["logs.tar.gz"])
    for pair in args.env:
        k, _, v = pair.partition("=")
        if not k or not v:
            raise SystemExit(f"--env attend K=V, reçu: {pair}")
        env[k] = v

    manifest = {
        "run_id": run,
        "launched_utc": datetime.now(timezone.utc).isoformat(),
        "argv": sys.argv[1:],
        "git": git_info,
        "kind": "bake",
        "pack_key": key,
        "job_env": redacted(env),
    }
    client.put_object(
        Bucket=R2_BUCKET, Key=f"{R2_PREFIX}/{run}/config.json",
        Body=json.dumps(manifest, indent=2, sort_keys=True).encode(),
        ContentType="application/json")
    print(f"archive: s3://{R2_BUCKET}/{R2_PREFIX}/{run}/", flush=True)
    print(f"pack:    s3://{R2_BUCKET}/{key}", flush=True)

    name = gcp_run(env, 1, dry_run=args.dry_run, token=token, job=GCP_BAKE_JOB)
    if name is None:
        print("validateOnly: la demande est acceptée, rien n'a tourné.")
        return
    print(f"exécution: {name.rsplit('/', 1)[-1]}  cuisson {args.frames}",
          flush=True)
    print(f"rendre avec: --pack {key}", flush=True)
    ok, bad = gcp_watch(name, token=token)
    print(f"{ok} réussie(s), {bad} en échec", flush=True)
    if bad:
        raise SystemExit(
            f"la cuisson a échoué — journaux dans "
            f"s3://{R2_BUCKET}/{R2_PREFIX}/{run}/logs.tar.gz")

def fps_of(trajectory, default=24):
    """La cadence que cette trajectoire décrit.

    Elle est déjà dans la chaîne — `pyrenees:minutes:fps:alt:offset` — et c'est
    la seule raison pour laquelle il n'y a pas de drapeau `--fps` à côté. Deux
    endroits pour le même nombre, c'est deux endroits qui finiront par ne plus
    dire la même chose, et le désaccord serait muet : les segments seraient
    encodés à une cadence et les poses calculées à une autre, ce qui donne un
    film de la bonne longueur en frames et de la mauvaise en secondes.

    Les autres genres ne portent pas de cadence — `orbit` compte des frames,
    `zoom` aussi — donc ils prennent le défaut.
    """
    parts = (trajectory or "").split(":")
    if parts[0] != "pyrenees" or len(parts) < 3 or not parts[2]:
        return default
    try:
        fps = float(parts[2])
    except ValueError:
        return default
    if fps <= 0:
        return default
    # Entier quand c'est un entier : le pod le passe à `render_usd.py --fps`,
    # qui est `type=int`, et `int("60.0")` lève. Une cadence fractionnaire —
    # 23.976 — reste un flottant et échouera là-bas, bruyamment, ce qui est la
    # bonne façon de découvrir que ce chemin ne la porte pas encore.
    return int(fps) if fps == int(fps) else fps


def frames_of(trajectory):
    """Combien de frames cette trajectoire décrit, quand elle le dit.

    `None` pour les genres qui ne l'énoncent pas — on ne devine pas. Sert à
    confronter `--frames`, qui est un argument séparé : rien n'obligeait les
    deux à s'accorder, et le désaccord dans le sens court est muet. Une bande
    de 7200 frames cuite `1:2880`, ce sont quarante-huit secondes de film
    livrées pour deux minutes demandées, sans qu'aucun compteur ne s'en plaigne.
    """
    parts = (trajectory or "").split(":")
    try:
        if parts[0] == "pyrenees":
            minutes = float(parts[1]) if len(parts) > 1 and parts[1] else 2.0
            return round(minutes * 60.0 * fps_of(trajectory))
        if parts[0] == "orbit":
            return int(parts[1]) if len(parts) > 1 and parts[1] else 1440
    except (ValueError, IndexError):
        return None
    return None


def check_frames(trajectory, frames, say=print):
    """Confronte `--frames` à ce que la trajectoire décrit.

    Trop loin est une erreur : la bande n'a pas ces poses. Trop court est
    légitime — on cuit une tranche exprès — mais doit se dire, parce que c'est
    la seule différence entre une tranche voulue et un film amputé.
    """
    expected = frames_of(trajectory)
    if expected is None:
        return
    last = frames.partition(":")[2] or frames
    try:
        last = int(last)
    except ValueError:
        return
    if last > expected:
        raise SystemExit(
            f"--frames va jusqu'à {last} et la trajectoire n'en décrit que "
            f"{expected} : la bande n'a pas ces poses")
    if last < expected:
        fps = fps_of(trajectory)
        say(f"cuisson partielle: {last} frames sur {expected} "
            f"({last / fps:.1f} s de film sur {expected / fps:.1f} s)")


def check_cadence(frames, duration, fps, tolerance_frames=2.0):
    """Le film dure-t-il ce que ses images et sa cadence annoncent ?

    Le montage concatène en `-c copy`, donc il n'impose aucune cadence : il
    hérite de celle des segments. Un paramètre `fps` qui ne servirait qu'à être
    passé serait pire qu'absent — il donnerait l'impression que le montage
    gère la cadence, ce qui est exactement la fausse confiance qui a laissé
    passer un film de cinq minutes pour deux minutes de vol.

    Alors il vérifie. Les deux nombres sont là, sous la main, à l'endroit exact
    où le mensonge apparaît : 7200 images étiquetées 24 i/s font 300 secondes,
    et rien d'autre dans la chaîne ne le remarque.

    Forcer la cadence ici serait la mauvaise correction : réencoder ou
    ré-étiqueter masquerait le défaut amont au lieu de le signaler.

    La tolérance est en IMAGES, pas en pourcentage : un conteneur arrondit ses
    horodatages, et on a mesuré 25 ms de bourrage sur un remux — une image et
    demie. Un pourcentage serait trop lâche sur un plan long et trop strict sur
    un plan court.
    """
    try:
        frames, duration, fps = int(frames), float(duration), float(fps)
    except (TypeError, ValueError):
        return None
    if frames <= 0 or fps <= 0:
        return None
    expected = frames / fps
    drift = abs(duration - expected) * fps
    if drift > tolerance_frames:
        raise SystemExit(
            f"le film dure {duration:.3f} s pour {frames} images à {fps:g} i/s, "
            f"soit {expected:.3f} s attendues — {drift:.1f} images d'écart. "
            f"Les images sont bonnes ; c'est la cadence du conteneur qui ment, "
            f"et elle vient des segments (JOB_FPS n'a pas atteint le rendu).")
    return expected


def assemble(run, fps=24):
    """Monte le film depuis les segments déposés sur R2.

    Hors du job, et c'est délibéré : avec plusieurs tâches, aucune ne voit tous
    les segments. Les concaténer dans la tâche produirait N films d'un N-ième
    chacun, déposés les uns sur les autres. Ici on les relit, dans l'ordre de
    leur numéro global — celui que `render_job.sh` leur donne — et le montage
    peut se faire longtemps après que la dernière tâche a rendu la main. C'est
    le point : personne n'a besoin de regarder à la fin."""
    import boto3
    from botocore.config import Config as BotoConfig
    key, secret = r2_credentials()
    if not key or not secret:
        raise SystemExit("identifiants R2 introuvables — rien à monter")
    client = boto3.client(
        "s3", endpoint_url=f"https://{R2_ACCOUNT}.r2.cloudflarestorage.com",
        aws_access_key_id=key, aws_secret_access_key=secret,
        region_name="auto", config=BotoConfig(signature_version="s3v4"))
    prefix = f"{R2_PREFIX}/{run}/"
    found = {}
    paginator = client.get_paginator("list_objects_v2")
    for page in paginator.paginate(Bucket=R2_BUCKET, Prefix=prefix):
        for obj in page.get("Contents", []):
            name = obj["Key"][len(prefix):]
            m = re.fullmatch(r"seg(\d+)\.mp4", name)
            if m and obj["Size"] > 1000:
                found[int(m.group(1))] = obj["Key"]
    if not found:
        raise SystemExit(f"aucun segment dans s3://{R2_BUCKET}/{prefix}")
    # Un trou dans la numérotation est un rendu incomplet. On le dit et on
    # monte quand même ce qu'on a : un film court dont on connaît le défaut
    # vaut mieux qu'un refus, et infiniment mieux qu'un film court qu'on croit
    # entier.
    order = sorted(found)
    gaps = [i for i in range(order[0], order[-1] + 1) if i not in found]
    if gaps:
        print(f"SEGMENTS-MANQUANTS: {gaps} — le film sera incomplet")
    tmp = pathlib.Path(tempfile.mkdtemp(prefix=f"tuile-assemble-{run}-"))
    listing = tmp / "list.txt"
    with listing.open("w") as fh:
        for i in order:
            local = tmp / f"seg{i}.mp4"
            client.download_file(R2_BUCKET, found[i], str(local))
            fh.write(f"file '{local}'\n")
    print(f"{len(order)} segments récupérés dans {tmp}")
    out = tmp / "render.mp4"
    done = subprocess.run(
        ["ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(listing),
         "-c", "copy", str(out)], capture_output=True, text=True)
    if done.returncode != 0 or not out.exists():
        raise SystemExit("ffmpeg concat a échoué:\n" + done.stderr[-2000:])
    frames = subprocess.run(
        ["ffprobe", "-v", "error", "-count_frames", "-select_streams", "v:0",
         "-show_entries", "stream=nb_read_frames", "-of", "csv=p=0", str(out)],
        capture_output=True, text=True).stdout.strip()
    size_mb = out.stat().st_size / 1e6
    print(f"film: {frames or '?'} frames, {size_mb:.1f} Mo")
    duration = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration",
         "-of", "csv=p=0", str(out)], capture_output=True, text=True).stdout.strip()
    check_cadence(frames, duration, fps)

    client.upload_file(str(out), R2_BUCKET, f"{prefix}render.mp4",
                       ExtraArgs={"ContentType": "video/mp4"})
    print(f"déposé: s3://{R2_BUCKET}/{prefix}render.mp4")

    # Et on la regarde.
    #
    # Voir CLAUDE.md : un rendu est une image, et tous les instruments qui
    # remplacent le fait de la regarder ont déjà menti ici — `RENDER-DONE` sur
    # quatre cinquièmes de film manquant, `frames: 2/2` sur un globe
    # entièrement rose, un journal vert sur une texture jamais branchée.
    try:
        subprocess.run(["open", str(out)], check=False, timeout=30)
    except (OSError, subprocess.SubprocessError):
        pass
    return out


def segment_count(backend, tasks, gpu_count, procs_per_gpu):
    """Combien de segments un run produira, tous processus et toutes tâches
    confondus.

    Les deux fournisseurs comptent différemment, et c'est la seule chose qui
    les distingue ici :

      runpod   un pod, N GPU, M processus par GPU      → N × M
      gcp      T tâches, UN GPU chacune, M processus   → T × M

    Le nombre importe parce qu'il fixe le nombre d'URL présignées. Une de trop
    ne coûte rien ; une de moins et le dernier processus rend un segment qu'il
    ne peut déposer nulle part, ce qui ne se voit qu'au montage."""
    per_task = procs_per_gpu if backend == "gcp" else gpu_count * procs_per_gpu
    return per_task * (tasks if backend == "gcp" else 1)


def backend_of(argv):
    """Le fournisseur, lu avant argparse.

    `--list` et `--kill-all` sont traités avant le parseur — ils doivent
    marcher même quand le lancement est cassé — et ils ont pourtant besoin de
    savoir à qui parler."""
    for i, a in enumerate(argv):
        if a == "--backend" and i + 1 < len(argv):
            return argv[i + 1]
        if a.startswith("--backend="):
            return a.split("=", 1)[1]
    return "gcp"


def main():
    # Le montage, séparé du rendu : aucune tâche ne voit tous les segments, et
    # celui-ci peut tourner des heures après que la dernière a rendu la main.
    if len(sys.argv) > 2 and sys.argv[1] == "--assemble":
        assemble(sys.argv[2])
        return

    # Deux verbes avant tout le reste, parce qu'ils doivent marcher même quand
    # le lancement est cassé : voir ce qui tourne, et tout arrêter.
    #
    # (`--bake` est parsé plus bas, avec le reste : il prend des paramètres.)
    if len(sys.argv) > 1 and sys.argv[1] == "--capacity":
        # Ce que la console montre sur son écran de déploiement, en une
        # commande rejouable. La capacité a coûté plus de temps que n'importe
        # quel bug cette semaine, et « réessaie » n'est pas une réponse quand
        # on peut regarder.
        key = api_key()
        count = int(sys.argv[2]) if len(sys.argv) > 2 else 1
        rows = []
        for cloud in ("COMMUNITY", "SECURE"):
            # `/catalog/gpus`, trouvé dans l'openapi plutôt que deviné :
            # /gputypes, /gpuTypes, /gpu-types et /gpus rendent tous 404.
            data = call(key, "GET",
                        f"/catalog/gpus?include=AVAILABILITY&product=POD"
                        f"&count={count}&cloud={cloud}")
            for g in data.get("gpus", []):
                if g.get("availability") in (None, "NONE"):
                    continue
                price = (g.get("price") or {}).get(cloud.lower())
                rows.append((price or 99, cloud, g.get("id", "?"),
                             g.get("memory"), g.get("availability")))
        if not rows:
            print(f"aucune carte disponible à {count} GPU, sur aucun cloud")
            return
        print(f"disponible à {count} GPU, du moins cher au plus cher :")
        for price, cloud, name, mem, avail in sorted(rows):
            print(f"  {price:6.2f} $/h  {cloud:<9} {avail:<7} "
                  f"{mem:>4} Go  {name}")
        return

    if len(sys.argv) > 1 and sys.argv[1] in ("--list", "--kill-all"):
        if backend_of(sys.argv) == "gcp":
            token = gcp_token()
            rows = gcp_executions(token)
            live = [e for e in rows if not e.get("completionTime")]
            if not rows:
                # « Aucune exécution » et « aucun job » se ressemblent trop :
                # executions.list sur un job absent répond une liste vide, pas
                # un 404. La confusion inverse a coûté dix-huit dollars côté
                # RunPod, où six pods tournaient pendant qu'on lisait zéro.
                # Un appel de plus, dans le seul cas où il est gratuit.
                try:
                    gcp_call("GET", gcp_job_path(), token=token)
                except SystemExit:
                    print(f"le job {GCP_JOB} n'existe pas dans "
                          f"{GCP_PROJECT}/{GCP_REGION} — `pulumi up` dans "
                          "sportstracklive-rails/infra")
                    return
                print("aucune exécution")
                return
            for ex in rows[:20]:
                print(gcp_execution_line(ex))
            if sys.argv[1] == "--kill-all":
                if not live:
                    print("rien en cours — aucune seconde de GPU en jeu")
                    return
                for ex in live:
                    gcp_call("POST", f"{ex['name']}:cancel", {}, token=token)
                    print(f"annulée {ex['name'].rsplit('/', 1)[-1]}")
                still = [e for e in gcp_executions(token)
                         if not e.get("completionTime")]
                print(f"encore en cours: {len(still)}")
            return
        key = api_key()
        pods = all_pods(key)
        if not pods:
            print("aucun pod")
            return
        for pod in pods:
            gpu = (pod.get("gpu") or {})
            print(f"{pod['id']}  {pod.get('name','')}  "
                  f"{gpu.get('count','?')}x {gpu.get('id','?')}  "
                  f"{pod.get('cost','?')} $/h  {pod.get('status','')}")
        if sys.argv[1] == "--kill-all":
            for pod in pods:
                call(key, "DELETE", f"/pods/{pod['id']}")
                print(f"supprimé {pod['id']}")
            left = all_pods(key)
            print(f"restants: {len(left)}")
        return

    p = argparse.ArgumentParser()
    p.add_argument("--backend", default="gcp", choices=["gcp", "runpod"],
                   help="où le rendu a lieu. gcp = Cloud Run Jobs, une tâche "
                        "= un L4 entier, facturé à la seconde ; l'image et la "
                        "taille de machine viennent du job décrit par Pulumi, "
                        "pas d'ici. runpod = un pod, N GPU, M processus par "
                        "GPU — conservé parce qu'il marche, et parce que leur "
                        "découpage GPU finira par changer.")
    p.add_argument("--tasks", type=int, default=1, metavar="N",
                   help="(gcp) tâches parallèles, un GPU chacune. Le quota "
                        "accordé d'office est de 3 GPU ; au-delà il faut le "
                        "demander. Chaque tâche calcule sa tranche de "
                        "--frames depuis CLOUD_RUN_TASK_INDEX, et le montage "
                        "se fait après coup par --assemble.")
    p.add_argument("--bake", action="store_true",
                   help="lance le job A — cuire --trajectory en un pack et le "
                        "déposer sur R2 — au lieu de rendre. Pas de GPU : "
                        "cette moitié traverse le globe et tire les tuiles, "
                        "l'autre ne fait que lire ce qu'elle a laissé. "
                        "--scene donne le digest sous lequel déposer.")
    p.add_argument("--dry-run", action="store_true",
                   help="(gcp) valide la demande sans exécuter — validateOnly. "
                        "Une erreur de forme se paie zéro seconde de L4.")
    p.add_argument("--stage", default="", help=".usda à rendre (embarquée gzip+b64)")
    p.add_argument("--stage-url", default="",
                   help="URL GET (présignée) du .usda — la voie des vraies "
                        "tracks enregistrées")
    p.add_argument("--trajectory", default="",
                   help="la voie générative : le pod fabrique son manifeste "
                        "(ex: orbit:1440:2.17:42.52:8000:5000, zoom:64)")
    p.add_argument("--sse", type=float, default=3.0)
    p.add_argument("--imagery", type=int, default=0, metavar="ASSET",
                   help="l'asset ion d'imagerie. 0 = le défaut du bake (2, "
                        "Bing Aerial) ; 3954 = Sentinel-2 cloudless d'EOX, "
                        "dont le descripteur plafonne au niveau 13 ; négatif = "
                        "pas d'imagerie du tout, la vue géométrique. La valeur "
                        "entre dans la clé du pack et dans le digest de scène.")
    p.add_argument("--terrain", type=int, default=0, metavar="ASSET",
                   help="l'asset ion de terrain. 0 = le défaut du bake (1, "
                        "Cesium World Terrain).")
    p.add_argument("--imagery-boost", type=int, default=1, metavar="N",
                   help="de combien de niveaux l'imagerie peut descendre sous "
                        "le terrain, à la cuisson. Le défaut du code est 1, "
                        "choisi pour une machine de bureau : le commentaire de "
                        "`globe.rs` raconte un essai local monté à 60 Go. Un "
                        "job de ferme a 32 Gio — mais mesuré le 17 septembre "
                        "2026, 2 fait tuer la cuisson (SIGKILL) sauf à baisser "
                        "TUILE_PINNED_LEVEL, et coûte alors 65 min et 11,3 Go "
                        "pour une minute de film, contre 32 min et 2,1 Go. "
                        "Chaque niveau quadruple l'imagerie par tuile, et la "
                        "valeur entre dans la clé du pack.")
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
                        "Ce que l'image sait faire tourner, précisément : "
                        "les noyaux OptiX sont du PTX, que le pilote compile "
                        "à la volée, donc ils marchent sur n'importe quelle "
                        "carte assez récente — 4090 comprise. Le noyau CUDA, "
                        "lui, est un cubin sm_120 : Blackwell uniquement. La "
                        "sonde essaie OptiX d'abord et annonce le backend "
                        "retenu, donc une carte Ada rend par OptiX ou ne rend "
                        "pas du tout, et le job le dit en quelques secondes. "
                        "Défaut : 4090 (0,34 $/h), puis 5080 (0,39), puis "
                        "5090 (0,69).")
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
    p.add_argument("--cheapest-available", action="store_true",
                   help="interroger le catalogue à chaque tour et prendre la "
                        "carte la moins chère réellement libre, au lieu de "
                        "POSTer une liste fixe à l'aveugle. Ignore "
                        "--gpu-type. Un minimum de VRAM est imposé : 16 Go, "
                        "sous quoi un path trace à 1280x960 n'a pas la place.")
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
    p.add_argument("--blender-args", default="",
                   help="args supplémentaires de BLENDER, avant -P — pas du "
                        "script. C'est par là que passe `--debug-cycles` : "
                        "Cycles ne journalise que si la ligne de commande le "
                        "demande, et un rendu qui ne démarre pas est sinon "
                        "parfaitement muet.")
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

    # Cuire est un autre job, pas une option du rendu : autre point d'entrée,
    # autre machine, aucune carte. Rien de ce qui suit ne le concerne.
    if args.bake:
        if args.backend != "gcp":
            raise SystemExit("--bake n'existe que sur gcp")
        if not args.trajectory:
            raise SystemExit("--bake veut --trajectory (ex: "
                             "orbit:1440:2.17:42.52:8000:5000)")
        check_frames(args.trajectory, args.frames)
        bake(args)
        return

    if args.trajectory:
        check_frames(args.trajectory, args.frames)

    # Pas de pack ? On le cuit, puis on rend ce qu'on vient de cuire.
    #
    # La chaîne va des paramètres de scène au mp4 sur R2, en une invocation :
    #
    #     paramètres → bake (job A) → tuilepack sur R2
    #                → render (job B) → segments → montage → mp4 sur R2
    #
    # Le pack est nommé d'après les paramètres de cuisson, donc la clef est
    # connue d'avance et une scène déjà cuite est réutilisée telle quelle —
    # recuire 1440 frames pour rendre deux fois le même plan serait treize
    # minutes et une poignée de dollars jetés.
    if args.backend == "gcp" and args.engine == "hydra" and not args.pack:
        if not args.trajectory:
            raise SystemExit(
                "--engine hydra sur gcp veut --trajectory (la scène) ou "
                "--pack (une scène déjà cuite)")
        key = pack_key(args)
        if pack_exists(key):
            print(f"pack déjà cuit: {key}", flush=True)
        else:
            print(f"pack absent, cuisson d'abord: {key}", flush=True)
            bake(args)
        args.pack = key
        if not args.scene:
            args.scene = scene_digest_of(key)

    if not args.gpu_type:
        # Du moins cher au plus cher. La 4090 n'est pas Blackwell : elle rendra
        # par OptiX (PTX compilé par le pilote) et jamais par le backend CUDA
        # (cubin sm_120). La sonde annonce lequel des deux a été retenu, donc
        # l'essai coûte quelques secondes et se lit dans le journal.
        args.gpu_type = ["NVIDIA GeForce RTX 4090",
                         "NVIDIA GeForce RTX 5080",
                         "NVIDIA GeForce RTX 5090"]
    if args.engine == "hydra" and args.image == p.get_default("image"):
        args.image = ECR_HOST + "/stl/blender-globe:5.1-su"

    key = reg = None
    if args.backend == "runpod":
        key = api_key()
        regs = call(key, "GET", "/registries")
        reg = next((r for r in regs.get("registries", [])
                    if r.get("name") == args.registry_name), None)
        if reg is None:
            raise SystemExit(
                f"credential registre '{args.registry_name}' absent — "
                "à créer une fois (secret hors conversation)")
    else:
        # Une tâche Cloud Run reçoit un GPU entier, par contrat. Le reste des
        # drapeaux de dimensionnement décrit la machine, et la machine est
        # décrite par Pulumi : les accepter ici ferait croire qu'ils agissent.
        if args.gpu_count != 1:
            raise SystemExit(
                "--gpu-count n'a pas de sens sur gcp : une tâche = un GPU. "
                "C'est --tasks qui donne le parallélisme.")
        if args.tasks < 1:
            raise SystemExit("--tasks doit valoir au moins 1")
        # `containerOverrides` ne porte pas d'image : celle qui tourne est
        # celle du job, poussée par build-push.sh --gcp dans le dépôt que
        # render_farm.py nomme. Le dire plutôt que de laisser croire.
        args.image = gcp_image_of_job()

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
        # Sur gcp, une tâche voit exactement un GPU — c'est le contrat du
        # service, et la sonde `probe: NGPU 1` doit le confirmer. Sur runpod
        # c'est ce qu'on a demandé, et la même sonde a passé quatre soirées à
        # dire non.
        "JOB_GPUS": "1" if args.backend == "gcp" else str(args.gpu_count),
        "JOB_PROCS_PER_GPU": str(args.procs_per_gpu),
        "JOB_BATCH_FRAMES": str(args.batch_frames),
        "JOB_EXTRA_ARGS": args.extra,
        "JOB_BLENDER_ARGS": args.blender_args,
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
        # La cadence, tirée de la trajectoire — voir `fps_of`.
        #
        # `render_job.sh` la tenait pour 24, en dur, et s'en sert deux fois :
        # pour générer les poses du manifeste, et pour le `-framerate` de
        # ffmpeg sur chaque segment. Une trajectoire à 60 aurait donc produit
        # des poses qui ne correspondent à aucune frame du pack — échec
        # bruyant — et, si elle y avait survécu, un film de cinq minutes pour
        # deux minutes de vol. Le second se serait appelé une réussite.
        env["JOB_FPS"] = str(fps_of(args.trajectory))
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
    manifest_digest = image_digest(args.image)
    tasks = args.tasks if args.backend == "gcp" else 1
    # Un segment par processus, sur toutes les tâches. Leur numéro est global :
    # la tâche t produit t*procs .. (t+1)*procs-1, et c'est ce qui permet à
    # --assemble de les remettre dans l'ordre sans demander qui a fait quoi.
    segments = segment_count(args.backend, tasks, args.gpu_count,
                             args.procs_per_gpu)
    manifest_client, urls = archive_urls(run, segments, tasks)
    if not args.upload_url:
        env["JOB_UPLOAD_PUT_URL"] = urls["render.mp4"]
    env["JOB_LOGS_PUT_URL"] = urls["logs.tar.gz"]
    env["JOB_TRACE_PUT_URL"] = urls["trace.tar.gz"]
    env["JOB_PROFILE_PUT_URL"] = urls["profile.tar.gz"]
    for t in range(tasks if tasks > 1 else 0):
        env[f"JOB_LOGS_PUT_URL_{t}"] = urls[f"logs-t{t}"]
        env[f"JOB_TRACE_PUT_URL_{t}"] = urls[f"trace-t{t}"]
        env[f"JOB_PROFILE_PUT_URL_{t}"] = urls[f"profile-t{t}"]
    for i in range(segments):
        env[f"JOB_SEG_PUT_URL_{i}"] = urls[f"seg{i}"]
    # Le cache OptiX, partagé entre exécutions.
    #
    # La clef porte la carte ET le digest de l'image : un changement de noyaux
    # ou de carte doit invalider le cache, pas le réutiliser de travers. Le GET
    # peut échouer — la première fois, il n'y a rien — et le job le traite comme
    # un cache vide, ce qui est exactement ce que c'est.
    accel = "nvidia-l4" if args.backend == "gcp" else "runpod"
    digest = (manifest_digest or "nodigest").replace(":", "-")[:19]
    optix_key = f"cache/optix/{accel}-{digest}.tar.gz"
    env["JOB_OPTIX_CACHE_GET_URL"] = manifest_client.generate_presigned_url(
        "get_object", Params={"Bucket": R2_BUCKET, "Key": optix_key},
        ExpiresIn=7 * 24 * 3600)
    env["JOB_OPTIX_CACHE_PUT_URL"] = manifest_client.generate_presigned_url(
        "put_object", Params={"Bucket": R2_BUCKET, "Key": optix_key},
        ExpiresIn=7 * 24 * 3600)

    if args.profile:
        env["TUILE_PROFILE"] = args.profile
        env["TUILE_PROFILE_DIR"] = "/out/profile"
    manifest = {
        "run_id": run,
        "launched_utc": datetime.now(timezone.utc).isoformat(),
        "argv": sys.argv[1:],
        "git": git_info,
        "image": args.image,
        "image_digest": manifest_digest,
        "pod_env": redacted(env),
        "params": {k: v for k, v in vars(args).items()
                   if k not in ("ssh_pubkey",)},
    }

    if args.backend == "gcp":
        token = gcp_token()
        manifest["backend"] = {
            "kind": "gcp", "project": GCP_PROJECT, "region": GCP_REGION,
            "job": GCP_JOB, "tasks": tasks, "segments": segments,
        }
        # Le manifeste part AVANT l'exécution.
        #
        # Côté RunPod il attendait la création du pod, pour porter son
        # identité. Ici il n'y a rien à attendre : la forme de la machine est
        # dans Pulumi, pas dans la réponse. Et un manifeste déposé d'abord est
        # un manifeste qui existe même quand la demande est refusée — ce qui
        # est exactement le moment où on veut lire ce qu'on avait demandé.
        manifest_client.put_object(
            Bucket=R2_BUCKET, Key=f"{R2_PREFIX}/{run}/config.json",
            Body=json.dumps(manifest, indent=2, sort_keys=True).encode(),
            ContentType="application/json")
        print(f"archive: s3://{R2_BUCKET}/{R2_PREFIX}/{run}/", flush=True)

        name = gcp_run(env, tasks, dry_run=args.dry_run, token=token)
        if name is None:
            print("validateOnly: la demande est acceptée, rien n'a tourné "
                  "et rien n'est facturé.")
            return
        print(f"exécution: {name.rsplit('/', 1)[-1]}  "
              f"{tasks} tâche(s) × 1 L4  frames {args.frames}", flush=True)
        ok, bad = gcp_watch(name, token=token)
        print(f"{ok} tâche(s) réussie(s), {bad} en échec", flush=True)
        if bad == 0 and tasks > 1:
            # Le montage suit le rendu, sans qu'on le demande.
            #
            # Il a été une commande à taper pendant quelques heures, et c'est
            # une faute : la chaîne va des paramètres de scène au mp4 sur R2,
            # et une étape qui attend qu'un humain la lance n'est pas une
            # chaîne. Aucune tâche ne voit tous les segments — c'est pour ça
            # que le montage est ici et non dans le job — mais « ailleurs que
            # dans le job » ne veut pas dire « à la main ».
            print("montage des segments…", flush=True)
            assemble(run, fps=fps_of(args.trajectory))
        if bad:
            raise SystemExit(
                "des tâches ont échoué — les journaux de chacune sont dans "
                f"s3://{R2_BUCKET}/{R2_PREFIX}/{run}/logs-t*.tar.gz, et le "
                "premier mot à y chercher est GPU-PARTIAL-HOST : c'est lui "
                "qui a disqualifié quatre hôtes RunPod, et sur Cloud Run il "
                "doit dire « 1 advertised, 1 device node ».")
        return

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
            if args.cheapest_available:
                free = [(c, k) for _, c, k, mem
                        in available_cards(key, args.gpu_count) if mem >= 16]
                if free:
                    print("libre : " + ", ".join(f"{k} ({c})" for c, k in free[:4]),
                          flush=True)
                for cloud, kind in free:
                    body["gpu"] = gpu_spec(kind)
                    body["cloud"] = cloud
                    try:
                        return call(key, "POST", "/pods", body), kind
                    except SystemExit as e:
                        if "no longer any instances" in str(e):
                            unavailable.append(kind)
                            continue
                        raise
                if not args.wait:
                    raise SystemExit("aucune carte libre")
                print(f"essai {attempt}: la capacité a filé entre la question "
                      "et la réponse — on redemande (20 s)", flush=True)
                import time
                time.sleep(20)
                continue
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
