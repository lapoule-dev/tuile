<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-record

A recorded viewer session, made readable.

The viewer records a session into one MCAP file (`--record <file.mcap>`, or
`O` / View > Record Session): each frame's camera on `/camera`, the wall and
scene clocks, the viewport and the render origin on `/viewer/frame`, what the
atmosphere lit it with on `/viewer/sky`, the input on `/viewer/input`, and
**the picture it drew, as it was on the screen**, on `/frame` — all on one
timeline, a picture carrying the index of the frame it shows
(`tuile_tape::session`). The camera and the clocks are what a replay needs to
put the renderer back at any frame; the pictures are what was seen, which no
replay reproduces: the tiles resident at that moment, the streaming's timing,
the exposure's history.

This crate reads such a file:

- **export** — the pictures as an mp4 (H.264 by the Mac's encoder, or AV1 by
  rav1e: the film's own encoders, never ffmpeg) and, frame by frame, the
  pose, the clocks, the sky and what the picture measures — the sky's and the
  ground's mean luma, the change since the last picture, the star-like points,
  the black pixels — as a Parquet sidecar ([`sidecar`]);
- **diagnose** — the frames where the sky or the ground stepped in brightness,
  or the stars thinned out at once, while the camera barely moved — a change
  its motion does not explain — or where ground came out black ([`flag`]), each
  with its pose, its clocks and a `tuile://goto` link that brings a viewer
  back there ([`link`]);
- **plots** — every measure against the frame, the altitude, the heading, the
  pitch and the render origin's step, flagged frames in red, as one PNG
  ([`plots`]): at which poses a defect happens is a look.

```bash
cargo run --release -p tuile-record -- export session.mcap \
    --mp4 session.mp4 --parquet session.parquet --plots session.png --burn-in
cargo run --release -p tuile-record -- diagnose session.parquet --luma-step 0.03
cargo run --release -p tuile-record -- goto session.parquet 1234 --open
```

Measured from the recorded pixels, so a JPEG recording measures within the
codec's tolerance; record with `--record-codec png` to keep every bit.

## Example

```rust
use tuile_record::{flag, Bounds, Row};

// Two pictures of the same sky, the second a step brighter: a flicker.
let dim = Row { pictured: true, sky_luma: 0.20, sky_pixels: 50_000, ..Row::default() };
let bright = Row { sky_luma: 0.30, frame: 1, ..dim.clone() };
let flagged = flag(&[dim, bright], &Bounds::default());
assert_eq!(flagged.len(), 1);
assert_eq!(flagged[0].row, 1);
```

## Where it sits

Depends on `tuile-tape` (the format), `tuile-film-native` (the encoders),
`tuile-core`, `tuile-camera` and `tuile-atmosphere`. A tool: nothing in the
workspace depends on it, and it never reaches a wasm build.

## Command-line tools

`tuile-record`: `export`, `diagnose`, `goto`, as above.

## Features

None.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
