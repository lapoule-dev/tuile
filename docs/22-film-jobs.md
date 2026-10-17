# 22 — Films as one-off GPU jobs

One way to make a whole film on a cloud GPU without touching any running
service: a Cloud Run **job** per card, declared by Pulumi, and a launcher
that runs one execution per film and brings the film home.

## Pieces

| Piece | Where | What |
|---|---|---|
| Job definitions | `integrations/film/infra/` (Pulumi, Python) | `tuile-film-oneoff` (L4, `europe-west1`: H.264, AV1) and `tuile-film-oneoff-spatial` (RTX PRO 6000, `europe-west4`: MV-HEVC). Command `tuile-film-produce`; image, identity and stores from config. One task, no retry, six hours at most. |
| Image | `integrations/film/Dockerfile` | `tuile-film-produce`, `-render`, `-serve` on Vulkan + NVENC. Tagged by commit. |
| Launcher | `tuile-film-job` (`crates/tuile-film-native`) | Puts the tape in the run store, starts an execution with the production's flags as its arguments, follows it, reads the receipt, downloads the film to `videos/` and opens it. |
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

# a film
tuile-tape timelapse sky.mcap place=0.14,42.94 height=4000 \
  start=2026-10-19T00:00Z days=14 length=120 fps=60 spin=10 pitch=8 fov=70
TUILE_GCP_PROJECT=… TUILE_STORE_…=… \
  tuile-film-job sky.mcap --name summit-14-days -- \
    --sky-only --atmosphere --fps 60 --mbps 20

# two eyes, for a headset
tuile-film-job limb.mcap --spatial --name limb-300km-spatial -- \
  --sky-only --atmosphere --fps 60 --codec mv-hevc \
  --stereo side-by-side --stereo-convergence 495000
```

The tape's clock (`<tape>.clock`, written by `tuile-tape timelapse`) is
passed as `--clock` unless the flags give one. `--dry-run` has the service
validate the request and run nothing; `--no-watch` returns once started,
and `--attach <execution> --run <run>` follows it later.

## Rules

- Never point the launcher at a service: it runs jobs, by name.
- A failed task is not retried by the job: its logs are read first.
- Every film is opened when it arrives (`CLAUDE.md`, "Every render gets
  opened"), and goes to `videos/` with its parameters in `videos/README.md`.
