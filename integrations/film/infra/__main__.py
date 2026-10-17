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
  store*, tilesBucket    the run store and the tile store (secret)
"""

import pulumi
import pulumi_gcp as gcp

config = pulumi.Config()
image = config.require("image")
service_account = config.require("serviceAccount")

env = [
    gcp.cloudrunv2.JobTemplateTemplateContainerEnvArgs(name=name, value=value)
    for name, value in [
        ("TUILE_STORE_ENDPOINT", config.require_secret("storeEndpoint")),
        ("TUILE_STORE_BUCKET", config.require("storeBucket")),
        ("TUILE_STORE_ACCESS_KEY_ID", config.require_secret("storeAccessKeyId")),
        ("TUILE_STORE_SECRET_ACCESS_KEY", config.require_secret("storeSecretAccessKey")),
        ("TUILE_TILES_BUCKET", config.require("tilesBucket")),
        ("RUST_LOG", "info"),
    ]
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
                timeout="21600s",
                service_account=service_account,
                gpu_zonal_redundancy_disabled=True,
                node_selector=gcp.cloudrunv2.JobTemplateTemplateNodeSelectorArgs(
                    accelerator=accelerator
                ),
                containers=[
                    gcp.cloudrunv2.JobTemplateTemplateContainerArgs(
                        image=image,
                        commands=["tuile-film-produce"],
                        envs=env,
                        resources=gcp.cloudrunv2.JobTemplateTemplateContainerResourcesArgs(
                            limits={"cpu": cpu, "memory": memory, "nvidia.com/gpu": "1"}
                        ),
                    )
                ],
            ),
        ),
    )
    pulumi.export(name, job.id)
