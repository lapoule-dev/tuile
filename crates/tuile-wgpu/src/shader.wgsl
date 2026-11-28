// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Minimal PBR-ish tile shader: base color texture × factor, lambert + ambient.

struct ViewUniform {
    view_proj: mat4x4f,
    // xyz = direction the light travels (normalized), w unused.
    sun_dir: vec4f,
    // x = ambient amount, yzw unused.
    params: vec4f,
    // Aerial perspective. Mirrors tuile_atmosphere::AerialPerspective field for
    // field; that crate owns the physics, this only spends it.
    // One nested struct rather than five flat fields, which is what the Rust
    // side has always had: `AerialPerspective` is a struct there too, and the
    // layout is unchanged — five `vec4f` at the same offsets.
    air: Air,
}
@group(0) @binding(0) var<uniform> view: ViewUniform;

// How many rows of strips a tile carries for one state of its stitching, and
// how far into the tile a correction reaches: both set from the Rust side
// (`prepare::STITCH_ROWS`, `tuile_core::stitch::BAND`) when the module is
// built.
const STITCH_ROWS: u32 = /*STITCH_ROWS*/u;
const STITCH_BAND: f32 = /*STITCH_BAND*/;

struct TileUniform {
    model: mat4x4f,
    // x: how far the tile has slid from the strips it had to the strips it
    // has, 0 to 1; y: whether it has any at all.
    stitch: vec4f,
    // `Stitch::packed`, twice: the strips the tile has now, then the ones it
    // had before its neighbourhood last changed.
    strips: array<vec4f, /*STITCH_ROWS_TWICE*/>,
}
@group(1) @binding(0) var<uniform> tile: TileUniform;

// Where a tile's vertex is once the tile is stitched to its neighbours.
//
// The rule is the engine's: `tuile_core::stitch::plan` decides, from the tiles
// actually drawn, how far each side of each tile is from the line it has to be
// on, and hands that over as strips — a few knots a side, linear between them.
// This adds them up for one vertex, by where it is in its tile: the whole
// correction on an edge, fading to nothing over the band inward.
// `Stitch::offset` is the same arithmetic on the CPU, operation for operation.
//
// Rows of one state, from `base`:
//   0      knots on the west, south, east and north sides
//   1      the row each side's knots begin at
//   2..5   the correction at the south-west, south-east, north-east and
//          north-west corners
//   6..9   how far the west, south, east and north sides go past the
//          edge they are put on: a hair, so that rounding laps and never
//          gaps
//   10..   the knots: where along the side, then the correction
fn stitch_weight(across: f32) -> f32 {
    return clamp(1.0 - across / STITCH_BAND, 0.0, 1.0);
}

fn stitch_side(base: u32, side: u32, s: f32) -> vec3f {
    let count = u32(tile.strips[base][side]);
    if (count == 0u) { return vec3f(0.0); }
    let start = base + u32(tile.strips[base + 1u][side]);
    var low = 0u;
    var high = count;
    while (low < high) {
        let mid = (low + high) / 2u;
        if (tile.strips[start + mid].x <= s) { low = mid + 1u; } else { high = mid; }
    }
    if (low == 0u) { return tile.strips[start].yzw; }
    if (low == count) { return tile.strips[start + count - 1u].yzw; }
    let a = tile.strips[start + low - 1u];
    let b = tile.strips[start + low];
    let t = (s - a.x) / (b.x - a.x);
    return a.yzw + (b.yzw - a.yzw) * t;
}

fn stitch_offset(base: u32, uv: vec2f) -> vec3f {
    let u = uv.x;
    let north = 1.0 - uv.y;
    let w = vec4f(
        stitch_weight(u),
        stitch_weight(north),
        stitch_weight(1.0 - u),
        stitch_weight(1.0 - north),
    );
    if (all(w == vec4f(0.0))) { return vec3f(0.0); }
    return w.x * (stitch_side(base, 0u, north) + tile.strips[base + 6u].xyz)
        + w.y * (stitch_side(base, 1u, u) + tile.strips[base + 7u].xyz)
        + w.z * (stitch_side(base, 2u, north) + tile.strips[base + 8u].xyz)
        + w.w * (stitch_side(base, 3u, u) + tile.strips[base + 9u].xyz)
        - (w.x * w.y) * tile.strips[base + 2u].xyz
        - (w.y * w.z) * tile.strips[base + 3u].xyz
        - (w.z * w.w) * tile.strips[base + 4u].xyz
        - (w.w * w.x) * tile.strips[base + 5u].xyz;
}

