// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One line where two drawn tiles meet.
//!
//! Two tiles that share a stretch of edge each state the ground along it from
//! their own data, and two levels of a source do not state the same ground:
//! metres apart along a line both sides place to the centimetre. A skirt hides
//! the step. This removes it: of the two tiles one keeps its edge and the
//! other is put on it, and which is which is decided here, from the set of
//! tiles a frame actually draws, for every renderer alike.
//!
//! **The rule.** Tiles are ordered — by level, then by the level of the data
//! their surface is from, then by place — and the first is the *coarsest*.
//!
//! - Along a shared stretch the coarser tile is the master: its edge, a line
//!   of straight segments between its own vertices, is what both sides draw.
//!   The other tile's edge vertices are moved onto that line, and it is given
//!   a vertex wherever the master has one it lacks — an edge of straight
//!   segments lies on another only where it bends at the same places.
//! - At a point where tiles meet — a corner of one or more of them — the
//!   coarsest tile touching it says where the ground is, for all of them.
//!
//! Nothing else moves an edge, so the plan is a function of the drawn set
//! alone: the same selection gives the same numbers whatever order it comes
//! in, on every run.
//!
//! **What a renderer is handed** ([`Stitch`], one a tile): along each side a
//! short strip of [`Knot`]s — how far the edge is from where it has to be, as
//! a vector, piecewise linear along the side — and the vertices to add
//! ([`Insert`]). A vertex stage adds [`Stitch::offset`] to each vertex: the
//! whole correction on the edge, fading to nothing over [`BAND`] of the tile
//! so that the edge row does not hinge against the row inside it. A mesh that
//! is displaced is first [`split`], which changes what triangles it has and
//! not where its surface is.
//!
//! [`displaced`] is the same arithmetic on the CPU, in `f32` as a shader does
//! it: what an instrument measures, and what a renderer with no vertex stage
//! of its own hands over.

use std::collections::HashMap;

use glam::{DVec3, Mat4, Vec3};

use crate::content::{DecodedMesh, DecodedTileContent};

/// How far into a tile an edge's correction reaches, as a part of the tile:
/// whole on the edge, none from here inward.
///
/// Chosen by measure. With no band the edge row alone moves and every
/// triangle against the edge turns by the whole step over one row — a crease
/// along the seam where there was a step. Over a quarter of the tile the same
/// step is a slope too gentle to see; see `a_band_spreads_the_hinge`.
pub const BAND: f32 = 0.25;

/// How near an edge a texture coordinate has to be to be on it.
const ON_THE_EDGE: f32 = 1.0e-6;
/// Two places along an edge nearer than this, as a part of the edge, are one.
const SAME_PLACE: f64 = 1.0e-5;
/// A side nowhere further than this from where it has to be, in metres, and
/// with no vertex to gain, is left alone.
const NOTHING: f64 = 1.0e-4;

/// Where a texture coordinate is against a side: how far from it, and where
/// along it — northward along a west or east side, eastward along the others.
/// In `f32`, exactly as the shaders do it.
fn place(side: usize, uv: [f32; 2]) -> (f32, f32) {
    let (u, north) = (uv[0], 1.0 - uv[1]);
    match side {
        0 => (u, north),
        1 => (north, u),
        2 => (1.0 - u, north),
        _ => (1.0 - north, u),
    }
}

/// The texture coordinate of the place `along` a side, and where a shader
/// will find it along that side — not always `along` to the last bit.
fn on_side(side: usize, along: f32) -> ([f32; 2], f32) {
    let uv = match side {
        0 => [0.0, 1.0 - along],
        1 => [along, 1.0],
        2 => [1.0, 1.0 - along],
        _ => [along, 0.0],
    };
    (uv, place(side, uv).1)
}

/// How far past its master's edge a stitched edge is put, as a part of the
/// largest coordinate its vertices have about their tile's origin plus the
/// edge's length — and [`LEAST_LAP`] at least.
///
/// Two tiles narrow the same line to `f32` about two origins, and each is
/// then placed from the eye in `f32` again: the two edges are drawn a tenth
/// of a millimetre apart, which is nothing — except that a rasteriser gives
/// a sample between two triangles to neither, and a sample of a picture now
/// and then falls in that hair. So the edge that moves is moved a little
/// further, level, into the tile it joins: there the hair is a lap, not a
/// gap. Measured on sixty-one frames of a film drawn with no skirts: 74
/// pixels touched by a hole without it, 25 to 37 with it — it takes away
/// about half of what is left, not all; the skirts take the rest. A few
/// millimetres; the engine's skirts lean out by a hundred times that, for
/// the same reason.
pub const LAP: f64 = 2.0e-6;
/// The least a side laps, in metres: a tile is placed from the eye, which
/// may be far from a small tile.
pub const LEAST_LAP: f64 = 2.0e-3;

/// Level, out of a tile across `side`, at `at` in the Earth's frame.
fn outward(side: usize, at: DVec3) -> DVec3 {
    let up = at.normalize_or_zero();
    let east = DVec3::Z.cross(up).normalize_or_zero();
    let north = up.cross(east);
    match side {
        0 => -east,
        1 => -north,
        2 => east,
        _ => north,
    }
}

/// A vertex of a tile's surface on one of its sides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Post {
    /// From 0 to 1 along the side.
    pub along: f32,
    /// As the mesh holds it: about the tile's origin.
    pub local: [f32; 3],
}

/// A tile's four sides as its mesh states them: the surface's vertices on
/// each, in order along it. West, south, east, north.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edges {
    pub origin: DVec3,
    pub sides: [Vec<Post>; 4],
}

impl Edges {
    /// Of a tile's content. Empty — a tile nothing is stitched to — when the
    /// content is not placed by its origin alone.
    pub fn of(content: &DecodedTileContent) -> Self {
        if content.transform_local != Mat4::IDENTITY {
            return Self::default();
        }
        let mut edges = Self {
            origin: content.local_origin_ecef,
            sides: Default::default(),
        };
        for mesh in &content.meshes {
            if let Some(uvs) = &mesh.uvs {
                edges.gather(&mesh.positions, uvs);
            }
        }
        edges.settle();
        edges
    }

    /// Of one mesh's vertices about `origin`.
    pub fn of_vertices(origin: DVec3, positions: &[[f32; 3]], uvs: &[[f32; 2]]) -> Self {
        let mut edges = Self {
            origin,
            sides: Default::default(),
        };
        edges.gather(positions, uvs);
        edges.settle();
        edges
    }

    fn gather(&mut self, positions: &[[f32; 3]], uvs: &[[f32; 2]]) {
        for (p, uv) in positions.iter().zip(uvs) {
            for (side, posts) in self.sides.iter_mut().enumerate() {
                let (across, along) = place(side, *uv);
                if across.abs() < ON_THE_EDGE {
                    posts.push(Post { along, local: *p });
                }
            }
        }
    }

    /// In order along each side; of the vertices at one place — the
    /// surface's and its skirt's — the surface's, the furthest from the
    /// Earth's centre.
    fn settle(&mut self) {
        let origin = self.origin;
        let radius = |p: &Post| (origin + Vec3::from(p.local).as_dvec3()).length_squared();
        for posts in &mut self.sides {
            posts.sort_by(|a, b| a.along.total_cmp(&b.along));
            let mut kept: Vec<Post> = Vec::with_capacity(posts.len());
            for post in posts.drain(..) {
                match kept.last_mut() {
                    Some(last) if post.along == last.along => {
                        if radius(&post) > radius(last) {
                            *last = post;
                        }
                    }
                    _ => kept.push(post),
                }
            }
            *posts = kept;
        }
    }

