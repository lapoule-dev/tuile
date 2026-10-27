<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-tape

Record a camera path, then fly it again exactly.

Some defects only appear after a particular sequence of gestures. The one
that produced black ground in the globe viewer took a zoom in, a zoom out, a
zoom in, a rotation, a tilt, more rotation and a zoom out, in that order,
over one specific lake. Reproducing that by hand costs a minute and lands
somewhere slightly different every time, which is no basis for deciding
whether a change helped.

So the camera is recorded, not the gestures: one row per frame holding the
eye, where it looks, which way is up, and the field of view. Those seven
numbers are the complete determinant of a frame, so replaying them drives
the render and the traversal from what the session actually used, with no
interpretation in between.

# Why MCAP

Because a session is not only a camera path. What has to be captured to
diagnose a render is the pose **and the picture it produced**, on one
timeline — two files means two clocks and no way to say which frame went
with which pose. MCAP is a container for exactly that: several channels, one
time base, and it streams, so a reader can open a file whose writer died.

It cost something to move here from Parquet, and the thing it cost is worth
naming: an `f64` written to Parquet comes back bit-identical, and a globe at
a metre of altitude does not survive rounding — shave the last digits off an
ECEF position and a replay is flying a different path while claiming to
reproduce one. So the camera is written with Rust's shortest round-trip
formatting, which is exact for `f64`: every value read back is the same
bits. That is a property this crate must keep, and a test holds it.

# Rendering-agnostic

This crate knows nothing about any camera type or any renderer: [`Frame`] is
seven numbers. A host converts its own camera in and out, which is a line
each way, and keeps this out of its dependency graph.

```no_run
use tuile_tape::{Frame, Tape};

// Record.
let mut tape = Tape::recording("path.mcap")?;
tape.push(Frame { position: [0.0; 3], direction: [0.0, 0.0, -1.0], up: [0.0, 1.0, 0.0], fovy: 1.0 });
tape.finish()?;                     // flushes and closes

// Replay.
let mut tape = Tape::replaying("path.mcap")?;
while let Some(frame) = tape.next_frame() {
    // drive the camera from `frame`
}
# Ok::<(), tuile_tape::TapeError>(())
```

# A session, recorded

A tape can carry a whole interactive session ([`session`]): beside each
frame's camera, the wall and scene clocks it was drawn at, its viewport and
render origin (`/viewer/frame`), what the atmosphere lit it with
(`/viewer/sky`), the input before it (`/viewer/input`) — and the picture the
viewer drew, as it was on the screen (`/frame`, `foxglove.CompressedImage`,
JPEG or PNG), carrying the index of the frame it shows. The viewer writes
one with `--record <file.mcap>`; `session::read` and
`session::for_each_picture` read it back, every number to the bit. The
camera and the clocks are what a replay needs to put the renderer back at any
frame; the pictures are what was seen. `tuile-record` makes a film, a
Parquet sidecar and a diagnosis of one.

[`scan::picture_stats`] measures a picture against the camera that drew it:
the sky's and the ground's mean luma, the star-like points, the black pixels.

## Where it sits

Depends on `tuile-core` and `tuile-camera`. Used by `tuile-viewer` (record and
replay), `tuile-bake` and `tuile-film-native` (camera paths), `tuile-record`
(a recorded session read), and `tuile-usd` (a tape as a USD stage).

## Command-line tools

Both read a trace written by the viewer.

`tuile-scan-black` measures, for each picture, how much of the ground is bare,
and prints the worst frames with their timestamps.

```bash
cargo run --release -p tuile-tape --bin tuile-scan-black -- trace.mcap
```

`tuile-trace-dump` writes the rendered frames out as PNG files, one every
`every-n` (default 1).

```bash
cargo run -p tuile-tape --bin tuile-trace-dump -- trace.mcap out-prefix [every-n]
```

Neither takes flags: arguments are positional.

## Features

None.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
