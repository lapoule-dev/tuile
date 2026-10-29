<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-film-native

A film rendered natively, from its packs and the tile store.

The same film a browser renders, by the same parts — the pack's plan
(`tuile_film::Cursor`), the store's tiles made into meshes and drapes
(`tuile_film::from_store`), the GPU's composition and frame
(`tuile_film_gpu`) — on a machine's own adapter, with no page around it.

Three things, kept apart:

- **what is read** ([`source`]): packs and the tile store, both behind
  `tuile_repository::Objects`, both through a cache of chunks on disk,
  both counted;
- **the render** ([`render`](fn@render)): frame after frame, tiles entering and
  leaving as the plan says, each picture handed to a [`Sink`];
- **what watches** ([`observe`]): an [`Observer`] is told of every
  imagery tile that comes in, every tile built, every picture that comes
  out. The render knows nothing of what an observer does with it;
  [`meter::LightMeter`] is one, and measures light; [`seams::SeamLog`]
  another, and measures what is left between the tiles a frame draws;
  [`discs::DiscEnergy`] a third, and measures the light of the sun's and
  the Moon's discs against what the air lets through.

Beside them, [`reference`](mod@reference) reads another layer of the store under a
film's imagery, for its tiles to be measured against.

## Where it sits

Depends on `tuile-film`, `tuile-film-gpu`, `tuile-bake`, `tuile-pack`,
`tuile-repository`, `tuile-farm`, `tuile-mp4`, `tuile-radiometry`,
`tuile-storage-foyer`, `tuile-tape`, `tuile-atmosphere` and `tuile-core`. The
top of the film stack: nothing in the workspace depends on it.

## Example

An instrument is an [`Observer`]; this one counts pictures:

```rust
use tuile_film_native::{FrameOut, Observer};

#[derive(Default)]
struct Frames(usize);

impl Observer for Frames {
    fn frame(&mut self, _frame: &FrameOut<'_>) {
        self.0 += 1;
    }
}
```

## Command-line tools

| Binary | What it does |
|---|---|
| `tuile-film-produce` | A whole film from a tape of cameras: bake, render, join, all kept in the run store and resumable. |
| `tuile-film-render` | Renders a film from its packs and the tile store, to an mp4, PNGs, or measurements. |
| `tuile-film-serve` | The render as an HTTP service: `POST /segments`. |
| `tuile-film-compress` | One film in, delivered by a profile (`h264`, `hevc-main10`, `mezzanine`, `copy`); or `--probe` a film. |
| `tuile-film-timelapse` | Writes the tape of a time-lapse of the sky. |

```bash
cargo run --release -p tuile-film-native --bin tuile-film-produce -- \
    flight.mcap --resolution 3840x2160 --fps 30 --sse 3
cargo run --release -p tuile-film-native --bin tuile-film-render -- \
    packs/<scene> --out film.mp4 --frames 1:450
cargo run --release -p tuile-film-native --bin tuile-film-compress -- \
    film.mp4 delivered.mp4 h264 --cap-mb 50
cargo run --release -p tuile-film-native --bin tuile-film-timelapse -- \
    sky.mcap place=0.14,42.94 height=4000 start=2026-10-19T00:00Z days=14 length=120 fps=60
```

`tuile-film-render` has many more flags (codec, tone, calibration, stereo,
atmosphere, overlays): its rustdoc page holds the full list, the same text it
was written from. The stores come from the environment (`TUILE_STORE_*`,
`TUILE_TILES_*`), as each binary's page says.

## Features

- `asm` — rav1e's own assembly: several times faster AV1, no longer Rust
  alone.
- `videotoolbox` — H.264 by the machine's own hardware encoder on macOS;
  ignored elsewhere.
- `nvenc` — AV1 or H.264 by an NVIDIA card's encoder on Linux; ignored
  elsewhere.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/18-color-harmonization.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/18-color-harmonization.md)
- [`docs/19-sky-atmosphere.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/19-sky-atmosphere.md)