// A vertex, in the tile's own frame: where its mesh has it, moved by the
// tile's strips — sliding, for the few frames after its neighbourhood
// changed, from where the strips it had put it.
fn stitched(position: vec3f, uv: vec2f) -> vec3f {
    if (tile.stitch.y == 0.0) { return position; }
    let now = stitch_offset(0u, uv);
    let slid = tile.stitch.x;
    if (slid >= 1.0) { return position + now; }
    return position + mix(stitch_offset(STITCH_ROWS, uv), now, slid);
}

struct MaterialUniform {
    base_color: vec4f,
    // x = 1 on the tile's first pass, 0 on every pass after it; y = 1 for a
    // material that declares `KHR_materials_unlit` (see `fs_main`).
    //
    // A tile with more layers than one draw can bind is drawn again rather than
    // coarsened — see `tuile_core::raster::MAX_IMAGERY_PASSES`. The later passes
    // are the same geometry with the next batch of layers, composed by alpha
    // blending, so they must start from *nothing* rather than from the base
    // colour: a fragment no layer of this pass covers has to leave the pass
    // beneath it showing, and starting from the base colour would paint over it
    // with untextured ground.
    // `pass` would be the name; WGSL reserves it.
    flags: vec4f,
    // The imagery levels the tile is told, for the three views that colour
    // tiles by them: x = the level its base layer wears (0 for none), y =
    // the level that layer asked for this step (0 for nothing), z = the
    // tile's own level, w = its role (0 itself, 1 held, 2 standing in) plus
    // four times the levels the terrain's level denied it.
    levels: vec4f,
}
@group(2) @binding(0) var<uniform> material: MaterialUniform;
@group(2) @binding(1) var base_tex: texture_2d<f32>;
@group(2) @binding(2) var base_samp: sampler;

// Imagery draped over the tile, blended on top of the base colour.
//
// A layer is two vec4, so that the whole set is one uniform array with the
// natural 16-byte stride:
//   [2i]     coverage:  u_min, v_min, u_max, v_max — in the TILE's uv space
//   [2i + 1] placement: translation.xy, scale.xy
// and `texture_uv = tile_uv * scale + translation`.
//
// The slot count is not written here. It comes from what the device reports it
// will bind — `tuile_core::raster::imagery_slots` turns that into a number — so
// this block and the sampling loop below are both generated by
// `renderer::ground_wgsl`, which substitutes the two markers.
//
// Writing it down was the older arrangement, and it needed a test that read the
// count back out of this file with a regex to check it still agreed with the
// core. A value with two homes always needs one of those.
//#IMAGERY_BINDINGS

struct VsIn {
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) normal: vec3f,
    @location(1) uv: vec2f,
    // Render space, not ECEF. Interpolating an ECEF position across a triangle
    // in f32 loses metres; render space keeps the magnitudes small enough that
    // it does not, which is the same reason the positions were rebased.
    @location(2) world: vec3f,
    // The vertex's height over the air's sphere, for the air: interpolated
    // across the triangle it is the ground the mesh means, where `world`
    // is the flat triangle's — a chord, kilometres under the ground inside
    // a coarse tile's facets. See `aerial_perspective`.
    @location(3) height: f32,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let world = tile.model * vec4f(stitched(in.position, in.uv), 1.0);
    out.clip = view.view_proj * world;
    out.normal = (tile.model * vec4f(in.normal, 0.0)).xyz;
    out.uv = in.uv;
    out.world = world.xyz;
    if (atmosphere.shape.w != 0.0) {
        // Over the physical atmosphere's sphere: its centre from the eye,
        // and the eye from the origin.
        let centre = atmosphere.centre.xyz + atmosphere.eye.xyz;
        out.height = length(world.xyz - centre) - atmosphere.centre.w;
    } else {
        out.height = length(world.xyz - view.air.earth.xyz) - view.air.earth.w;
    }
    return out;
}
/// thing left behind.
fn layer(dst: vec4f, tex: texture_2d<f32>, uv: vec2f, slot: u32) -> vec4f {
    return blend_layer(
        dst,
        tex,
        base_samp,
        uv,
        imagery.layers[2u * slot],
        imagery.layers[2u * slot + 1u],
    );
}

