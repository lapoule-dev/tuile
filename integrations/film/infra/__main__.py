# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The film jobs: `tuile-film-produce` as a Cloud Run job, one per card.

A job is a definition — image, card, resources, timeout, the stores' keys —
and each film is an execution of it, its arguments given by
`tuile-film-job` (`docs/22-film-jobs.md`). Nothing here is a service: a job
runs when asked, exits when its film is made, and bills nothing between.
These jobs share no name with any service, and changing them changes no
service.

Config (set by `up.sh` from the environment):
  gcp:project            the project
  image                  the film image, `<registry>/film:<commit>`
  serviceAccount         the identity the jobs run as
  storeEndpoint          the run store's S3 endpoint (not a secret)
  storeBucket, tilesBucket

The keys are never in this program, its state or an execution: each job's
definition names Secret Manager secrets that already exist in the project —
`farm-store-access-key-id`, `farm-store-secret-access-key` (the run and
tile stores' S3 key) and `farm-cesium-ion-token` (the bake's sources) — and
the service account reads them (`roles/secretmanager.secretAccessor`, as the
project's farm already grants it).
"""

import pulumi
import pulumi_gcp as gcp

config = pulumi.Config()
image = config.require("image")
service_account = config.require("serviceAccount")

env = gcp.cloudrunv2.JobTemplateTemplateContainerEnvArgs


def secret(name, secret_id):
    """An environment variable whose value is a secret, by reference."""
    return env(
        name=name,
        value_source=gcp.cloudrunv2.JobTemplateTemplateContainerEnvValueSourceArgs(
            secret_key_ref=gcp.cloudrunv2.JobTemplateTemplateContainerEnvValueSourceSecretKeyRefArgs(
                secret=secret_id, version="latest"
            )
        ),
    )


envs = [
    env(name="TUILE_STORE_ENDPOINT", value=config.require("storeEndpoint")),
    env(name="TUILE_STORE_BUCKET", value=config.require("storeBucket")),
    env(name="TUILE_TILES_BUCKET", value=config.require("tilesBucket")),
    env(name="RUST_LOG", value="info"),
    secret("TUILE_STORE_ACCESS_KEY_ID", "farm-store-access-key-id"),
    secret("TUILE_STORE_SECRET_ACCESS_KEY", "farm-store-secret-access-key"),
    secret("TUILE_ION_TOKEN", "farm-cesium-ion-token"),
]

# One job a card. The L4 encodes H.264 and AV1; MV-HEVC — two eyes in one
# stream — wants a card whose encoder writes it (an RTX PRO 6000).
JOBS = [
    # name, region, accelerator, cpu, memory
    ("tuile-film-oneoff", "europe-west1", "nvidia-l4", "8", "32Gi"),
    ("tuile-film-oneoff-spatial", "europe-west4", "nvidia-rtx-pro-6000", "20", "80Gi"),
]

for name, region, accelerator, cpu, memory in JOBS:
    job = gcp.cloudrunv2.Job(
        name,
        name=name,
        location=region,
        deletion_protection=False,
        template=gcp.cloudrunv2.JobTemplateArgs(
            task_count=1,
            template=gcp.cloudrunv2.JobTemplateTemplateArgs(
                # A failed film is read, not retried blind.
                max_retries=0,
                # The most Cloud Run gives a task with a card. A longer film
                # is more tasks, or the same command again: a production
                # resumes from the parts already in the store.
                timeout="3600s",
                service_account=service_account,
                gpu_zonal_redundancy_disabled=True,
                node_selector=gcp.cloudrunv2.JobTemplateTemplateNodeSelectorArgs(
                    accelerator=accelerator
                ),
                containers=[
                    gcp.cloudrunv2.JobTemplateTemplateContainerArgs(
                        image=image,
                        commands=["tuile-film-produce"],
                        envs=envs,
                        resources=gcp.cloudrunv2.JobTemplateTemplateContainerResourcesArgs(
                            limits={"cpu": cpu, "memory": memory, "nvidia.com/gpu": "1"}
                        ),
                    )
                ],
            ),
        ),
    )
    pulumi.export(name, job.id)
