#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Dépose un pack cuit sur R2, sous la clé que `tuile-bake` a imprimée.

    tuile-bake --tape shot.mcap --frames 1:48 --out shot.tuilepack
    # -> BAKE-KEY packs/<scene>/1-48.tuilepack
    ./push_pack.py shot.tuilepack packs/<scene>/1-48.tuilepack

Job A tourne sur une machine de confiance — le builder, ou ce poste — donc il
lit les identifiants directement plutôt que par une URL présignée : c'est le
pod qui ne doit jamais en voir, pas nous.

Le téléversement est multipart et repris par tronçons : un pack de plusieurs
gigaoctets sur une liaison domestique n'est pas un `put_object`, et un
téléversement qui casse à 90 % sans reprendre est un téléversement qu'on ne
refait pas.
"""

import argparse
import importlib.util
import pathlib
import sys

_spec = importlib.util.spec_from_file_location(
    "launch_job", pathlib.Path(__file__).with_name("launch_job.py"))
assert _spec is not None and _spec.loader is not None
launch_job = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(launch_job)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("pack", help="le fichier .tuilepack local")
    p.add_argument("key", help="la clé R2 (celle de BAKE-KEY)")
    args = p.parse_args()

    path = pathlib.Path(args.pack)
    if not path.is_file():
        raise SystemExit(f"{path} n'existe pas")
    if not args.key.startswith("packs/"):
        # Le préfixe `renders/` est celui des archives de run, et un pack qui
        # y atterrit sera balayé avec elles.
        raise SystemExit(f"une clé de pack commence par packs/ — reçu {args.key}")

    try:
        import boto3
        from boto3.s3.transfer import TransferConfig
        from botocore.config import Config as BotoConfig
    except ImportError:
        raise SystemExit("boto3 absent — `pip install boto3`")

    access, secret = launch_job.r2_credentials()
    if not access or not secret:
        raise SystemExit(
            "identifiants R2 introuvables — ni dans l'environnement ni dans "
            "Pulumi (TUILE_PULUMI_DIR, par défaut "
            "../sportstracklive-rails/infra)")

    client = boto3.client(
        "s3",
        endpoint_url=f"https://{launch_job.R2_ACCOUNT}.r2.cloudflarestorage.com",
        aws_access_key_id=access, aws_secret_access_key=secret,
        region_name="auto", config=BotoConfig(signature_version="s3v4"))

    size = path.stat().st_size
    print(f"{path} → s3://{launch_job.R2_BUCKET}/{args.key}  "
          f"({size / 1024 / 1024:.0f} Mo)", flush=True)
    seen = [0]

    def progress(n):
        seen[0] += n
        pct = 100 * seen[0] / size if size else 100
        print(f"\r  {pct:5.1f}%", end="", flush=True)

    client.upload_file(
        str(path), launch_job.R2_BUCKET, args.key, Callback=progress,
        Config=TransferConfig(multipart_threshold=64 * 1024 * 1024,
                              multipart_chunksize=64 * 1024 * 1024,
                              max_concurrency=4))
    print(f"\nPACK-PUSHED {args.key}")


if __name__ == "__main__":
    sys.exit(main())