    fn at(&self, post: &Post) -> DVec3 {
        self.origin + Vec3::from(post.local).as_dvec3()
    }

    /// How a side laps over its master's: [`LAP`] of the largest coordinate
    /// on it and of its length, level and outward at its middle.
    fn lap(&self, side: usize) -> Vec3 {
        let posts = &self.sides[side];
        let (Some(first), Some(last)) = (posts.first(), posts.last()) else {
            return Vec3::ZERO;
        };
        let far = posts
            .iter()
            .map(|p| Vec3::from(p.local).abs().max_element())
            .fold(0.0, f32::max);
        let length = (Vec3::from(first.local) - Vec3::from(last.local)).length();
        let middle = (self.at(first) + self.at(last)) / 2.0;
        (outward(side, middle) * (LAP * f64::from(far + length)).max(LEAST_LAP)).as_vec3()
    }

    /// Whether a side is stated from end to end.
    fn whole(&self, side: usize) -> bool {
        let posts = &self.sides[side];
        match (posts.first(), posts.last()) {
            (Some(first), Some(last)) => {
                posts.len() >= 2 && first.along <= ON_THE_EDGE && last.along >= 1.0 - ON_THE_EDGE
            }
            _ => false,
        }
    }

    /// The side's own line at `along`: on the segment between the two posts
    /// around it.
    fn on(&self, side: usize, along: f64) -> Option<DVec3> {
        let posts = &self.sides[side];
        let (first, last) = (posts.first()?, posts.last()?);
        if along <= f64::from(first.along) {
            return Some(self.at(first));
        }
        if along >= f64::from(last.along) {
            return Some(self.at(last));
        }
        let next = posts.partition_point(|p| f64::from(p.along) <= along);
        let (a, b) = (&posts[next - 1], &posts[next]);
        let t = (along - f64::from(a.along))
            / (f64::from(b.along) - f64::from(a.along)).max(f64::MIN_POSITIVE);
        Some(self.at(a).lerp(self.at(b), t))
    }

    /// Where the tile is in the geographic quadtree (two tiles across at
    /// level 0), read off its own corners: level, x, and y from the south.
    /// `None` for a mesh that is not a whole tile of that tree.
    ///
    /// For a consumer whose tile handles do not say: the geometry does.
    pub fn cell(&self) -> Option<(u32, u64, u64)> {
        use std::f64::consts::PI;
        if !(0..4).all(|side| self.whole(side)) {
            return None;
        }
        let south_west = crate::geo::ecef_to_geodetic(self.at(self.sides[0].first()?));
        let north_east = crate::geo::ecef_to_geodetic(self.at(self.sides[2].last()?));
        let height = north_east.lat - south_west.lat;
        if height.is_nan() || height <= 0.0 {
            return None;
        }
        let level = (PI / height).log2().round();
        if !(0.0..=40.0).contains(&level) {
            return None;
        }
        let size = PI / 2f64.powi(level as i32);
        let width = (north_east.lon - south_west.lon).rem_euclid(2.0 * PI);
        let (x, y) = (
            (south_west.lon + PI) / size,
            (south_west.lat + PI / 2.0) / size,
        );
        let near = |v: f64| (v - v.round()).abs() < 1.0e-3;
        let fits = near(x) && near(y) && (height / size - 1.0).abs() < 1.0e-3;
        // A tile on the antimeridian's east side spans to +π, which wraps.
        let wide = (width / size - 1.0).abs() < 1.0e-3 || (level == 0.0 && width.abs() < 1.0e-9);
        (fits && wide).then(|| {
            let across = 2u64 << level as u32;
            (level as u32, (x.round() as u64) % across, y.round() as u64)
        })
    }
}

/// One tile a frame draws.
#[derive(Debug, Clone, Copy)]
pub struct Tile<'a> {
    pub level: u32,
    pub x: u64,
    /// Counted from the south.
    pub y: u64,
    /// The level of the data its surface is from: its own, or an ancestor's
    /// it was cut from. A caller that does not know says `level`.
    pub source: u32,
    pub edges: &'a Edges,
}

/// How far an edge is from where it has to be, at one place along it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Knot {
    pub along: f32,
    /// In the Earth's axes, metres.
    pub delta: [f32; 3],
}

/// A vertex a mesh has to gain on a side, on its own edge as it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Insert {
    pub along: f32,
    pub local: [f32; 3],
}

/// What stitching asks of one tile. See the module.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stitch {
    /// A side's correction: knots in order along it, linear between them.
    /// Empty for a side that stays where it is.
    pub sides: [Vec<Knot>; 4],
    pub inserts: [Vec<Insert>; 4],
    /// How far each side that moves onto another tile's goes past it, level
    /// and outward: see [`LAP`]. Nothing for a side that keeps its line.
    pub laps: [[f32; 3]; 4],
    /// The level of the coarsest tile drawn across each side, if any is.
    pub across: [Option<u32>; 4],
}

fn weight(across: f32, band: f32) -> f32 {
    (1.0 - across / band).clamp(0.0, 1.0)
}

fn along_a_side(knots: &[Knot], s: f32) -> Vec3 {
    let (Some(first), Some(last)) = (knots.first(), knots.last()) else {
        return Vec3::ZERO;
    };
    let (mut low, mut high) = (0usize, knots.len());
    while low < high {
        let mid = (low + high) / 2;
        if knots[mid].along <= s {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low == 0 {
        return first.delta.into();
    }
    if low == knots.len() {
        return last.delta.into();
    }
    let (a, b) = (knots[low - 1], knots[low]);
    let t = (s - a.along) / (b.along - a.along);
    let (da, db) = (Vec3::from(a.delta), Vec3::from(b.delta));
    da + (db - da) * t
}

impl Stitch {
    /// Nothing to move and nothing to add.
    pub fn is_nothing(&self) -> bool {
        self.sides.iter().all(Vec::is_empty)
            && self.inserts.iter().all(Vec::is_empty)
            && self.laps == [[0.0; 3]; 4]
    }

    /// The correction at each corner — south-west, south-east, north-east,
    /// north-west — where both sides that meet there carry one.
    fn corners(&self) -> [Vec3; 4] {
        let [west, south, east, north] = &self.sides;
        let end = |a: &Vec<Knot>, b: &Vec<Knot>, of: Option<&Knot>| match of {
            Some(knot) if !a.is_empty() && !b.is_empty() => Vec3::from(knot.delta),
            _ => Vec3::ZERO,
        };
        [
            end(west, south, west.first()),
            end(south, east, south.last()),
            end(east, north, east.last()),
            end(north, west, west.last()),
        ]
    }

    /// How far the vertex at `uv` moves: each side's correction where the
    /// vertex is along it — and its lap — weighed by how near that side it
    /// is, less what two sides both bring at the corner they share.
    ///
    /// `f32` throughout and operation for operation what the shaders compute
    /// (`stitch.wgsl` of each renderer), so that a mesh displaced here is the
    /// mesh a GPU draws.
    pub fn offset(&self, uv: [f32; 2], band: f32) -> Vec3 {
        let (u, north) = (uv[0], 1.0 - uv[1]);
        let w = [
            weight(u, band),
            weight(north, band),
            weight(1.0 - u, band),
            weight(1.0 - north, band),
        ];
        if w == [0.0; 4] {
            return Vec3::ZERO;
        }
        let c = self.corners();
        w[0] * (along_a_side(&self.sides[0], north) + Vec3::from(self.laps[0]))
            + w[1] * (along_a_side(&self.sides[1], u) + Vec3::from(self.laps[1]))
            + w[2] * (along_a_side(&self.sides[2], north) + Vec3::from(self.laps[2]))
            + w[3] * (along_a_side(&self.sides[3], u) + Vec3::from(self.laps[3]))
            - (w[0] * w[1]) * c[0]
            - (w[1] * w[2]) * c[1]
            - (w[2] * w[3]) * c[2]
            - (w[3] * w[0]) * c[3]
    }

    /// The strips as a renderer uploads them, four numbers a row: how many
    /// knots each side has, where each side's begin, the four corners'
    /// corrections, the four sides' laps, then the knots — where along, and
    /// the correction.
    pub fn packed(&self) -> Vec<[f32; 4]> {
        let mut rows = vec![[0.0f32; 4]; 10];
        let mut start = 10usize;
        for (side, knots) in self.sides.iter().enumerate() {
            rows[0][side] = knots.len() as f32;
            rows[1][side] = start as f32;
            start += knots.len();
        }
        for (row, corner) in rows[2..6].iter_mut().zip(self.corners()) {
            *row = [corner.x, corner.y, corner.z, 0.0];
        }
        for (row, lap) in rows[6..10].iter_mut().zip(self.laps) {
            *row = [lap[0], lap[1], lap[2], 0.0];
        }
        rows.extend(
            self.sides
                .iter()
                .flatten()
                .map(|k| [k.along, k.delta[0], k.delta[1], k.delta[2]]),
        );
        rows
    }
}

/// Places on the grid are compared at this level.
const DEEP: u32 = 40;

/// The line of the grid a side lies on — a meridian or a parallel, and
/// which — and the stretch of it the side covers.
type Span = (bool, u64, (u64, u64));
/// What tiles are ordered by, the coarsest first: level, the level of the
/// data, then place.
type Key = (u32, u32, u64, u64);

/// A side on its line of the grid: which tile, which side, the stretch.
type OnALine = Vec<(usize, usize, (u64, u64))>;

struct Grid<'a> {
    tiles: &'a [Tile<'a>],
    round: u64,
    band: f64,
    /// Points where tiles meet, once worked out.
    met: std::cell::RefCell<HashMap<(u64, u64), Option<DVec3>>>,
    lines: HashMap<(bool, u64), OnALine>,
    spans: Vec<[Option<Span>; 4]>,
}