/// Diagnostic views, selected by `view.params.y`. Zero is the real picture.
///
/// They exist because "there is black on the globe" is not a diagnosis: a black
/// pixel can be ground that was never drawn, ground drawn with no imagery, ground
/// drawn with imagery that is itself black, or ground correctly drawn and then
/// unlit because the sun is behind the planet. Those four have four different
/// fixes and one appearance. Each mode below removes exactly one of the four
/// from the picture, so a single keypress splits the possibilities in half.
const DIAGNOSTIC_OFF: f32 = 0.0;
const DIAGNOSTIC_UNLIT: f32 = 1.0;
const DIAGNOSTIC_COVERAGE: f32 = 2.0;
const DIAGNOSTIC_GEOMETRY: f32 = 3.0;
// The two views after those answer a different question — not "why is
// this black" but "why is this soft": they colour each tile by the imagery
// level it wears (`material.levels`), so a coarse patch is read as a
// number rather than guessed from the blur.
//
// Levels: one hue a level round a wheel of eight — red at a multiple of
// eight (8, 16), then orange, yellow, green, cyan, blue, violet, magenta
// (15) — dark grey wearing nothing. Two tiles a level apart are a step
// round the wheel; the top of a tilted view, farther, is expected a step
// or two behind the foreground.
const DIAGNOSTIC_LEVELS: f32 = 4.0;
// Lag: each tile's imagery against what it asked for this step. Green at
// the level asked; yellow one level under it, orange two, red three or
// more; blue finer than asked; grey asking or wearing nothing. Red is a
// tile wearing an ancestor's picture while its own loads — the stand-in
// that smears — and a lag that stays once the loads land is the budget
// asking for less than the eye needs.
const DIAGNOSTIC_LAG: f32 = 5.0;
// Cap: each tile by the levels its terrain's level denies its imagery —
// the screen asks finer than the tile's terrain level plus the imagery cap
// allows, so the tile wears what it asked for (green in the lag view) and
// is soft all the same. Green none, yellow one level denied, orange two,
// red three or more; grey wearing nothing. A band of it across the far top
// of a tilted view is the cap, not the loads.
const DIAGNOSTIC_CAP: f32 = 6.0;

// The level view's hue for `level`.
fn level_hue(level: f32) -> vec3f {
    let step = i32(round(level)) % 8;
    switch step {
        case 0: { return vec3f(0.95, 0.15, 0.15); }
        case 1: { return vec3f(1.0, 0.55, 0.1); }
        case 2: { return vec3f(0.95, 0.9, 0.1); }
        case 3: { return vec3f(0.2, 0.8, 0.2); }
        case 4: { return vec3f(0.1, 0.85, 0.9); }
        case 5: { return vec3f(0.2, 0.35, 1.0); }
        case 6: { return vec3f(0.6, 0.25, 0.95); }
        default: { return vec3f(0.95, 0.3, 0.8); }
    }
}

// The lag view's colour for a tile wearing `worn` that asked for `asked`.
fn lag_colour(worn: f32, asked: f32) -> vec3f {
    if (worn < 0.5 || asked < 0.5) { return vec3f(0.35); }
    let under = asked - worn;
    if (under < -0.5) { return vec3f(0.2, 0.4, 1.0); }
    if (under < 0.5) { return vec3f(0.15, 0.75, 0.2); }
    if (under < 1.5) { return vec3f(0.95, 0.9, 0.1); }
    if (under < 2.5) { return vec3f(1.0, 0.5, 0.05); }
    return vec3f(0.95, 0.1, 0.1);
}

// The cap view's colour for a tile wearing `worn` denied `capped` levels.
fn cap_colour(worn: f32, capped: f32) -> vec3f {
    if (worn < 0.5) { return vec3f(0.35); }
    return lag_colour(1.0, 1.0 + capped);
}

