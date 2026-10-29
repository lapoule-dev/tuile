<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-atmosphere

The air around the planet: where the sun is, what the air between the eye and
the ground does to the ground's colour, and what the shell of air seen
against space looks like at a given hour.

## Why this is its own crate

It is not part of the tile engine. `tuile-core` streams and decodes geometry;
nothing about scattering coefficients or solar ephemerides belongs to that
job, and putting them there would mean every consumer of the core — a server,
a WASM worker, a USD exporter — carried an atmosphere model it never asks a
question of.

Nor is it part of any renderer. The physics is the same for wgpu, for a
WebGL facade and for a Hydra delegate; only the *pass* differs. Two backends
quietly disagreeing about the colour of air is exactly the kind of bug that
surfaces as "the wgpu one looks different" and takes a day to place.

So: renderer-agnostic, engine-agnostic, and consumed by both. It computes
parameters; a backend draws with them.

## The pieces

- [`clock`] — the scene's instant from the wall's: a start, an offset
  and a rate ([`SceneClock`]).
- [`sun`] — where the sun is at a stated instant. Everything else is
  downstream of it, which is why a globe lit "at 18:00 UTC" needs this rather
  than a direction someone picked by eye.
- [`aerial`] — the air between the eye and the ground. A correction applied to
  a surface's colour, and the strongest depth cue a globe has.
- [`illumination`] — what one moment's sun and sky give a scene, and how a
  camera meets it: the per-frame contract a renderer is lit by.
- [`hillaire`] — the physical atmosphere (Hillaire 2020): transmittance
  and multiple-scattering tables, the sky and the air between eye and
  ground marched over them, and a moment's [`ReferenceIllumination`]
  worked out from them. [`params`] says what the air is made of and
  which sphere it is laid on.
- [`stars`] — the stars of the Bright Star Catalogue: each one's light
  from its magnitude, its colour from its B−V, and the binned table
  (`stars.table`, made by the `tuile-star-catalogue` tool) that the sky
  reads them from. They are always drawn: the air dims them and the
  camera decides whether they show.
- [`planets`] — Mercury to Uranus: VSOP87 places, Mallama & Hilton's
  magnitudes, drawn by the stars' rule.
- [`tuning`] — every setting of the air as a tuning value: read from the
  environment by a native host, today's constant by default.
- [`look`] — how a renderer draws the physical atmosphere: its
  calibration, exposure, grade, discs and march ([`SkyLook`]).
- `gpu` (feature `wgpu`) — the tables, the sky-view and the uniform on
  a GPU, and the WGSL that draws the sky and the lit ground with them:
  the one implementation a film's renderer and the viewer's share.
- [`sky`] — the air with nothing behind it but space. A colour of its own,
  drawn as a shell, turning over from blue through orange to dark with the
  hour.
- [`scenes`] — the sun's and the Moon's discs' acceptance scenes: places,
  aims and instants worked out from the ephemeris, one description for a
  film's tape and a viewer's script.

## Not here yet

**Clouds.** Deliberately absent rather than stubbed. A convincing cloud layer
is a volumetric problem — a density field, a march through it, and a lighting
model that is not the one above — and it shares almost nothing with the
closed-form work here beyond the sun direction. It wants its own module and
its own pass, and shipping a flat billboard in the meantime would be worse
than shipping nothing, because it would look answered.

## Precision

Positions crossing to a shader are in **render space** — relative to the
render origin the tiles were rebased onto — and only ever narrow to f32 after
the subtraction. This is the same anti-jitter protocol the geometry uses, and
for the same reason: an ECEF position in f32 is good to a few metres, and a
haze that moves by metres between frames reads as a shimmer across the whole
scene.

## Where it sits

Depends on `tuile-core` (geodesy). Used by every renderer that lights a
globe — `tuile-wgpu`, `tuile-film`, `tuile-film-gpu`, `tuile-film-native` —
by `tuile-viewer`, and by the `globe-bulk` example.

## Example

```rust
use glam::DVec3;
use tuile_atmosphere::Sun;

// 2026-10-25 12:00 UTC, seen from where the prime meridian meets the equator.
let sun = Sun::at_unix_seconds(1_792_929_600.0);
let elevation = sun.elevation_at(DVec3::X);
assert!(elevation > 0.0, "noon on the equator is daytime");
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/19-sky-atmosphere.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/19-sky-atmosphere.md)
- [`docs/21-sky-sun-moon.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/21-sky-sun-moon.md)
