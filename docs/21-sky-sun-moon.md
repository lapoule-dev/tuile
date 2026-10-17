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
- White balance: 0.35 of the way (a golden hour stays golden) to the metered light's white, blended
  towards a warm white as night falls — the picture goes blue, as the
  dark-adapted eye sees it.
- Night floor: a clear moonless night's starlight and airglow (2·10⁻⁸ of
  daylight, ≈0.002 lx) on all ground. Where the camera is exposed for
  brighter ground in the same frame (a night side beside a day side), no
  ground and no sky — space included — is drawn darker than a sixteenth of
  noon. Never black.

## Discs, glare, veil and towns

- **Discs.** `Look::sun_scale` and `Look::moon_scale` draw the sun's and
  the Moon's discs that many times their size, each keeping its own
  radiance, limb and phase. 1 is the sky as it is (half a degree); the
  time-lapse films use 3 and 5.
- **Glare.** `Look::glare` (1 by default; 0 for none) spreads a disc's
  light by a camera point-spread function: two normalised Lorentzian rings
  (4 % of the light in a 1.5° core, 1 % in an 8° halo) and a faint
  six-pointed star. It is an energy-conserving convolution of a point
  source, computed analytically per pixel. It is hidden where ground
  stands before the disc. The meter reads the air, never the discs: the
  exposure does not crash when the sun is in frame.
- **Veil.** The air's light between eye and ground is bounded softly at six
  times the ground's own light (on ground of a tenth's reflectance). By
  day the bound is seldom reached. At twilight, air still lit by the sky
  high above would otherwise lay one grey over ground lit by little, and
  the relief would go. This is a photographer's choice over physics, and
  it is stated as one.
- **Towns.** With night lights, NASA's Black Marble lights towns as the
  ground's own light falls. It is a layer of the tile store like any
  source's (`asset-1000000201`, geographic, level 3: sixteen by eight tiles
  of 320, about 8 km a texel), declared with its licence and credit. The
  bake writes the tiles any camera of the tape can see through the store's
  writer; the pack names the layer (`night_layer`); the render reads the
  tiles back through the store's reader and puts them together into one
  texture, dark where the store holds none. The film's interactive viewer
  does not draw them yet: that needs an emissive layer in its drape.

## Ground imagery

The day ground is any imagery layer of the store. With an ion token,
`--imagery 2` (the default) drapes Bing Aerial over ion's relief.
`--imagery 1000000101` drapes NASA's Blue Marble: Next Generation (public
domain, about 600 m a pixel) over the same relief: only the imagery
differs. Each is kept in the store as its own layer, with its licence.
`--ground open-earth` takes Blue Marble over the bare ellipsoid and needs no
token.

## Time-lapses

`tuile-film-timelapse <tape> key=value…` writes the cameras of one generic
time-lapse and the frames' instants, as a versioned `/clock` record in the
tape itself. Its parameters are:

- place and height, start and duration, film length and fps;
- the camera's frame (`earth` or `inertial`);
- its motion (`spin=<turns>`, `track=sun-moon` or a held heading);
- pitch and its swing, and the vertical field of view.

`track=sun-moon` turns the camera to the sun by day and through twilight
and to the Moon at night, raised to keep it in frame with ground below.
The tape asks a `Sky` trait for the bodies; the ephemerides answer from
`tuile-film-native`.

`tuile-film-produce` reads the instants from the tape (`--clock` overrides
them). A frame's light comes from one path: the host's
(`Overlays::light`), else the tape's instant, else the look's. A film
under a physical atmosphere with neither instants nor a sun placed by hand
is refused.

`--ground ion|open-earth|empty` chooses the ground:

- `ion`: Cesium World Terrain and Bing Aerial;
- `open-earth`: the ellipsoid draped with NASA's Blue Marble, no token;
- `empty`: no ground, two root tiles of no triangle, baked through the
  same stages, only under `--atmosphere`.

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