// What the level views draw over a tile's colour: the relief, faintly,
// so the ground stays readable; a dark edge round the tile, so tiles can
// be counted; hatching on a mesh drawn in another's place (held, or
// standing in). The edge is a pixel and a half wide at every distance,
// measured along each axis of the tile's uv (`d` its change across a
// pixel) as `outlined` measures it: one axis's change sets the width of
// that axis's sides only — taken for both, a tile seen at a grazing angle
// had bands many pixels wide on the sides running into the view — and a
// skirt, whose uv along its side does not change, has no edge there and
// is not painted dark all the way down.
fn tile_marks(colour: vec3f, uv: vec2f, role: f32, lambert: f32, d: vec2f) -> vec3f {
    var c = colour * (0.65 + 0.35 * lambert);
    var px = 1e9;
    if (d.x > 0.0) { px = min(px, min(uv.x, 1.0 - uv.x) / d.x); }
    if (d.y > 0.0) { px = min(px, min(uv.y, 1.0 - uv.y) / d.y); }
    if (px < 1.5) { c *= 0.3; }
    if (role > 0.5 && fract((uv.x + uv.y) * 6.0) < 0.5) { c *= 0.6; }
    return c;
}

// The tiles' outlines over the real picture, imagery and all
// (`view.params.z`, the viewer's `tuile://view?labels=on`): a dark line a
// pixel wide on the tile's edge and a light one inside it, so an edge reads
// on snow and on sea alike, and the picture between the edges is untouched.
//
// Measured in pixels along each axis of the tile's uv (`d` its change
// across a pixel), so the line is a pixel wide however the tile is turned.
// An axis whose uv does not change across the pixel has no edge there: a
// skirt hanging from the tile's edge carries that edge's uv all the way
// down, and outlining it would paint the whole wall.
fn outlined(colour: vec3f, uv: vec2f, d: vec2f) -> vec3f {
    var px = 1e9;
    if (d.x > 0.0) { px = min(px, min(uv.x, 1.0 - uv.x) / d.x); }
    if (d.y > 0.0) { px = min(px, min(uv.y, 1.0 - uv.y) / d.y); }
    // Dark, never black: black on the globe is ground missing, to the eye
    // and to every scan of a picture (`tuile_tape::scan::BLACK`).
    if (px < 1.0) { return vec3f(0.03); }
    if (px < 2.0) { return vec3f(0.85); }
    return colour;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let first = material.flags.x;
    var albedo = textureSample(base_tex, base_samp, in.uv) * material.base_color;
    // Content that owns its texturing has no layers, and terrain has no base
    // colour texture — the two never both contribute, but nothing here needs to
    // know which case it is in.
    // `w` carries the covering-layer count, not opacity — see `blend_layer`.
    var ground = vec4f(albedo.rgb * first, 0.0);
    //#IMAGERY_SAMPLES
    let layers = ground.w;
    // On the first pass the surface is opaque, whatever its layers did — the
    // base colour is behind them and there is nothing underneath to reveal. On a
    // later pass only the fragments this batch actually covered may be written,
    // so the alpha *is* the coverage, and the blend leaves everything else to
    // the pass before.
    //
    // This composes exactly rather than approximately: lighting is a multiply
    // and the air is a `mix`, both linear in the colour, so alpha-blending the
    // lit and hazed results of two passes gives the same answer as lighting and
    // hazing the blended colour once.
    let opacity = max(first * albedo.a, saturate(layers));
    albedo = vec4f(ground.rgb, opacity);

    var n = normalize(in.normal);
    // `KHR_materials_unlit`: the colour already carries its light — a
    // photograph of the place, its slopes lit and shaded by the day it was
    // taken — and the facets of the mesh are not to shade it again. It is lit
    // as the ground is, by the sky over the place: the normal is the local
    // vertical, so the day and the night, the sun's height and the air still
    // reach it, and nothing of its own triangulation does — two tiles side by
    // side, each with the normals its own triangles give, would otherwise be
    // shaded apart along their common edge.
    if (material.flags.y > 0.5) {
        var centre = view.air.earth.xyz;
        if (atmosphere.shape.w != 0.0) {
            centre = atmosphere.centre.xyz + atmosphere.eye.xyz;
        }
        n = normalize(in.world - centre);
    }
    let lambert = max(dot(n, -view.sun_dir.xyz), 0.0);
    let ambient = view.params.x;
    let lit = albedo.rgb * (ambient + (1.0 - ambient) * lambert);

    let mode = view.params.y;
    // The uv a pixel spans: taken here, in uniform control flow, for the
    // level views' tile edges.
    let uv_d = fwidth(in.uv);
    // The role and the capped levels share the last lane.
    let role = material.levels.w % 4.0;
    let capped = floor(material.levels.w / 4.0);
    if (mode == DIAGNOSTIC_LEVELS) {
        var c = vec3f(0.25);
        if (material.levels.x > 0.5) { c = level_hue(material.levels.x); }
        return vec4f(tile_marks(c, in.uv, role, lambert, uv_d), first);
    }
    if (mode == DIAGNOSTIC_LAG) {
        let c = lag_colour(material.levels.x, material.levels.y);
        return vec4f(tile_marks(c, in.uv, role, lambert, uv_d), first);
    }
    if (mode == DIAGNOSTIC_CAP) {
        let c = cap_colour(material.levels.x, capped);
        return vec4f(tile_marks(c, in.uv, role, lambert, uv_d), first);
    }
    // Geometry: a flat lit surface, imagery and air removed. Anything visible
    // here IS a mesh. Black here means nothing was drawn at all, which is a
    // traversal or upload fault and never a texture one.
    if (mode == DIAGNOSTIC_GEOMETRY) {
        // Alpha 0 on a later pass: the geometry view asks whether a mesh was
        // drawn, and a second pass over the same mesh has nothing to add. Left
        // opaque it would blend a flat grey over the answer.
        return vec4f(vec3f(0.15 + 0.85 * lambert), first);
    }
    // Coverage: magenta where no layer reached this fragment, green where
    // exactly one did, blue where several overlap. Answers "is a texture
    // missing *here*" per pixel rather than per tile.
    if (mode == DIAGNOSTIC_COVERAGE) {
        // Magenta only on the first pass. A later pass covers a fraction of the
        // tile by construction, and painting its uncovered fragments magenta
        // would report missing imagery over ground the first pass drew.
        if (layers < 0.5) { return vec4f(1.0, 0.0, 1.0, first); }
        if (layers < 1.5) { return vec4f(0.0, 0.8, 0.2, 1.0); }
        return vec4f(0.1, 0.4, 1.0, 1.0);
    }
    // The outlines, over the picture as it is drawn below — after its light
    // and its air, so the line is the same line at every distance.
    let outlines = view.params.z > 0.5;
    // Unlit: the imagery exactly as sampled, with lighting and air removed. If
    // the black survives this, no amount of sun or atmosphere caused it.
    if (mode == DIAGNOSTIC_UNLIT) {
        var c = albedo.rgb;
        if (outlines) { c = outlined(c, in.uv, uv_d); }
        return vec4f(c, opacity);
    }
    if (atmosphere.shape.w != 0.0) {
        // The physical atmosphere (`ground_seen_through`, the film's own):
        // the fragment lit by the sun and the Moon where it is, and seen
        // through the air between it and the eye. From the eye; the ground
        // it means is the drawn point moved to the height its vertices
        // interpolate — a coarse tile's flat facet is a chord kilometres
        // under it. The air is the frame's aerial table read at that
        // ground (`air_seen`): the march the film makes for each pixel,
        // made once a frame at the table's nodes.
        let p = in.world - atmosphere.eye.xyz;
        var facing = n;
        if (dot(facing, p) > 0.0) { facing = -facing; }
        let x = p - atmosphere.centre.xyz;
        let ground = atmosphere.centre.xyz + normalize(x) * (atmosphere.centre.w + in.height);
        let ambient = max(
            atmosphere.ambient.xyz + atmosphere.sh_x.xyz * facing.x + atmosphere.sh_y.xyz * facing.y
                + atmosphere.sh_z.xyz * facing.z,
            vec3f(0.0),
        );
        let air = air_seen(ground);
        var seen = graded(ground_seen_through(
            albedo.rgb, p, ground, facing, ambient, 1.0, air.veil, air.through,
        ));
        if (outlines) { seen = outlined(seen, in.uv, uv_d); }
        return vec4f(seen, albedo.a);
    }
    var seen = aerial_perspective(lit, in.world, in.height, view.air);
    if (outlines) { seen = outlined(seen, in.uv, uv_d); }
    return vec4f(seen, albedo.a);
}

/// What the air between the eye and this fragment does to its colour.
///
/// Single scattering through an exponential atmosphere, integrated in closed
/// form. The reference implementation ray-marches this; over the ground the two
/// agree closely and a march costs sixty-four samples a fragment.
///
/// Two things come out of it, and the second is the one people notice. Distant