impl Grid<'_> {
    fn key(&self, n: usize) -> Key {
        let t = &self.tiles[n];
        (t.level, t.source, t.y, t.x)
    }

    /// Where a point of a line is along a side lying on it.
    fn along(stretch: (u64, u64), at: u64) -> f64 {
        (at - stretch.0) as f64 / (stretch.1 - stretch.0) as f64
    }

    /// Where the ground is at a point where tiles meet: on the line of the
    /// coarsest tile that touches it — its corner as its mesh has it, or,
    /// for a point part-way along one of its sides, that side as it is
    /// drawn, its own ends settled first. That asks only ever coarser tiles,
    /// so it ends.
    fn meeting(&self, x: u64, y: u64) -> Option<DVec3> {
        let x = x % self.round;
        if let Some(known) = self.met.borrow().get(&(x, y)) {
            return *known;
        }
        let mut best: Option<(Key, usize, usize, f64)> = None;
        let mut hold = |n: usize, side: usize, along: f64| {
            let key = self.key(n);
            if best.is_none_or(|(least, ..)| key < least) {
                best = Some((key, n, side, along));
            }
        };
        if let Some(sides) = self.lines.get(&(true, x)) {
            for &(n, side, stretch) in sides {
                if (stretch.0..=stretch.1).contains(&y) {
                    hold(n, side, Self::along(stretch, y));
                }
            }
        }
        if let Some(sides) = self.lines.get(&(false, y)) {
            for &(n, side, stretch) in sides {
                // A tile against the antimeridian ends where the round does.
                for at in [x, x + self.round] {
                    if (stretch.0..=stretch.1).contains(&at) {
                        hold(n, side, Self::along(stretch, at));
                    }
                }
            }
        }
        let at = best.and_then(|(_, n, side, along)| {
            if along <= 0.0 || along >= 1.0 {
                self.tiles[n].edges.on(side, along)
            } else {
                line_at(&self.settled(n, side), along)
            }
        });
        self.met.borrow_mut().insert((x, y), at);
        at
    }

    /// The two ends of a side, as points of the grid.
    fn ends(&self, n: usize, side: usize) -> Option<[(u64, u64); 2]> {
        let (meridian, line, (from, to)) = self.spans[n][side]?;
        Some(if meridian {
            [(line, from), (line, to)]
        } else {
            [(from, line), (to, line)]
        })
    }

    /// A side's line as it is drawn once stitched, were the tile its
    /// master: its own posts, its two ends where the tiles meeting there
    /// have the ground — and what an end moves by, fading along the side
    /// over the band as it fades into the tile, so that a corner's
    /// correction is not taken up by the one segment next to it. Along the
    /// side, and in the Earth's frame.
    fn settled(&self, n: usize, side: usize) -> Vec<(f64, DVec3)> {
        let edges = self.tiles[n].edges;
        let mut line: Vec<(f64, DVec3)> = edges.sides[side]
            .iter()
            .map(|p| (f64::from(p.along), edges.at(p)))
            .collect();
        let (Some([from, to]), Some(first), Some(last)) = (
            self.ends(n, side),
            line.first().copied(),
            line.last().copied(),
        ) else {
            return line;
        };
        let moved = |end: (f64, DVec3), node: (u64, u64)| {
            self.meeting(node.0, node.1)
                .map_or(DVec3::ZERO, |at| at - end.1)
        };
        let (at_first, at_last) = (moved(first, from), moved(last, to));
        if at_first == DVec3::ZERO && at_last == DVec3::ZERO {
            return line;
        }
        let fade = |along: f64| (1.0 - along / self.band).clamp(0.0, 1.0);
        for (along, at) in &mut line {
            *at += at_first * fade(*along) + at_last * fade(1.0 - *along);
        }
        if let Some(first) = line.first_mut() {
            first.0 = 0.0;
        }
        if let Some(last) = line.last_mut() {
            last.0 = 1.0;
        }
        line
    }

    /// The coarsest tile drawn across a side, and the side of it that faces.
    fn across(&self, n: usize, side: usize) -> Option<(usize, usize, (u64, u64))> {
        let (meridian, line, stretch) = self.spans[n][side]?;
        self.lines
            .get(&(meridian, line))?
            .iter()
            .filter(|other| other.1 / 2 != side / 2)
            .filter(|other| other.2 .0.max(stretch.0) < other.2 .1.min(stretch.1))
            .min_by_key(|other| self.key(other.0))
            .copied()
    }
}

fn line_at(line: &[(f64, DVec3)], along: f64) -> Option<DVec3> {
    let (first, last) = (line.first()?, line.last()?);
    if along <= first.0 {
        return Some(first.1);
    }
    if along >= last.0 {
        return Some(last.1);
    }
    let next = line.partition_point(|p| p.0 <= along);
    let (a, b) = (line[next - 1], line[next]);
    Some(a.1.lerp(b.1, (along - a.0) / (b.0 - a.0).max(f64::MIN_POSITIVE)))
}

