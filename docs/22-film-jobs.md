# 22 — Films as one-off GPU jobs

One way to make a whole film on a cloud GPU without touching any running
service: a Cloud Run **job** per card, declared by Pulumi, and a launcher
that runs one execution per film and brings the film home.

## Pieces

| Piece | Where | What |
|---|---|---|
| Job definitions | `integrations/film/infra/` (Pulumi, Python) | `tuile-film-oneoff` (L4, `europe-west1`: H.264, AV1) and `tuile-film-oneoff-spatial` (RTX PRO 6000, `europe-west4`: MV-HEVC). Command `tuile-film-produce`; image, identity and stores from config. One task, no retry, an hour at most (Cloud Run's limit for a task with a card): a longer film takes more tasks, or the same command again — a production resumes from the parts already in the store. |
| Image | `integrations/film/Dockerfile` | `tuile-film-produce`, `-render`, `-serve` on Vulkan + NVENC. Tagged by commit. |
| Launcher | `tuile-launch-job film` (`crates/tuile-farm`) | Puts the tape in the run's prefix (`renders/<run>/traj.mcap`, beside its manifest and receipt), starts an execution with the production's flags as its arguments, follows it, reads the receipt, downloads the film to `videos/` and opens it. |
| Production | `tuile-film-produce` | Bakes, renders in parts, joins (H.264 or spatial), writes `--receipt`. |

A job is a process. When the film is joined it exits and the card is
released: nothing is left running and nothing bills between films. The
production services keep their own names, images and configuration; the
jobs share none of them.

## Use

```bash
# once, and whenever the image changes
docker buildx build --builder <builder> -f integrations/film/Dockerfile \
  -t <registry>/film:<commit> --push .
TUILE_GCP_PROJECT=… TUILE_FILM_IMAGE=<registry>/film:<commit> \
TUILE_FILM_SERVICE_ACCOUNT=… TUILE_STORE_…=… TUILE_TILES_BUCKET=… \
  integrations/film/infra/up.sh          # preview; --yes to apply

# a film: the tape states its frames' instants
tuile-film-timelapse sky.mcap place=0.14,42.94 height=4000 \
  start=2026-10-19T00:00Z days=14 length=120 fps=60 track=sun-moon pitch=-4 fov=70
TUILE_GCP_PROJECT=… TUILE_STORE_…=… \
  tuile-launch-job film --tape sky.mcap --name summit-14-days -- \
    --atmosphere --night-lights --sun-scale 3 --moon-scale 5 --fps 60 --mbps 20

# two eyes, for a headset
tuile-launch-job film --tape limb.mcap --spatial --name limb-300km-spatial -- \
  --atmosphere --night-lights --fps 60 --codec mv-hevc \
  --stereo side-by-side --stereo-convergence 440000
```

The frames' instants are in the tape (`tuile-film-timelapse` writes them);
the production reads them from it. `--dry-run` has the service validate the
request and run nothing; `--no-watch` returns once started, and
`TUILE_RUN=<run> … --attach <execution>` follows it later.

## Rules

- Never point the launcher at a service: it runs jobs, by name.
- A failed task is not retried by the job: its logs are read first.
- Every film is opened when it arrives (`CLAUDE.md`, "Every render gets
  opened"), and goes to `videos/` with its parameters in `videos/README.md`.
