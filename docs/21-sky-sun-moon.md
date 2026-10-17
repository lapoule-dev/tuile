# 21 — Sky, sun and Moon that follow the clock

Status: implemented (`tuile-atmosphere`, `tuile-film-gpu`, `tuile-tape`,
`tuile-film-native`). Builds on the Hillaire 2020 atmosphere of
`docs/19-sky-atmosphere.md`.

## What a frame is lit by

A frame's light is a function of its instant and of where its camera is,
never of the wall clock nor of rendered pixels:

- **Sun** — NREL SPA (`solar-positioning`), `Sun::seen_from`.
- **Moon** — Meeus ch. 47 (ELP-2000/82 truncated, 10″) through the `astro`
  crate, `Moon::seen_from`: topocentric direction (parallax up to 1° comes
  from the geometry), distance, angular radius, phase angle, illuminated
  fraction. Its light outside the air follows Allen's magnitude law
  (`moon_irradiance`: 2.5·10⁻⁶ of the sun at full, ≈0.3 lx).
- **Stars** — procedural (one star a cell of a cube on the celestial
  sphere, magnitudes drawn so that the sky holds about 5 000 stars brighter
  than magnitude 6), turned by Greenwich apparent sidereal time
  (`celestial_to_ecef`). Not a catalogue: no constellation is real.

`ReferenceIllumination` carries `moon: Option<Moonlight>` beside the sun:
`to_moon_ecef`, `phase_angle`, `angular_radius` (what a host stores per
frame) and `moon_rgb` (worked out by the atmosphere). `FrameLight::instant`
states the frame's instant; a renderer given an instant and no Moon places
the Moon from the frame's own eye.

## The atmosphere, per light and per fragment

- Two lights march through one air: the sun and the Moon, each with its own
  sky-view table (per unit of its own light), the Moon's scaled by its
  strength. The moonlit sky is the sunlit sky's shape, dimmed.
- Ground is lit **where it is**: each fragment reads its own sun and Moon
  through the transmittance table and its own sky through a level-ground
  irradiance table (256 × 16, sun zenith cosine × height), the frame's L1
  harmonic scaled by the fragment's level sky against the frame's. A
  terminator a frame shows falls on the ground where it is.
- From above the top of the air (100 km) every march starts where the ray
  enters it; a ray that misses it gathers nothing. The sky-view table is
  used up to the top of the air; above it the sky is marched per pixel —
  the limb is a band a few degrees thick that no table row can hold.
- The air between eye and ground takes 24 steps bunched towards the ground
  once the eye is over 10 km: the dense air is at the end of a long path.
- All atmosphere tables are 32-bit floats read by a hand-written bilinear:
  a night is ten orders of magnitude under a day, below what half floats
  hold. Lights are pre-exposed into the HDR target for the same reason.

## Exposure, white balance and night

- A camera meters a 24 × 14 grid of rays across its view: ground points by
  the light on them seen through the air, sky by the light that would make
  middle-grey ground as bright; space past the air is not metered.
- `shown_ev`: one smooth curve from noon to the darkest night. Brighter
  than daylight: three quarters made up. Dimmer: `−3·log₂(1 + |ev|/17.3)` —
  sunset about 1.7 stops under noon, civil dusk 2.7, full Moon 3.1, a
  moonless night 4. No cap, no step, nothing to pump about.
- White balance: 0.6 of the way to the metered light's white, blended
  towards a warm white as night falls — the picture goes blue, as the
  dark-adapted eye sees it.
- Night floor: a clear moonless night's starlight and airglow (2·10⁻⁸ of
  daylight, ≈0.002 lx) on all ground. Where the camera is exposed for
  brighter ground in the same frame (a night side beside a day side), no
  ground and no sky — space included — is drawn darker than a sixteenth of
  noon. Never black.

## Time-lapses

`tuile-tape timelapse <tape> key=value…` writes the cameras of one generic
time-lapse and its clock (`<tape>.clock`): place, height, start, duration,
film length, fps, camera frame (`earth` or `inertial`), motion (`spin`
turns or held heading), pitch and its swing, field of view.
`tuile-film-produce --atmosphere --clock <start>,<seconds a frame>` lights
each frame by its instant. `--sky-only` bakes packs of cameras with no
ground — the sky over the atmosphere's own smooth planet — without any
tile source: what a sky is checked by.

A camera held to the stars (`frame=inertial`) sees the Earth turn beneath
it, but the sun does not move in its sky: the terminator stays put in the
picture and nothing rises or sets but the Moon. Sunrises and sunsets over
the limb want a camera held to the Earth.

## Not done

- Refraction (the sun and Moon are geometric).
- Lunar libration and the Moon's albedo map: the disc is uniform
  Lommel-Seeliger, with earthshine on its night side.
- A real star catalogue; planets.
- The aerial perspective's shadowing by terrain (no crepuscular rays).
