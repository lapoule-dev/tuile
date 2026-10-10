<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Seams, poles, islands, colour — an exploration

Four families of defect in what the globe draws, looked into together because
they touch one another. For each: what is drawn today, how large the defect
is, where in the code it comes from, what could be done and at what cost, and
what was **not** established.

Written on `explore/seams-poles-islands-colour` (from `main` at `fd4caa5`;
file and line numbers are that commit's). The numbers are in the executed
notebook beside this file, `seams-poles-islands-colour.ipynb`, and its `data/`.
Nothing here is a fix, with one exception decided while this was being
written: the seams are fixed on `fix/terrain-seams` (section 1.5). Everything
else is a finding and a proposal.

**How things were measured.** No tile source was asked for anything. Three
kinds of evidence, named each time:

- *measured*: a number out of a run of this branch's probes — scenes on
  made-up tiles drawn by the film's own code
  (`crates/tuile-film-native/tests/exploration.rs`), or the tables of the two
  packs of the Paris orbit read with no tile behind them
  (`crates/tuile-film/examples/seam_survey.rs`);
- *computed*: the code's own formula evaluated, quoted with its place;
- *read*: what the code says, not run.

**What could not be run.** No frame of a real film. `tuile-film-render` reads
the packs and the tile store from two buckets named by `TUILE_STORE_BUCKET`
and `TUILE_TILES_BUCKET`; the environment file this work was given holds the
endpoint and the keys and neither name, and they were not found anywhere they
may be looked for. So frame 4500 of the 577d film was not rendered again —
neither with the probes, nor after the fix — and the Paris packs, which hold
references into a store served by a host, could be read (their tables) but
not drawn. Every place this bites is marked **not established**, with the
command that settles it.

The probes, all marked `PROBE` in the code and none a feature:

| probe | what it is |
|---|---|
| `tuile-film-render … --seams <dir>` | `SeamMeter`, an `Observer` of `tuile-film-native`: for every pair of drawn tiles that share a stretch of edge, the step between their surfaces, what of it no skirt closes, and that opening in pixels from the frame's camera; `seams.csv` and a marked picture a frame |
| `TUILE_PROBE_HOLES=1` | the resolve paints magenta every pixel whose ground is further than an eye can see — what a gap shows — so the picture counts its own holes |
| `TUILE_PROBE_UPSAMPLED_SKIRTS`, `TUILE_PROBE_SKIRT_OF_SOURCE`, `TUILE_PROBE_SKIRT_SCALE` | switches on how skirts are hung, to measure what each closes |
| `seam_survey` (example of `tuile-film`) | a pack's shared edges from its table alone |
| `tests/exploration.rs` | four scenes: a seam, the film's base colour, the drape of a polar tile, the mesh and the maths at the pole |

## 1. Seams

### 1.1 What is drawn today

The film shows thin dark slivers along straight lines of the tile grid: about
300 pixels of frame 4500 of the 577d film at 1080p
(`videos/577d-f4500-overlay-ribbon-terrain.png`, near the ridges top left,
bottom right, bottom centre left). They are holes. Nothing closes the step
between two tiles there, and since a film draws tiles all the way round the
Earth with no face culled (`tuile-film-gpu/src/lib.rs`, `cull_mode: None`),
what shows through is the inside of the planet's far side: deep sea seen from
behind, sRGB (0, 8, 21).

The interactive viewer has the same holes in its geometry — it builds its
tiles with the same two functions — and shows them differently: it draws a
whole-planet shell behind everything (`tuile_terrain::globe_shell`,
`examples/wgpu-viewer/src/backdrop.rs:43`), so a gap there is a sliver of the
shell's colour, not of the far side.

### 1.2 The cause

Three facts, each in the code:

1. **A tile cut from an ancestor's terrain is given no skirt.**
   `tuile_terrain::upsample` returns its mesh with the four edge lists empty
   (`tuile-terrain/src/upsample.rs:233`, under the comment "Skirts are not
   drawn on this globe, and an upsampled tile shares its edges exactly with
   the ancestor it came from anyway"), and `append_skirts` skips an edge of
   fewer than two vertices (`tuile-terrain/src/mesh.rs:210`). The first half
   of the comment has not been true since skirts were switched on; the second
   is true only against tiles of the same ancestor.
2. **Nearly every tile near a camera is such a tile.** The tree refines past
   the terrain a source has, so that imagery can go on sharpening
   (`upsample.rs:8`). *Measured* on the Paris packs: no surface is from
   terrain finer than level 13, and every tile of levels 14 to 19 is cut —
   460 of the 683 tiles of a frame at a screen-space error of 2, 49 of 92 at
   16.
3. **A skirt is sized by the tile's own level, not by the level its surface is
   from** (`skirt_height(&rect)`, `mesh.rs:132`, called with the drawn tile's
   rectangle at `tuile-planetary/src/lib.rs:1263` and `:1445` and
   `tuile-film/src/from_store.rs:117`; the pack records the same,
   `tuile-planetary/src/provenance.rs:72`, with a test that holds it:
   "the skirt is the drawn tile's, not its ancestor's"). Fact 1 hides this
   one: a cut tile has no skirt to be too short.

So wherever two **different** terrain tiles meet and the higher side is drawn
by tiles cut from its terrain, the step between the two surfaces is open.
Those are exactly the straight lines of the source's grid at its deepest
level, which is what the pictures show.

*Measured* (`tests/exploration.rs`, scene `seams_where_tiles_are_cut_from_an_ancestor`;
pictures `videos/explore-seam-synthetic-*.png`): two level-13 terrain tiles
over one relief, measured on grids of 32 and 20 spacings, part along their
shared meridian by 1.5 m on average and 5.6 m at most. Drawn through the
film's path at 1920×1080, four samples a pixel:

| what is drawn | eye west of the line | eye east |
|---|---|---|
| one terrain tile as tiles of levels 14, 15, 16 | 0 pixels of sky | — |
| the same, its neighbour drawn as itself (48 m skirt) | 0 | 361 (509 touched) |
| the same, its neighbour drawn as its children, cut | 241 (378) | 361 (509) |
| any of them with edge lists on cut tiles | 0 | 0 |

A film's defect, at its size, from steps a real source certainly has. The
second row is why a skirted neighbour does not save a cut tile: a skirt closes
the steps where its own tile is the higher side, and a step is looked into
from the lower side.

Two things found on the way:

- **Tiles cut from one terrain tile do not state exactly one line.** A vertex
  made by cutting is given its height over the ellipsoid (the cut interpolates
  in the tile's coordinates and height, `upsample.rs:55`), so it stands above
  the straight edge it was cut on by the curve of the Earth between that
  edge's ends: `L² / 8R`, a tenth of a millimetre on level-13 terrain, 3.4 km
  on a level-4 tile of three spacings. Invisible where films fly; open on a
  coarse globe.
- **A cut tile keeps its ancestor's centre as the origin its vertices are
  narrowed around** (`upsample.rs:213`, "at worst a few tiles away"). Cut from
  level-2 terrain, a tile can be 3 500 km from it, where an `f32` holds a
  quarter of a metre. *Computed*; not seen, since no terrain in the packs read
  is that coarse under a camera.

### 1.3 Other seams looked for

- **T-junctions between same-level neighbours of a real source**: not
  measured — needs real tiles. Both sides are skirted there, so a step is open
  only beyond the higher side's skirt (12 m at level 15).
- **The antimeridian**: handled, and held by a test
  (`mesh.rs`, `the_easternmost_tile_is_not_torn_by_the_branch_cut`); the seam
  meter wraps the grid there. Not rendered.
- **Imagery seams** (*read*): an imagery tile never reads its neighbour. A
  layer covers a hard rectangle of the terrain tile (`raster.rs:556`), is
  sampled edge to edge with no half-texel inset — deliberately,
  `raster.rs:957` — clamped to its last texel (`compose.wgsl:160`); each
  drape gets its own mip chain, clamped at its border
  (`tuile-film-gpu/src/shaders/mips.wgsl`). So nothing bleeds across a tile
  edge, and any difference of tone between two imagery tiles, or between two
  terrain tiles draped at two imagery levels, is a sharp step along the grid.
  That is colour, not geometry: section 4.
- **The straight break across the Paris film at a screen-space error of 16**
  (`videos/README.md`): the survey says what it is made of — neighbours up to
  three levels apart, and with them their imagery levels. At an error of 2 no
  two neighbours are more than one level apart, and the break is gone.

### 1.4 The light line seen with haze (frame 4500, bottom left)

`videos/explore-577d-f4500-seam-light-line-haze-over-default.png`. *Measured*
on the pictures already in `videos/`: 172 pixels on a straight line along the
boundary between the tiles of levels 15 and 16.

- It is there **without haze**, at the same value (80, 92, 76 against 81, 93,
  78 with): the haze did not make it. The haze adds 1.3 to it where it adds 5
  to the ground around, which is the resolve leaving alone a pixel it cannot
  place.
- Under the default sun (on the Earth's axis) the same pixels are as dark as
  the ground around them (50, 62, 54 against 51, 69, 61). Under the low sun of
  those pictures (250°, 12°) they are 0.8 stop lighter.

So it is something on that seam that a low sun from the west-south-west
lights and a high one does not: a near-vertical face, shaded by its own face
normal — the resolve takes the triangle's normal when a mesh has none
(`resolve.wgsl`, `normalize(cross(e1, e2))`), and a skirt's wall has the
normal of a wall. **Not established** which face: a skirt of a tile that has
one (then that seam has terrain of its own level on one side), or the far
side through a gap where it is light. One frame says:

```bash
TUILE_PROBE_HOLES=1 tuile-film-render 1557732/20260924T221734Z-577d/packs \
  --frames 4500:4500 --no-tone --sun 250,12 --pictures <dir> --seams <dir>
```

magenta on the line means a gap; a row of `seams.csv` with a skirted side and
`open_px_facing` of 0 means a wall. Either way the line outlives the fix of
the gaps unless walls are shaded as the ground they hang from: see 1.6.

### 1.5 The fix: `fix/terrain-seams`

Asked for while this was being written, and made at the source of the
geometry, so that every path inherits it and no renderer has a rule of its
own. On that branch:

- `tuile_terrain::upsample` lists the four edges of the tile it cuts, and the
  mesh counts how many levels below its source it lies (`QuantizedMesh::cut`);
- `tuile_terrain::skirt_depth` sizes a skirt by the level the surface is from,
  and as deep as the rule gives a tile four levels coarser — eighty geometric
  errors where the rule gives five — which covers a neighbour up to six levels
  coarser (a coast, an island); never deeper than a level-6 tile's 6 km;
- `tuile_terrain::to_ground` is the one way a terrain tile becomes drawable
  ground: the loader, its stand-ins (`tuile-planetary`) and a film built again
  from the store (`tuile-film`) all call it;
- `tuile_core::seam` casts rays through every step between two tiles'
  content and says what passed: the judge of the tests, with no picture.

The per-path status, the proof and what was not proven are in that branch's
pull request. In short: a pack of references needs **no re-bake** (its meshes
are built at render time); a pack that carries its meshes keeps the holes it
was baked with.

### 1.6 What the fix does not do

- **A wall that shows is lit as a wall.** Where a step is now closed, the
  wall across it has the texture of the edge it hangs from and, on a mesh
  with no normals, a wall's normal: under a low sun it can be lighter or
  darker than the ground. Giving skirt vertices the surface's normal needs
  the surface to have one; computing normals for meshes that carry none
  changes the shading of every pixel, so it is a decision about the picture,
  not a seam fix. Cost: small (one pass over the mesh at build). It also
  decides whether walls take part in shadows as walls.
- **The ancestor's centre as origin** (1.2): untouched.

## 2. Poles

### 2.1 What is drawn today above 85° and at the pole

*Measured* (scene `imagery_over_the_polar_cap`, picture
`videos/explore-pole-drape-truth-level4-level7.png`) and *computed*:

- **Not black, not bare, not the marker green**: ground, in the wrong place.
  Web-mercator imagery ends at 85.0511° (`raster.rs:112`). The loader says the
  top row of every imagery level reaches the pole (`to_the_pole`,
  `tuile-planetary/src/lib.rs:275`, used at `:1029` and `:1083`), and the
  layer is then laid **linearly** over that taller rectangle
  (`ImageryLayer::substituted`, `raster.rs:947`: `scale = th / sh`). The
  comment above `to_the_pole` says a clamping sampler supplies the rest; the
  code stretches instead.
- The stretch grows with the imagery level, because the top row gets thinner:
  3.1 times at level 4 (82.68° to 85.05° pulled over 82.68° to 90°), 20.9 at
  level 7, 82 at level 9. The ground of 85.00° is drawn at 89.84° with level
  4 imagery and 88.93° with level 7: moved north by 538 and 437 km. Two
  neighbouring tiles draped at two imagery levels do not show the same ground
  along their shared edge.
- Inside the top row, south of 85°, everything is displaced too: the stretch
  starts at the row's south edge, not at 85°.
- **At the pole itself** every tile samples the top texel row of its imagery:
  a different colour by longitude, meeting at a point.
- Fragments no layer reaches get `UNCOVERED_GROUND` (0.16, 0.20, 0.24)
  (`lib.rs:194`); marker green only for a tile with no layer at all
  (`lib.rs:187`).
- The imagery level is chosen with the cosine of the tile's latitude and no
  floor at 85° (`raster.rs:385`, *read*), so levels fall towards the scheme's
  minimum near the pole — which is what keeps the stretch at the small end of
  the table in practice.

### 2.2 The mesh

*Measured* (scene `the_mesh_and_the_maths_at_the_pole`) on tile 3/8/7, an 8×8
grid:

- the north row collapses to a point: 8 of 128 surface triangles and 8 of 64
  skirt triangles have no area (`to_decoded` has no case for the pole);
- the north skirt is leaned a hair north of 90° (`mesh.rs:187`); 
  `geodetic_to_ecef` does not clamp, so the vertex of the meridian 0° comes
  out at longitude −180°: the wall wraps over the pole onto the opposite
  meridian. No NaN; a small inverted cone under the pole;
- its skirt is 48.9 km deep, the equator's, on a tile 959 km wide at its
  south edge and nothing at its north: `skirt_height` reads the rectangle's
  width in radians, with no cosine. Harmless — deeper than needed.

### 2.3 Selection, culling (*read*)

- Geometric error is a function of level alone (`tree.rs:258`), so polar
  tiles, far narrower in metres, are refined `1 / cos(lat)` too far east to
  west: more tiles, not a wrong picture.
- Bounding volumes are a tangent-plane box at the tile's mid-latitude
  (`geo.rs:124`): not degenerate at the pole. Horizon culling is against a
  sphere of the polar radius (`tree.rs:160`); the header's own occlusion point
  is decoded and never used.
- The two root tiles and the antimeridian: nothing special found.

### 2.4 Camera, sun, air

- **`ecef_to_geodetic` on the axis itself is wrong.** *Measured*: 1 000 m over
  the pole it returns latitude 135° and a height of 2 645 km; a millimetre off
  the axis it is right (`geo.rs:58`: with `rho = 0` the iteration divides by
  it). It feeds the film camera's near plane (`tuile-film/src/camera.rs:45`),
  the haze's local frame (`look.rs:83`), `--sun`'s frame, the bake's texel
  spacing (`session.rs:1135`).
- **A tape over the pole is degenerate** (*read*): `tuile-tape orbit` builds
  east as `cross(Z, zenith)` with a guard that turns zero into zero
  (`orbit.rs:30`, `:82`) — every frame of the orbit sits at the same point
  with `up` parallel to where it looks; `zoom` hands a zero `up`
  (`zoom.rs:84`). Neither `ViewState::perspective` (`traversal.rs:109`) nor
  `FrameCamera::of` guards it: a basis of NaN, and an empty frame.
  `enu_frame` itself is built from sines and cosines and is sound at the pole
  (`geo.rs:87`).
- **The sun** is on the Earth's axis by default (`Look::default`,
  `look.rs:162`): at the north pole a sun at the zenith, at the south pole no
  sun at all, the dome alone. `--sun` fixes one direction for a whole film
  from its first frame's eye. The shadow map's basis cannot degenerate
  (`frame.rs:82`).

### 2.5 Options, smallest correct first

1. **Clamp instead of stretch**: lay the top row's texture over its own
   rectangle and let its last texel row stand for everything north of it
   (coverage to the pole, scale and translation from the true rectangle).
   Cost: a few lines where a layer is placed, and the same in the pack's
   placements — which are baked, so **films over 85° need a re-bake** to show
   it. Picture: ground in its place up to 85.05°, then streaks along the
   meridians to the pole. Risk: none south of 85°.
2. **Drape the cap from a geographic source**, where one is configured: the
   only way to show the ground that is there. Cost: a second imagery scheme in
   a drape (the placements already allow it); a source to pay for.
3. **Guard the axis** in `ecef_to_geodetic` and in the two tapes: a few lines
   and a test each. No picture changes off the axis.
4. Leave the collapsed triangles and the wrapped skirt: nothing shows.

**Not established**: what a real film looks like over a pole — no pack goes
there. It needs a bake (section 5).

## 3. Islands and open sea

Almost all of this is *read*; no coast is in the material at hand.

### 3.1 What is drawn where a source has nothing

- **Terrain.** A tile the source does not have is cut from the nearest
  ancestor it has (`tuile-planetary/src/lib.rs:874`); the tree never asks
  whether a child exists (`tree.rs:189`). Never a plane at height zero, never
  a hole by this path. **Any** failure of a fetch above level 0, not only an
  absence, silently becomes a cut (`lib.rs:854`): a bake on a bad network
  bakes coarser ground and says nothing. A tile server records an absence as
  a one-byte marker and serves it back for seven days
  (`tuile-tile-server/src/service.rs:478`, `tuile-core/src/storage.rs:66`).
- **Sea level.** Heights are over the ellipsoid; the sea's surface is the
  geoid; nothing in the workspace knows the difference (no geoid anywhere).
  The sea is drawn where the source's tile puts it.
- **Imagery.** A missing tile above the scheme's minimum is replaced by its
  parent, masked to the child's share (`lib.rs:724`); the level-4 floor is
  always under (`lib.rs:56`). An opaque black "no data" tile is counted,
  logged and **still draped** (`lib.rs:702`): a black square over the floor
  is possible there, and against the rule.
- **Missing siblings.** Refinement holds the parent until its descendants
  cover the ground (`traversal.rs:1088`); a bake forbids holes and stand-ins
  and fails the frame on a tile without content (`session.rs:954`,
  `:1189`). No place was found where a child without its siblings leaves a
  hole.

### 3.2 Coasts: level jumps

No rule keeps two neighbours within a level of each other
(`traversal.rs`: no neighbour constraint; the selection is bounded by
screen-space error alone, softened in a bake by `uniform_detail`).
*Measured* inland, on the Paris packs: at most one level at a screen-space
error of 2, up to three at 16. **Not established** at a coast, where a
detailed shore meets a sea the source holds coarsely.

What a jump costs, *computed* (notebook, section 3): the edge of a coarse
tile between two vertices sags under the ellipsoid by `L² / 8R`; the fine
neighbour stands on it; the fine tile is the higher side and its skirt is
what has to close the sag. With one segment an edge a level-8 sea tile sags
120 m; with sixteen, half a metre. The skirts of `fix/terrain-seams` reach
1.5 km at level 12 and close either. **Not established**: how many vertices a
source's sea tiles have along an edge, which decided whether coasts were open
before the fix.

### 3.3 Tiny islands

An island smaller than a coarse tile is in that tile's mesh or it is not;
where the source has finer terrain over it and not over the sea around, the
island's tiles are fine and skirted and the sea's are cut from a coarse
ancestor: the seam of section 1 all round the island, at whatever jump the
selection makes. After the fix that is a wall of the island's shore colour
where the two surfaces disagree, instead of a hole.

### 3.4 Options

1. Section 1's fix — done.
2. **Tell a failure from an absence when a bake cuts a tile** (3.1): fail the
   frame on a failure, as a bake does for everything else. A few lines.
3. **Do not drape a "no data" tile**: let the parent stand, as for a missing
   one. A few lines; changes pictures only where such tiles are.
4. A geoid for the sea: a model and a decision, not a fix.

## 4. Colour

Not decided here, by standing instruction: measured, shown, proposed. No
calibration was run — it reads the tile store — so no checkerboard and no
triptych were produced; the first act of any follow-up is to run
`--calibrate` and open both, coarse level and fine.

### 4.1 Where things stand (*read*, with the history in `videos/README.md`)

- **What a plain render applies.** `Tone::OfTheFilm`: the grade kept beside
  each pack, `<pack>.tone.json`, in the packs' bucket
  (`tuile-repository/src/tone.rs:70`); none, no correction. Its exposure,
  contrast and saturation are folded into the look; per tile, a matrix field
  wins over a corner field, applied in `compose.wgsl`.
- **What `--calibrate` fits by default**: a corner field of curves
  (`tuile-radiometry/src/corners.rs`), reference level 12. With
  `--reference-layer`, a line a tile (`linear.rs`) or a 3×4 matrix a corner on
  a level-14 lattice (`matrix.rs`), taken at a dose (0.3, "chosen by eye on a
  first film", `matrix.rs:103`).
- **Dead but still wired**: the grade a level. The renderer passes an
  identity grade (`render.rs`, `LayerGrade::IDENTITY`); `--meter` still
  writes `<layer>/tone/…`, read only by the web page's fallback; and
  `--calibrate` still requires `<layer>/tone.json` to exist though nothing
  reads it.
- **Numeric crates**: `nalgebra` and `nalgebra-sparse` in `linear.rs` and
  `matrix.rs`, `petgraph`'s union-find in `matrix.rs`; the corner field's
  Gauss–Seidel, the grade's Levenberg–Marquardt and the block fusion's
  union-find are still written by hand.
- **Last measurements** (README): a gain a level, top third against bottom
  third, worst frame 0.72 → 0.30 stop; a grade a level 1.55 → 0.61; against a
  reference layer, a capture boundary through the middle of a tile "smoothed,
  not removed", patches in the sea, steps along the shore, sea too green by
  zones; a matrix a tile smeared fields. No residual by level is recorded for
  the matrix film.

### 4.2 A finding: the film multiplies every tile by a slate

*Measured* (scene `the_film_multiplies_a_drape_by_the_tiles_base_colour`; the
packs' factors by `seam_survey`).

The loader sets a tile's base colour to `UNCOVERED_GROUND`, (0.16, 0.20,
0.24): what shows where no imagery layer reaches
(`tuile-planetary/src/lib.rs:1186`). The interactive renderer blends its
layers **over** it. The bake keeps it in the pack
(`tuile-bake/src/bake.rs:383`; both Paris packs carry it on every tile, 1 313
and 182 read), and the film's resolve **multiplies** the composed drape by it
(`resolve.wgsl`: `var base = tile.factor.rgb; … base *= texture`).

On the GPU, a grey drape comes out 2.63 / 2.31 / 2.06 stops darker (R, G, B)
than under a white factor. So every film is 2.3 stops down — which the look's
exposure, set on films rendered this way, carries — and **tinted: red 0.32
stop under green, blue 0.26 over** — which nothing carries. It is the same
for every tile: no step between tiles comes from it. It shifts every
comparison with a reference layer by a constant, and a calibration taken at a
dose of 0.3 undoes three tenths of it.

Options: (a) the film uses the factor only where no layer reaches, as the
viewer does — one line in the resolve, the exposure re-aimed by 2.3 stops,
every film's white balance moves; (b) the bake writes white once a drape
covers the tile — new bakes only; (c) leave it and let the calibration absorb
it. The judge of any of them is the checkerboard and the triptych against the
reference layer, before and after. **To be decided by the owner.**

### 4.3 Where seams, poles and islands meet the calibration (*read*)

- **Edges two tiles share.** In the corner field a corner belongs to its
  tile; corners are merged across every same-level edge that is not a seam
  (`corners.rs:445`), so continuity there is exact by construction. Across
  **levels** there is no constraint at all (`corners.rs:335` builds edges at
  the same level only): a level-L tile beside or under a level-L+1 tile is
  solved apart. The matrix field ties levels finer than 14 to their level-14
  ancestor (`matrix.rs:271`) and nothing coarser. So the tone step along a
  boundary between two imagery levels — the very lines where the geometry had
  its gaps, and where a film's coarse and fine ground meet — is the one the
  calibration does not constrain.
- **Walls.** A skirt's wall takes the texel column of the edge it hangs from.
  Once seams are closed, a wall that shows carries its tile's corrected
  colour: if the two tiles' corrections disagree along that edge the wall
  shows the step in a few pixels of height. The edge constraint and the walls
  are the same line; fixing the first makes the second invisible.
- **Missing neighbours.** A tile whose neighbour is not in the film has free
  corners on that side, held only by smoothness and a weak pull to nothing
  (`corners.rs:490`): the border of a film's footprint drifts.
- **Water.** Zones exist only with the linear measure; a tile is water if most
  of its places are (`linear.rs:140`), so a shore tile gives its strip of sea
  the ground's correction — the green sea of the README — and every shore
  edge is forced to be a seam (`corners.rs:404`): a step along every coast by
  construction. The matrix field chooses per place (`matrix.rs:136`).
- **Dark sea.** The meter drops a tile where under half the texels count
  (stored values ≤ 6 or ≥ 250) (`meter.rs:198`): deep sea is never calibrated
  and is drawn untouched beside corrected neighbours. An island in open sea is
  a corrected patch in uncorrected water.
- **Tiles carried by a pack** (not built from the store) are never graded
  (`render.rs`, the pack's PNG is uploaded as it is).
- **The pole's stretched texels**: a tile whose drape is one row of texels
  pulled over five degrees has no statistics worth fitting; the measure does
  not know it. Nothing excludes it.
- **Shadows and haze** come after the drape and do not enter the measure; the
  meter reads imagery as stored. A film calibrated without them is not
  re-aimed when they are switched on (the haze "set by eye on one film",
  `look.rs:39`).

### 4.4 Pending, in the order they would be proposed

1. The slate (4.2): decide.
2. A constraint across levels in the corner field — the coarse tile's value at
   the fine tile's corner — at least from the reference level down. A term in
   the solve; judged on the checkerboard at a level boundary.
3. Zones by place, not by tile, for the corner field; shore edges no longer
   forced seams once they are.
4. Sea too dark to measure: take the nearest measured neighbour's correction,
   or the zone's.
5. Grade tiles a pack carries.
6. Remove the per-level machinery, and the `tone.json` gate with it.
7. Hand-written solvers onto `nalgebra-sparse` / `petgraph`, with the oracle
   kept.
8. The earlier design note (`docs/18-color-harmonization.md`) describes a path
   that was not built; what still applies is its statement of the problem and
   its risks (halos, seasons and water, the quality of the anchor).

## 5. What was not established, and what it would take

| question | what it takes |
|---|---|
| Frame 4500 of the 577d film: the gaps attributed tile by tile, and counted at 0 after the fix | the two bucket names in the environment; then one command (1.4) on each branch. No source asked |
| The light line: a wall or a gap | the same run |
| Whether real same-level neighbours part by more than their skirts | `--seams` over any film's frames: `step_max_m` against the levels |
| How many vertices a source's sea tiles have along an edge | reading twenty sea tiles of levels 6 to 10 from the store |
| A coast, an island | **a bake** — below |
| A pole | **a bake** — below |
| Any calibration number | `--calibrate` against the reference layer, with the checkerboard and the triptych opened, coarse and fine |

**Bakes proposed — none was run.** Both fetch new source tiles, which cost
quota; existing material cannot answer, because no pack goes to a coast, an
island or beyond 85°.

- *An island*: an orbit of 8 frames, 1280×720, screen-space error 8, 4 km out
  and 2 km up, round a small island with open sea all round and detailed
  terrain (for instance Ouessant, 5.10° W 48.46° N). Estimated from the Paris
  bake at an error of 16 (188 terrain tiles and 6 542 imagery tiles for 48
  frames): about 100 terrain tiles and 2 000 to 3 000 imagery tiles. It
  answers: the jump at the shore, the sea's own tiles, what the walls look
  like there, imagery over water, absences.
- *A pole*: 4 frames, 1280×720, screen-space error 16, a camera 300 km over
  88° N looking at the pole, off the axis. About 60 terrain tiles and under
  500 imagery tiles (coarse levels only, the cosine sees to that). It
  answers: what a film shows above 85°, and whether anything in the bake's
  path fails there.

## 6. A proposed order of work

1. **Seams** — done on `fix/terrain-seams`; confirm on frame 4500 as soon as
   the store can be read. Everything below is easier to look at without
   holes.
2. **The slate** (4.2) — a decision, then a line. Before any further
   calibration: every measurement against a reference is taken through it.
3. **The constraint across levels** (4.4, item 2) — the tone step is on the
   same lines the walls now stand on; done second, the walls go unseen.
4. **Walls shaded as the ground they hang from** (1.6) — after shadows are
   looked at on steep ground, since it decides what a wall does in the sun's
   map.
5. **Poles**: guard the axis now (no picture changes); clamp instead of
   stretch with the polar bake to look at.
6. **Islands**: the island bake, then water and dark sea in the calibration
   (4.4, items 3 and 4) with it as the test film; a failure told from an
   absence in a bake.

What interacts: the skirts and the edge constraint are the same lines (1 with
3); walls and shadows (4, and the low-sun line of 1.4); the slate and every
calibration number (2 before 3 and 6); the pole's stretch and the pack's
placements (a re-bake, where the seams needed none).