/// **What each drawn tile has to do for the frame's tiles to meet.**
///
/// One [`Stitch`] a tile, in the order given. `roots` is how many tiles the
/// tree has across and up at level 0 (2 and 1 for the geographic globe); the
/// tree wraps east to west and a side on a pole meets nothing. `band` is the
/// one the corrections will be applied with ([`BAND`]): a corner's reaches
/// that far along its sides too.
pub fn plan(tiles: &[Tile<'_>], roots: (u64, u64), band: f32) -> Vec<Stitch> {
    let (round, top) = (roots.0 << DEEP, roots.1 << DEEP);
    let span = |tile: &Tile<'_>, side: usize| -> Option<Span> {
        if !tile.edges.whole(side) {
            return None;
        }
        let shift = DEEP.checked_sub(tile.level)?;
        let (x, y, one) = (tile.x << shift, tile.y << shift, 1u64 << shift);
        match side {
            0 => Some((true, x, (y, y + one))),
            2 => Some((true, (x + one) % round, (y, y + one))),
            1 => (y > 0).then_some((false, y, (x, x + one))),
            _ => (y + one < top).then_some((false, y + one, (x, x + one))),
        }
    };
    let spans: Vec<[Option<Span>; 4]> = tiles
        .iter()
        .map(|tile| std::array::from_fn(|side| span(tile, side)))
        .collect();
    let mut lines: HashMap<(bool, u64), OnALine> = HashMap::new();
    for (n, of_tile) in spans.iter().enumerate() {
        for (side, span) in of_tile.iter().enumerate() {
            if let Some((meridian, line, stretch)) = span {
                lines
                    .entry((*meridian, *line))
                    .or_default()
                    .push((n, side, *stretch));
            }
        }
    }
    let grid = Grid {
        tiles,
        round,
        band: f64::from(band).max(f64::MIN_POSITIVE),
        met: Default::default(),
        lines,
        spans,
    };

    let mut plans = Vec::with_capacity(tiles.len());
    for (n, tile) in tiles.iter().enumerate() {
        let mut stitch = Stitch::default();
        for side in 0..4 {
            let Some((_, _, stretch)) = grid.spans[n][side] else {
                continue;
            };
            let facing = grid.across(n, side);
            stitch.across[side] = facing.map(|other| tiles[other.0].level);
            // The line this side has to be on, along the side.
            let target: Vec<(f64, DVec3)> = match facing {
                Some((m, facing_side, of_master)) if grid.key(m) < grid.key(n) => {
                    let whole = (of_master.1 - of_master.0) as f64;
                    let from = (stretch.0.max(of_master.0) - of_master.0) as f64 / whole;
                    let to = (stretch.1.min(of_master.1) - of_master.0) as f64 / whole;
                    let master = grid.settled(m, facing_side);
                    let mut line = Vec::with_capacity(master.len() + 2);
                    if let Some(at) = line_at(&master, from) {
                        line.push((0.0, at));
                    }
                    line.extend(
                        master
                            .iter()
                            .map(|&(along, at)| ((along - from) / (to - from), at))
                            .filter(|(along, _)| *along > SAME_PLACE && *along < 1.0 - SAME_PLACE),
                    );
                    if let Some(at) = line_at(&master, to) {
                        line.push((1.0, at));
                    }
                    line
                }
                _ => grid.settled(n, side),
            };
            // Where the tiles across have vertices: this side takes one at
            // each, so that the two edges are not only one line but the
            // same segments — a vertex against the middle of a segment is
            // on it only as far as rounding goes, and a rasteriser can
            // leave a pixel between the two.
            let mut theirs: Vec<f64> = Vec::new();
            if let Some((meridian, line, _)) = grid.spans[n][side] {
                let whole = (stretch.1 - stretch.0) as f64;
                for other in grid.lines.get(&(meridian, line)).into_iter().flatten() {
                    if other.1 / 2 == side / 2 || other.2 .0 >= stretch.1 || other.2 .1 <= stretch.0
                    {
                        continue;
                    }
                    let (size, start) = ((other.2 .1 - other.2 .0) as f64, other.2 .0 as f64);
                    theirs.extend(tiles[other.0].edges.sides[other.1].iter().map(|post| {
                        (start + f64::from(post.along) * size - stretch.0 as f64) / whole
                    }));
                }
            }
            let (knots, inserts) = strip(tile.edges, side, &target, &theirs);
            let moves = facing.is_some_and(|(m, ..)| grid.key(m) < grid.key(n));
            if moves {
                stitch.laps[side] = tile.edges.lap(side).to_array();
            }
            stitch.sides[side] = knots;
            stitch.inserts[side] = inserts;
        }
        plans.push(stitch);
    }
    plans
}

/// A side's knots and the vertices it has to gain, for the line it has to
/// be on and the places the tiles across have vertices.
fn strip(
    edges: &Edges,
    side: usize,
    target: &[(f64, DVec3)],
    theirs: &[f64],
) -> (Vec<Knot>, Vec<Insert>) {
    let posts = &edges.sides[side];
    let mut wanted: Vec<f64> = target
        .iter()
        .map(|p| p.0)
        .chain(theirs.iter().copied())
        .filter(|along| *along > SAME_PLACE && *along < 1.0 - SAME_PLACE)
        .collect();
    wanted.sort_by(f64::total_cmp);
    wanted.dedup_by(|b, a| *b - *a < SAME_PLACE);
    let mut inserts: Vec<Insert> = Vec::new();
    for along in wanted {
        let own = posts.partition_point(|p| f64::from(p.along) < along);
        let near = |i: usize| {
            posts
                .get(i)
                .is_some_and(|p| (f64::from(p.along) - along).abs() < SAME_PLACE)
        };
        if near(own) || (own > 0 && near(own - 1)) {
            continue;
        }
        let Some(at) = edges.on(side, along) else {
            continue;
        };
        inserts.push(Insert {
            along: on_side(side, along as f32).1,
            local: (at - edges.origin).as_vec3().to_array(),
        });
    }
    // Every place the side has a vertex once split, in order.
    let mut places: Vec<Post> = posts.clone();
    places.extend(inserts.iter().map(|i| Post {
        along: i.along,
        local: i.local,
    }));
    places.sort_by(|a, b| a.along.total_cmp(&b.along));
    let mut far = 0.0f64;
    let mut knots: Vec<Knot> = places
        .iter()
        .filter_map(|post| {
            let own = edges.at(post);
            let delta = line_at(target, f64::from(post.along))? - own;
            far = far.max(delta.length());
            Some(Knot {
                along: post.along,
                delta: delta.as_vec3().to_array(),
            })
        })
        .collect();
    if far < NOTHING && inserts.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // A run of places that do not move is said by its two ends.
    let still = |k: &Knot| k.delta == [0.0; 3];
    let kept: Vec<bool> = (0..knots.len())
        .map(|i| {
            i == 0
                || i + 1 == knots.len()
                || !(still(&knots[i - 1]) && still(&knots[i]) && still(&knots[i + 1]))
        })
        .collect();
    let mut keep = kept.iter();
    knots.retain(|_| keep.next().copied().unwrap_or(true));
    (knots, inserts)
}

/// A vertex this far under the highest at its place of a side, in metres, is
/// a skirt's foot. A mesh may state one place of its surface twice, a hair
/// apart; a skirt hangs metres.
const UNDER: f64 = 0.05;

/// For each vertex, where it is along `side` if it is the surface's there:
/// of the vertices at one place of a side the surface's is the furthest
/// from the Earth's centre, and the rest are a skirt's feet.
fn tops(positions: &[[f32; 3]], uvs: &[[f32; 2]], origin: DVec3, side: usize) -> Vec<Option<f32>> {
    let radius = |p: [f32; 3]| (origin + Vec3::from(p).as_dvec3()).length();
    let mut highest: HashMap<u32, f64> = HashMap::new();
    for (p, uv) in positions.iter().zip(uvs) {
        let (across, along) = place(side, *uv);
        if across.abs() < ON_THE_EDGE {
            let r = radius(*p);
            highest
                .entry(along.to_bits())
                .and_modify(|most| *most = most.max(r))
                .or_insert(r);
        }
    }
    positions
        .iter()
        .zip(uvs)
        .map(|(p, uv)| {
            let (across, along) = place(side, *uv);
            (across.abs() < ON_THE_EDGE
                && highest
                    .get(&along.to_bits())
                    .is_some_and(|most| radius(*p) > most - UNDER))
            .then_some(along)
        })
        .collect()
}

/// `mesh` without its skirts: every triangle that stands on a skirt's foot
/// is dropped. For measuring what stitching closes by itself — skirts are
/// the net under it, and a net hides what it catches.
pub fn without_skirts(mesh: &DecodedMesh, origin: DVec3) -> DecodedMesh {
    let mut out = mesh.clone();
    let Some(uvs) = &mesh.uvs else {
        return out;
    };
    let mut foot = vec![false; mesh.positions.len()];
    for side in 0..4 {
        let on_top = tops(&mesh.positions, uvs, origin, side);
        for (i, uv) in uvs.iter().enumerate() {
            if place(side, *uv).0.abs() < ON_THE_EDGE && on_top[i].is_none() {
                foot[i] = true;
            }
        }
    }
    out.indices = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|t| !t.iter().any(|i| foot[*i as usize]))
        .flatten()
        .copied()
        .collect();
    out
}

/// `mesh` with the vertices `inserts` ask for: every triangle with an edge
/// on a side — the surface's, and the skirt's hung from it — is cut at them,
/// fanned from its third vertex. The surface is where it was; it has more
/// triangles.
pub fn split(mesh: &DecodedMesh, origin: DVec3, inserts: &[Vec<Insert>; 4]) -> DecodedMesh {
    let mut out = mesh.clone();
    let Some(mut uvs) = out.uvs.take() else {
        return out;
    };
    for (side, wanted) in inserts.iter().enumerate() {
        if wanted.is_empty() {
            continue;
        }
        let mut wanted: Vec<Insert> = wanted.clone();
        wanted.sort_by(|a, b| a.along.total_cmp(&b.along));
        let on_top = tops(&out.positions, &uvs, origin, side);
        let mut made: Vec<Option<u32>> = vec![None; wanted.len()];
        let mut indices = Vec::with_capacity(out.indices.len() + 6 * wanted.len());
        for triangle in out.indices.clone().as_chunks::<3>().0 {
            let mut cut = false;
            for k in 0..3 {
                let (a, b, third) = (triangle[k], triangle[(k + 1) % 3], triangle[(k + 2) % 3]);
                let (Some(from), Some(to)) = (
                    on_top.get(a as usize).copied().flatten(),
                    on_top.get(b as usize).copied().flatten(),
                ) else {
                    continue;
                };
                let (low, high) = (from.min(to), from.max(to));
                let first = wanted.partition_point(|i| i.along <= low);
                let last = wanted.partition_point(|i| i.along < high);
                if first >= last {
                    continue;
                }
                let mut chain = vec![a];
                let mut between: Vec<usize> = (first..last).collect();
                if from > to {
                    between.reverse();
                }
                for i in between {
                    let index = *made[i].get_or_insert_with(|| {
                        let insert = wanted[i];
                        let index = out.positions.len() as u32;
                        out.positions.push(insert.local);
                        uvs.push(on_side(side, insert.along).0);
                        if let Some(normals) = out.normals.as_mut() {
                            let t = (insert.along - from) / (to - from);
                            let (na, nb) = (
                                Vec3::from(normals[a as usize]),
                                Vec3::from(normals[b as usize]),
                            );
                            normals.push(na.lerp(nb, t).normalize_or_zero().to_array());
                        }
                        index
                    });
                    chain.push(index);
                }
                chain.push(b);
                for pair in chain.windows(2) {
                    indices.extend_from_slice(&[pair[0], pair[1], third]);
                }
                cut = true;
                break;
            }
            if !cut {
                indices.extend_from_slice(triangle);
            }
        }
        out.indices = indices;
    }
    out.uvs = Some(uvs);
    out
}

/// `content` as a renderer that stitches draws it: [`split`], then every
/// vertex moved by [`Stitch::offset`] times `amount` — 1 for the whole
/// correction. The shaders' arithmetic, on the CPU.
pub fn displaced(
    content: &DecodedTileContent,
    stitch: &Stitch,
    band: f32,
    amount: f32,
) -> DecodedTileContent {
    let mut out = content.clone();
    if stitch.is_nothing() || content.transform_local != Mat4::IDENTITY {
        return out;
    }
    for mesh in &mut out.meshes {
        *mesh = split(mesh, content.local_origin_ecef, &stitch.inserts);
        let Some(uvs) = &mesh.uvs else { continue };
        for (p, uv) in mesh.positions.iter_mut().zip(uvs) {
            *p = (Vec3::from(*p) + stitch.offset(*uv, band) * amount).to_array();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::MaterialDesc;
    use crate::geo::{geodetic_to_ecef, Geodetic};
    use crate::seam::{residuals, Drawn, Tolerance};
    use std::f64::consts::PI;

    /// A made-up relief, different at every level so that two levels do not
    /// state the same ground: metres apart, as a source's are.
    fn relief(level: u32, lon: f64, lat: f64) -> f64 {
        let (x, y) = (lon * 6.4e6 / 300.0, lat * 6.4e6 / 300.0);
        800.0
            + 200.0 * (x * 0.31).sin() * (y * 0.27).cos()
            + 6.0 * ((x * 1.7 + f64::from(level) * 2.1).sin() + (y * 1.3 - f64::from(level)).cos())
    }

    /// Tile `level/x/y` as a grid of `n` spacings a side over [`relief`],
    /// with a skirt `skirt` metres deep on every side if asked.
    fn tile(level: u32, x: u64, y: u64, n: usize, skirt: Option<f64>) -> DecodedTileContent {
        tile_over(level, x, y, n, skirt, &|lon, lat| relief(level, lon, lat))
    }

    fn tile_over(
        level: u32,
        x: u64,
        y: u64,
        n: usize,
        skirt: Option<f64>,
        height: &dyn Fn(f64, f64) -> f64,
    ) -> DecodedTileContent {
        let size = PI / 2f64.powi(level as i32);
        let (west, south) = (-PI + x as f64 * size, -PI / 2.0 + y as f64 * size);
        let at = |u: f64, north: f64, drop: f64| {
            let (lon, lat) = (west + u * size, south + north * size);
            geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: height(lon, lat) - drop,
            })
        };
        let origin = at(0.5, 0.5, 0.0);
        let mut positions = Vec::new();
        let mut uvs = Vec::new();
        let mut indices = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let (u, north) = (i as f64 / n as f64, j as f64 / n as f64);
                positions.push((at(u, north, 0.0) - origin).as_vec3().to_array());
                uvs.push([u as f32, 1.0 - north as f32]);
            }
        }
        let row = n as u32 + 1;
        for j in 0..n as u32 {
            for i in 0..n as u32 {
                let a = j * row + i;
                indices.extend_from_slice(&[a, a + 1, a + row, a + row, a + 1, a + row + 1]);
            }
        }
        if let Some(depth) = skirt {
            let on_side = |side: usize, k: usize| -> (usize, usize) {
                match side {
                    0 => (0, k),
                    1 => (k, 0),
                    2 => (n, k),
                    _ => (k, n),
                }
            };
            for side in 0..4 {
                let base = positions.len() as u32;
                for k in 0..=n {
                    let (i, j) = on_side(side, k);
                    let (u, north) = (i as f64 / n as f64, j as f64 / n as f64);
                    positions.push((at(u, north, depth) - origin).as_vec3().to_array());
                    uvs.push([u as f32, 1.0 - north as f32]);
                }
                for k in 0..n as u32 {
                    let top = |k: u32| {
                        let (i, j) = on_side(side, k as usize);
                        j as u32 * row + i as u32
                    };
                    indices.extend_from_slice(&[
                        top(k),
                        top(k + 1),
                        base + k,
                        base + k,
                        top(k + 1),
                        base + k + 1,
                    ]);
                }
            }
        }
        DecodedTileContent {
            withheld_drape: None,
            meshes: vec![DecodedMesh {
                positions,
                normals: None,
                uvs: Some(uvs),
                indices,
                material: MaterialDesc::default(),
            }],
            textures: Vec::new(),
            imagery: Vec::new(),
            local_origin_ecef: origin,
            transform_local: Mat4::IDENTITY,
        }
    }

    type Made = (u32, u64, u64, DecodedTileContent);

    fn stitched_with(made: &[Made], band: f32, mutate: &dyn Fn(&mut Stitch)) -> Vec<Made> {
        let edges: Vec<Edges> = made.iter().map(|m| Edges::of(&m.3)).collect();
        let tiles: Vec<Tile<'_>> = made
            .iter()
            .zip(&edges)
            .map(|(m, edges)| Tile {
                level: m.0,
                x: m.1,
                y: m.2,
                source: m.0,
                edges,
            })
            .collect();
        made.iter()
            .zip(plan(&tiles, (2, 1), band))
            .map(|(m, mut stitch)| {
                mutate(&mut stitch);
                (m.0, m.1, m.2, displaced(&m.3, &stitch, band, 1.0))
            })
            .collect()
    }

    fn stitched(made: &[Made]) -> Vec<Made> {
        stitched_with(made, BAND, &|_| {})
    }

    /// The largest residual among the tiles' shared edges, in metres, and
    /// how many edges they share.
    fn left(made: &[Made]) -> (f64, usize) {
        let drawn: Vec<Drawn<'_>> = made
            .iter()
            .map(|m| Drawn {
                level: m.0,
                x: m.1,
                y: m.2,
                content: &m.3,
            })
            .collect();
        let found = residuals(&drawn, (2, 1), None, Tolerance::Metres(0.001));
        (found.metres[0], found.shared)
    }

    /// Level 14 near 45° north: tiles of about a kilometre.
    const X: u64 = 9000;
    const Y: u64 = 6200;

    /// A level-14 tile and the two level-15 tiles along its east side.
    fn one_level_apart() -> Vec<Made> {
        vec![
            (14, X, Y, tile(14, X, Y, 8, Some(40.0))),
            (
                15,
                2 * X + 2,
                2 * Y,
                tile(15, 2 * X + 2, 2 * Y, 8, Some(20.0)),
            ),
            (
                15,
                2 * X + 2,
                2 * Y + 1,
                tile(15, 2 * X + 2, 2 * Y + 1, 8, Some(20.0)),
            ),
        ]
    }

    #[test]
    fn two_tiles_a_level_apart_are_put_on_one_line() {
        let made = one_level_apart();
        let (before, shared) = left(&made);
        assert_eq!(
            shared, 3,
            "the coarse tile and each fine one, and the fine pair"
        );
        assert!(before > 2.0, "the levels part by metres: {before}");
        let (after, _) = left(&stitched(&made));
        assert!(after < 1.0e-2, "what is left is rounding: {after} m");
    }

    #[test]
    fn the_coarser_tile_keeps_its_edge() {
        let made = one_level_apart();
        let after = stitched(&made);
        let (was, is) = (
            &made[0].3.meshes[0].positions,
            &after[0].3.meshes[0].positions,
        );
        assert_eq!(
            *was,
            is[..was.len()],
            "the master's vertices are where they were"
        );
        // What it gains is the fine tiles' vertices, on its own segments:
        // the two edges are the same segments, not a vertex of one against
        // the middle of a segment of the other.
        assert!(is.len() > was.len());
        let (coarse, fine) = (Edges::of(&after[0].3), Edges::of(&after[1].3));
        let south_half = coarse.sides[2].iter().filter(|p| p.along <= 0.5).count();
        assert_eq!(south_half, fine.sides[0].len());
        assert_ne!(
            made[1].3.meshes[0].positions,
            after[1].3.meshes[0].positions
        );
    }

    #[test]
    fn tiles_several_levels_apart_are_put_on_one_line() {
        // A level-13 tile, and along its east side one level-14 tile and,
        // north of it, two of level 16 and one of level 15.
        let (x, y) = (X / 2, Y / 2);
        let mut made = vec![
            (13, x, y, tile(13, x, y, 6, Some(80.0))),
            (14, 2 * x + 2, 2 * y, tile(14, 2 * x + 2, 2 * y, 8, None)),
            (
                15,
                4 * x + 4,
                4 * y + 2,
                tile(15, 4 * x + 4, 4 * y + 2, 8, None),
            ),
        ];
        for k in 0..2 {
            let (fx, fy) = (8 * x + 8, 8 * y + 6 + k);
            made.push((16, fx, fy, tile(16, fx, fy, 5, Some(10.0))));
        }
        let (before, shared) = left(&made);
        assert_eq!(shared, 7);
        assert!(before > 2.0, "{before}");
        let (after, _) = left(&stitched(&made));
        assert!(after < 1.0e-2, "{after} m");
    }

    #[test]
    fn where_four_tiles_meet_the_coarsest_has_the_ground() {
        // Around one point: a level-14 tile to the south-west, level-15
        // tiles to the south-east and north-west, a level-16 to the
        // north-east.
        let (cx, cy) = (X + 1, Y + 1);
        let made = vec![
            (14, cx - 1, cy - 1, tile(14, cx - 1, cy - 1, 8, None)),
            (
                15,
                2 * cx,
                2 * cy - 1,
                tile(15, 2 * cx, 2 * cy - 1, 8, None),
            ),
            (
                15,
                2 * cx - 1,
                2 * cy,
                tile(15, 2 * cx - 1, 2 * cy, 8, None),
            ),
            (16, 4 * cx, 4 * cy, tile(16, 4 * cx, 4 * cy, 8, None)),
        ];
        let corner = |m: &Made, uv: [f32; 2]| {
            let mesh = &m.3.meshes[0];
            let i = mesh
                .uvs
                .as_ref()
                .expect("uvs")
                .iter()
                .position(|at| *at == uv)
                .expect("a corner vertex");
            m.3.local_origin_ecef + Vec3::from(mesh.positions[i]).as_dvec3()
        };
        let at = |made: &[Made]| {
            [
                corner(&made[0], [1.0, 0.0]),
                corner(&made[1], [0.0, 0.0]),
                corner(&made[2], [1.0, 1.0]),
                corner(&made[3], [0.0, 1.0]),
            ]
        };
        let before = at(&made);
        assert!((before[0] - before[3]).length() > 1.0);
        let after = at(&stitched(&made));
        assert_eq!(after[0], before[0], "the coarsest stays");
        for other in &after[1..] {
            assert!((*other - after[0]).length() < 1.0e-2, "{after:?}");
        }
        assert!(left(&stitched(&made)).0 < 1.0e-2);
        // The rule by its absence: the corner left to each side's master
        // leaves the level-16 tile on two lines that end at two heights.
        let broken = stitched_with(&made, BAND, &|stitch| {
            for knots in &mut stitch.sides {
                if let Some(first) = knots.first_mut() {
                    first.delta = [0.0; 3];
                }
            }
        });
        assert!(left(&broken).0 > 0.5, "{}", left(&broken).0);
    }

    #[test]
    fn a_corner_against_the_middle_of_a_moved_segment_is_on_it() {
        // Three levels around one point: a level-13 tile west; east of it
        // a level-14 tile of two triangles, whose north-west corner —
        // moved onto the level-13 tile — takes the whole of its north
        // side with it; and north of that side two level-15 tiles, whose
        // shared corner falls in its middle. The point they meet at is on
        // the side as it is drawn, not as its mesh had it.
        let (x, y) = (X / 2, Y / 2);
        let made = vec![
            (13, x, y, tile(13, x, y, 5, None)),
            (14, 2 * x + 2, 2 * y, tile(14, 2 * x + 2, 2 * y, 1, None)),
            (
                15,
                4 * x + 4,
                4 * y + 2,
                tile(15, 4 * x + 4, 4 * y + 2, 4, None),
            ),
            (
                15,
                4 * x + 5,
                4 * y + 2,
                tile(15, 4 * x + 5, 4 * y + 2, 4, None),
            ),
        ];
        assert!(left(&made).0 > 2.0);
        let (after, shared) = left(&stitched(&made));
        assert_eq!(shared, 5);
        assert!(after < 1.0e-2, "{after} m");
    }

    #[test]
    fn a_side_with_nothing_across_it_stays_where_it_is() {
        let made = one_level_apart();
        let edges: Vec<Edges> = made.iter().map(|m| Edges::of(&m.3)).collect();
        let tiles: Vec<Tile<'_>> = made
            .iter()
            .zip(&edges)
            .map(|(m, edges)| Tile {
                level: m.0,
                x: m.1,
                y: m.2,
                source: m.0,
                edges,
            })
            .collect();
        let plans = plan(&tiles, (2, 1), BAND);
        // The southern fine tile: the coarse tile west of it, its sibling
        // north, nothing south or east.
        assert_eq!(plans[1].across, [Some(14), None, None, Some(15)]);
        assert!(!plans[1].sides[0].is_empty());
        assert!(plans[1].sides[2].is_empty());
        // Its south side moves at the one corner the coarse tile has.
        let south = &plans[1].sides[1];
        assert!(south.first().is_some_and(|k| k.delta != [0.0; 3]));
        // …fading over the band, and not at all past it.
        assert!(
            south
                .iter()
                .all(|k| (k.delta == [0.0; 3]) == (k.along >= BAND)),
            "{south:?}"
        );
        // A vertex on the east side, away from every corrected side, is
        // where it was; so is one in the middle.
        for uv in [[1.0, 0.5], [0.5, 0.5], [0.75, 0.3]] {
            assert_eq!(plans[1].offset(uv, BAND), Vec3::ZERO, "{uv:?}");
        }
    }

    #[test]
    fn a_tile_cut_from_coarser_data_gains_its_neighbours_vertices() {
        // West, a tile with many vertices on its edge; east, one of the
        // same level with three — as a tile cut from an ancestor has — and a
        // surface of its own.
        let made = vec![
            (15, 2 * X, 2 * Y, tile(15, 2 * X, 2 * Y, 16, Some(20.0))),
            (
                15,
                2 * X + 1,
                2 * Y,
                tile_over(15, 2 * X + 1, 2 * Y, 2, Some(40.0), &|lon, lat| {
                    relief(14, lon, lat)
                }),
            ),
        ];
        assert!(left(&made).0 > 2.0);
        let after = stitched(&made);
        assert!(left(&after).0 < 1.0e-2, "{} m", left(&after).0);
        // The tile to the east, further north-east in the order, is the one
        // that moves: it has its neighbour's seventeen vertices on that side.
        let east = Edges::of(&after[1].3);
        assert_eq!(east.sides[0].len(), 17);
        assert_eq!(east.sides[2].len(), 3);
        // Without the vertices its edge is on the line only where it had
        // some: the line's bends stand off it.
        let unsplit = stitched_with(&made, BAND, &|stitch| stitch.inserts = Default::default());
        assert!(left(&unsplit).0 > 0.5, "{}", left(&unsplit).0);
    }

    #[test]
    fn the_level_of_the_data_breaks_a_tie_of_levels() {
        let content = tile(15, 2 * X, 2 * Y, 4, None);
        let other = tile_over(15, 2 * X + 1, 2 * Y, 4, None, &|lon, lat| {
            relief(14, lon, lat)
        });
        let (a, b) = (Edges::of(&content), Edges::of(&other));
        let tiles = |source_east: u32| {
            [
                Tile {
                    level: 15,
                    x: 2 * X,
                    y: 2 * Y,
                    source: 15,
                    edges: &a,
                },
                Tile {
                    level: 15,
                    x: 2 * X + 1,
                    y: 2 * Y,
                    source: source_east,
                    edges: &b,
                },
            ]
        };
        // Told nothing, the western tile is first by place and keeps its edge.
        let plans = plan(&tiles(15), (2, 1), BAND);
        assert!(plans[0].is_nothing() && !plans[1].is_nothing());
        // Told the eastern one is from level-14 data, that one keeps its.
        let plans = plan(&tiles(14), (2, 1), BAND);
        assert!(!plans[0].is_nothing() && plans[1].is_nothing());
    }

    #[test]
    fn a_tile_of_two_triangles_is_a_master_like_any_other() {
        // The sea: four vertices, flat — an edge of its two corners — beside
        // relief two levels finer.
        let (x, y) = (X / 2, Y / 2);
        let mut made = vec![(13, x, y, tile_over(13, x, y, 1, None, &|_, _| 52.0))];
        for k in 0..4 {
            let (fx, fy) = (4 * x + 4, 4 * y + k);
            made.push((15, fx, fy, tile(15, fx, fy, 6, Some(20.0))));
        }
        assert!(left(&made).0 > 100.0);
        let after = stitched(&made);
        assert!(left(&after).0 < 1.0e-2, "{} m", left(&after).0);
        // The sea is as flat as it was: it has the relief's vertices on its
        // side, each on the straight line between its two corners.
        let sea = &after[0].3.meshes[0].positions;
        assert_eq!(made[0].3.meshes[0].positions, sea[..4]);
        let edges = Edges::of(&after[0].3);
        let (first, last) = (edges.sides[2][0], edges.sides[2][edges.sides[2].len() - 1]);
        let (from, to) = (Vec3::from(first.local), Vec3::from(last.local));
        for post in &edges.sides[2] {
            let off = (Vec3::from(post.local) - from)
                .cross((to - from).normalize())
                .length();
            assert!(off < 1.0e-2, "{off} m off the sea's edge");
        }
        assert_eq!(edges.sides[2].len(), 4 * 6 + 1);
    }

    #[test]
    fn the_plan_does_not_depend_on_the_order_tiles_come_in() {
        let made = one_level_apart();
        let mut turned = made.clone();
        turned.reverse();
        let mut again = stitched(&turned);
        again.reverse();
        for (a, b) in stitched(&made).iter().zip(&again) {
            assert_eq!(a.3.meshes[0].positions, b.3.meshes[0].positions);
            assert_eq!(a.3.meshes[0].indices, b.3.meshes[0].indices);
        }
    }

    #[test]
    fn splitting_adds_triangles_and_moves_nothing() {
        let made = one_level_apart();
        let edges: Vec<Edges> = made.iter().map(|m| Edges::of(&m.3)).collect();
        let tiles: Vec<Tile<'_>> = made
            .iter()
            .zip(&edges)
            .map(|(m, edges)| Tile {
                level: m.0,
                x: m.1,
                y: m.2,
                source: m.0,
                edges,
            })
            .collect();
        let plans = plan(&tiles, (2, 1), BAND);
        // The fine tile's west side has 9 vertices over half the coarse
        // side's 9: every other coarse vertex is one it has, so none to add.
        assert!(plans[1].inserts.iter().all(Vec::is_empty));
        // A coarse side of 5 spacings shares none but the ends.
        let coarse = tile(14, X, Y, 5, Some(40.0));
        let edges_coarse = Edges::of(&coarse);
        let mut odd = tiles.clone();
        odd[0].edges = &edges_coarse;
        let plans = plan(&odd, (2, 1), BAND);
        let wanted = plans[1].inserts[0].len();
        assert_eq!(wanted, 2, "{:?}", plans[1].inserts[0]);
        let mesh = &made[1].3.meshes[0];
        let cut = split(mesh, made[1].3.local_origin_ecef, &plans[1].inserts);
        // A surface triangle and a skirt triangle at each.
        assert_eq!(cut.indices.len(), mesh.indices.len() + 6 * wanted);
        assert_eq!(cut.positions.len(), mesh.positions.len() + wanted);
        assert_eq!(cut.positions[..mesh.positions.len()], mesh.positions[..]);
        let area = |m: &DecodedMesh| -> f32 {
            m.indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| {
                    let p = |i: u32| Vec3::from(m.positions[i as usize]);
                    (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0])).length() / 2.0
                })
                .sum()
        };
        assert!((area(&cut) - area(mesh)).abs() < 1.0e-3 * area(mesh));
    }

    /// A level step and nothing else: flat ground at 100 m, a level-14
    /// tile and, east of it, two level-15 tiles of sixteen spacings 3 m
    /// higher.
    fn a_step() -> Vec<Made> {
        let fine = |y: u64| {
            let x = 2 * X + 2;
            (15, x, y, tile_over(15, x, y, 16, None, &|_, _| 103.0))
        };
        vec![
            (14, X, Y, tile_over(14, X, Y, 8, None, &|_, _| 100.0)),
            fine(2 * Y),
            fine(2 * Y + 1),
        ]
    }

    /// The steepest any triangle of the fine tile turned, in radians, when
    /// stitched with `band`.
    fn hinge(band: f32) -> f32 {
        let made = a_step();
        let after = stitched_with(&made, band, &|_| {});
        let normals = |m: &DecodedMesh| -> Vec<Vec3> {
            m.indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| {
                    let p = |i: u32| Vec3::from(m.positions[i as usize]);
                    (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0])).normalize()
                })
                .collect()
        };
        normals(&made[1].3.meshes[0])
            .iter()
            .zip(&normals(&after[1].3.meshes[0]))
            .map(|(a, b)| a.angle_between(*b))
            .fold(0.0, f32::max)
    }

    #[test]
    fn a_band_spreads_the_hinge() {
        // With next to no band only the edge row moves: its triangles turn
        // by the step over one row of the mesh, 3 m over 38 m. Over the
        // band they turn by the step over a quarter of the tile.
        let (sharp, spread) = (hinge(1.0e-3), hinge(BAND));
        assert!(sharp > 0.07, "a crease: {sharp} rad");
        assert!(spread < 0.3 * sharp, "{spread} against {sharp} rad");
        // Past the band nothing has moved.
        let made = one_level_apart();
        let after = stitched(&made);
        let (was, is) = (&made[1].3.meshes[0], &after[1].3.meshes[0]);
        for (i, uv) in was.uvs.as_ref().expect("uvs").iter().enumerate() {
            let inside = uv[0] > BAND && uv[0] < 1.0 - BAND && uv[1] > BAND && uv[1] < 1.0 - BAND;
            if inside {
                assert_eq!(was.positions[i], is.positions[i]);
            }
        }
    }

    #[test]
    fn a_skirt_goes_with_its_edge() {
        let made = one_level_apart();
        let after = stitched(&made);
        let (was, is) = (&made[1].3.meshes[0], &after[1].3.meshes[0]);
        let uvs = was.uvs.as_ref().expect("uvs");
        // Every vertex at one place of the west side moved by one vector.
        let mut moved: HashMap<u32, Vec3> = HashMap::new();
        for (i, uv) in uvs.iter().enumerate().filter(|(_, uv)| uv[0] == 0.0) {
            let by = Vec3::from(is.positions[i]) - Vec3::from(was.positions[i]);
            let first = *moved.entry(uv[1].to_bits()).or_insert(by);
            assert!((first - by).length() < 1.0e-2, "{first} and {by}");
        }
        assert_eq!(moved.len(), 9);
    }

    #[test]
    fn a_mesh_is_left_its_surface_when_its_skirts_go() {
        let with = tile(14, X, Y, 8, Some(40.0));
        let bare = tile(14, X, Y, 8, None);
        let cut = without_skirts(&with.meshes[0], with.local_origin_ecef);
        assert_eq!(cut.indices, bare.meshes[0].indices);
    }

    #[test]
    fn a_place_stated_twice_a_hair_apart_is_no_skirt() {
        // A mesh cut from another may hold two vertices at one place of a
        // side, a rounding apart. Neither is a foot: the triangles on the
        // lower one are surface, kept when skirts go and cut when the side
        // gains a vertex.
        let mut twice = tile(14, X, Y, 4, None);
        let mesh = &mut twice.meshes[0];
        // The vertex at the middle of the west side, again, a millimetre
        // lower along the tile's up, used by one of its triangles.
        let middle = 2 * 5;
        let up = twice.local_origin_ecef.normalize().as_vec3();
        let lower = (Vec3::from(mesh.positions[middle]) - up * 1.0e-3).to_array();
        mesh.positions.push(lower);
        let uv = mesh.uvs.as_ref().expect("uvs")[middle];
        mesh.uvs.as_mut().expect("uvs").push(uv);
        let again = mesh.positions.len() as u32 - 1;
        let used = mesh
            .indices
            .iter()
            .position(|i| *i == middle as u32)
            .expect("used");
        mesh.indices[used] = again;
        let bare = without_skirts(mesh, twice.local_origin_ecef);
        assert_eq!(
            bare.indices.len(),
            mesh.indices.len(),
            "a surface triangle was taken for a skirt"
        );
    }

    #[test]
    fn a_tile_says_where_it_is_by_its_corners() {
        for (level, x, y) in [
            (14, X, Y),
            (16, 4 * X + 3, 4 * Y + 1),
            (3, 15, 2),
            (0, 1, 0),
        ] {
            let edges = Edges::of(&tile_over(level, x, y, 4, Some(10.0), &|_, _| 100.0));
            assert_eq!(edges.cell(), Some((level, x, y)));
        }
        // Half a tile is not a tile.
        let mut half = tile(14, X, Y, 4, None);
        half.meshes[0]
            .uvs
            .as_mut()
            .expect("uvs")
            .iter_mut()
            .for_each(|uv| uv[0] *= 0.5);
        assert_eq!(Edges::of(&half).cell(), None);
    }

    #[test]
    fn the_strips_pack_as_a_shader_reads_them() {
        let made = one_level_apart();
        let edges: Vec<Edges> = made.iter().map(|m| Edges::of(&m.3)).collect();
        let tiles: Vec<Tile<'_>> = made
            .iter()
            .zip(&edges)
            .map(|(m, edges)| Tile {
                level: m.0,
                x: m.1,
                y: m.2,
                source: m.0,
                edges,
            })
            .collect();
        let stitch = &plan(&tiles, (2, 1), BAND)[2];
        let rows = stitch.packed();
        let knots: usize = stitch.sides.iter().map(Vec::len).sum();
        assert_eq!(rows.len(), 10 + knots);
        assert_eq!(rows[1][0], 10.0);
        assert_eq!(rows[0][0] as usize, stitch.sides[0].len());
        let first = stitch.sides[0][0];
        assert_eq!(
            rows[10],
            [first.along, first.delta[0], first.delta[1], first.delta[2]]
        );
        let none = Stitch::default().packed();
        assert_eq!((none.len(), none[0]), (10, [0.0; 4]));
    }
}
