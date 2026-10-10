// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Whether the ground is closed where two tiles meet.
//!
//! Two tiles that share a stretch of edge each state the ground along it,
//! from their own mesh. Where they state it at two heights there is a step,
//! and a step is a hole — the far side of the planet seen through the ground
//! — unless one of the two hangs a wall across it. Whether one does is a
//! question about the geometry the engine hands out, the same for every
//! renderer, so it is asked here: of [`DecodedTileContent`], with no picture.
//!
//! [`seam`] answers it by casting rays. Along the shared stretch, at each of
//! several heights inside the step, a short segment is laid across the line
//! from one tile into the other; a segment that meets no triangle of either
//! tile is a ray that passes between them.
//!
//! What it is told by the tiles themselves: which vertices lie on an edge, by
//! their texture coordinates — a terrain mesh carries its place in its tile
//! there (`u` eastward, `v` southward), and a skirt's vertices carry the
//! place of the edge they hang from.

use glam::DVec3;

use crate::content::DecodedTileContent;

/// A side of a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    West,
    South,
    East,
    North,
}

/// One tile where it meets another: which side, and the stretch of that
/// side the other covers — from 0 to 1 along it, northward along a west or
/// east side, eastward along a south or north one.
#[derive(Debug, Clone, Copy)]
pub struct Meeting<'a> {
    pub content: &'a DecodedTileContent,
    pub side: Side,
    pub span: (f64, f64),
}

impl<'a> Meeting<'a> {
    /// The whole of `side`.
    pub fn whole(content: &'a DecodedTileContent, side: Side) -> Self {
        Self {
            content,
            side,
            span: (0.0, 1.0),
        }
    }
}

/// What [`seam`] found along a shared stretch of edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seam {
    /// The stretch's length, in metres.
    pub length: f64,
    /// The largest step between the two surfaces, in metres.
    pub step: f64,
    /// The largest step a ray passed through, in metres: 0 for a closed seam.
    pub open: f64,
    /// Rays cast, and how many of them met nothing.
    pub rays: usize,
    pub through: usize,
}

impl Seam {
    /// No ray passes between the two tiles.
    pub fn is_closed(&self) -> bool {
        self.through == 0
    }
}

/// A place on an edge: where along it, and the surface there.
#[derive(Clone, Copy)]
struct Post {
    along: f64,
    top: DVec3,
}

/// How near an edge a texture coordinate has to be to be on it.
const ON_THE_EDGE: f64 = 1.0e-6;

fn triangles(content: &DecodedTileContent) -> Vec<[DVec3; 3]> {
    let place = |p: [f32; 3]| {
        content.local_origin_ecef
            + content
                .transform_local
                .transform_point3(p.into())
                .as_dvec3()
    };
    content
        .meshes
        .iter()
        .flat_map(|mesh| {
            mesh.indices.as_chunks::<3>().0.iter().map(move |t| {
                [
                    place(mesh.positions[t[0] as usize]),
                    place(mesh.positions[t[1] as usize]),
                    place(mesh.positions[t[2] as usize]),
                ]
            })
        })
        .collect()
}

/// The surface along one side, south to north or west to east. Of the
/// vertices that share a place on the edge — the surface's and its skirt's
/// — the surface's is the one furthest from the Earth's centre.
fn posts(content: &DecodedTileContent, side: Side) -> Vec<Post> {
    let mut on: Vec<Post> = Vec::new();
    for mesh in &content.meshes {
        let Some(uvs) = &mesh.uvs else { continue };
        for (p, uv) in mesh.positions.iter().zip(uvs) {
            let (u, north) = (f64::from(uv[0]), 1.0 - f64::from(uv[1]));
            let (across, along) = match side {
                Side::West => (u, north),
                Side::East => (1.0 - u, north),
                Side::South => (north, u),
                Side::North => (1.0 - north, u),
            };
            if across.abs() < ON_THE_EDGE {
                let top = content.local_origin_ecef
                    + content
                        .transform_local
                        .transform_point3((*p).into())
                        .as_dvec3();
                on.push(Post { along, top });
            }
        }
    }
    on.sort_by(|a, b| a.along.total_cmp(&b.along));
    let mut kept: Vec<Post> = Vec::new();
    for post in on {
        match kept.last_mut() {
            Some(last) if post.along - last.along < ON_THE_EDGE => {
                if post.top.length_squared() > last.top.length_squared() {
                    last.top = post.top;
                }
            }
            _ => kept.push(post),
        }
    }
    kept
}

/// The surface at `along` an edge: on the line between the two posts
/// around it, which is where the mesh's own edge is.
fn at(posts: &[Post], along: f64) -> Option<DVec3> {
    let (first, last) = (posts.first()?, posts.last()?);
    if along <= first.along {
        return Some(first.top);
    }
    if along >= last.along {
        return Some(last.top);
    }
    let next = posts.partition_point(|p| p.along <= along);
    let (a, b) = (posts[next - 1], posts[next]);
    Some(a.top.lerp(
        b.top,
        (along - a.along) / (b.along - a.along).max(f64::MIN_POSITIVE),
    ))
}

/// Whether the segment from `from` to `to` meets the triangle.
fn meets(from: DVec3, to: DVec3, [a, b, c]: [DVec3; 3]) -> bool {
    let d = to - from;
    let (e1, e2) = (b - a, c - a);
    let p = d.cross(e2);
    let det = e1.dot(p);
    // A triangle of no area, or one the segment runs along, is met by nothing.
    if det.abs() < 1.0e-12 * e1.length() * e2.length() * d.length() {
        return false;
    }
    let inv = 1.0 / det;
    let s = from - a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    (0.0..=1.0).contains(&(e2.dot(q) * inv))
}

/// Casts rays through every step between `a` and `b` along the stretch of
/// edge they share, `places` of them along it, and says what passed.
///
/// A segment is as long as a skirt may lean: a thousandth of the longer of
/// the two edges to each side of the line.
pub fn seam(a: Meeting<'_>, b: Meeting<'_>, places: usize) -> Seam {
    /// Heights a ray is cast at, as fractions of the step.
    const HEIGHTS: [f64; 5] = [0.1, 0.3, 0.5, 0.7, 0.9];
    /// A step under this is two statements of one height.
    const NO_STEP: f64 = 1.0e-3;

    let (posts_a, posts_b) = (posts(a.content, a.side), posts(b.content, b.side));
    let mut found = Seam {
        length: 0.0,
        step: 0.0,
        open: 0.0,
        rays: 0,
        through: 0,
    };
    let edge = |posts: &[Post]| match (posts.first(), posts.last()) {
        (Some(first), Some(last)) => (first.top - last.top).length(),
        _ => 0.0,
    };
    let reach = 1.0e-3 * edge(&posts_a).max(edge(&posts_b)) + 0.05;
    let mut all = triangles(a.content);
    all.extend(triangles(b.content));
    let on = |span: (f64, f64), k: f64| span.0 + (span.1 - span.0) * k;
    let places = places.max(1);
    let mut before: Option<DVec3> = None;
    for place in 0..places {
        let k = (place as f64 + 0.5) / places as f64;
        let (Some(pa), Some(pb)) = (at(&posts_a, on(a.span, k)), at(&posts_b, on(b.span, k)))
        else {
            continue;
        };
        if let Some(before) = before {
            found.length += (pa - before).length();
        }
        // Along the edge, by where it is a little further on.
        let ahead = at(&posts_a, on(a.span, k + 0.5 / places as f64)).unwrap_or(pa);
        let behind = at(&posts_a, on(a.span, k - 0.5 / places as f64)).unwrap_or(pa);
        before = Some(pa);
        let up = pa.normalize();
        let Some(across) = (ahead - behind).cross(up).try_normalize() else {
            continue;
        };
        let step = (pa - pb).dot(up);
        found.step = found.step.max(step.abs());
        if step.abs() < NO_STEP {
            continue;
        }
        let lower = if step > 0.0 { pb } else { pa };
        for height in HEIGHTS {
            let through = lower + up * (height * step.abs());
            let (from, to) = (through - across * reach, through + across * reach);
            found.rays += 1;
            if !all.iter().any(|t| meets(from, to, *t)) {
                found.through += 1;
                found.open = found.open.max(step.abs());
            }
        }
    }
    found
}

/// One tile a frame draws: where it is in the quadtree, and its content.
#[derive(Debug, Clone, Copy)]
pub struct Drawn<'a> {
    pub level: u32,
    pub x: u64,
    /// Counted from the south.
    pub y: u64,
    pub content: &'a DecodedTileContent,
}

/// Where a frame is looked at from, to state a residual in pixels.
#[derive(Debug, Clone, Copy)]
pub struct Eye {
    pub position: DVec3,
    /// Pixels a metre covers a metre away: the picture's height over
    /// `2·tan(fovy / 2)`.
    pub pixels_per_radian: f64,
}

/// What is left between two tiles along the stretch of edge they share:
/// the furthest any vertex of either edge stands from the other's edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Residual {
    /// The two tiles, level, x, y: `a` west or south of `b`.
    pub a: (u32, u64, u64),
    pub b: (u32, u64, u64),
    /// Across a meridian (`a` west of `b`) or across a parallel.
    pub meridian: bool,
    /// The stretch's length, in metres.
    pub length: f64,
    /// The residual, in metres, and its parts: along the vertical, and
    /// across it.
    pub metres: f64,
    pub height: f64,
    pub horizontal: f64,
    /// Where it is, in the Earth's frame.
    pub at: DVec3,
    /// The same in pixels, when an eye was given: 0 otherwise.
    pub pixels: f64,
}

/// A frame's residuals, summed up. See [`residuals`].
#[derive(Debug, Clone, PartialEq)]
pub struct Residuals {
    /// Stretches of edge two drawn tiles share.
    pub shared: usize,
    /// …of which those whose residual is over the tolerance.
    pub over: usize,
    /// …and those between two tiles of one level, with their largest
    /// residual in metres: the pairs that should agree to rounding.
    pub same_level: usize,
    pub same_level_max: f64,
    /// Edges, or parts of edges, that no drawn tile lies across: ground with
    /// a border onto nothing. A different fault from a residual, counted
    /// apart, with the length left bare in metres.
    pub uncovered: usize,
    pub uncovered_metres: f64,
    /// Each of them: the tile, the middle of its edge, and the length left
    /// bare — for a caller that knows what is in view, and what is not is
    /// the selection's border, not a fault.
    pub bare: Vec<Bare>,
    /// Over the shared stretches: the largest, the median and the 99th
    /// centile, in metres and — with an eye — in pixels.
    pub metres: [f64; 3],
    pub pixels: [f64; 3],
    /// Every shared stretch, the worst first (by pixels with an eye, by
    /// metres without).
    pub pairs: Vec<Residual>,
}

/// An edge, or part of one, that no drawn tile lies across.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bare {
    pub tile: (u32, u64, u64),
    pub side: Side,
    /// The middle of the edge, in the Earth's frame.
    pub at: DVec3,
    pub metres: f64,
}

/// What a residual is held against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tolerance {
    Metres(f64),
    /// Needs an eye; without one nothing is over.
    Pixels(f64),
}

impl Tolerance {
    /// A quarter of a pixel: one sample of a picture rendered at four
    /// samples a pixel. A residual under it cannot uncover a sample.
    pub const QUARTER_PIXEL: Tolerance = Tolerance::Pixels(0.25);
}

/// The nearest point of a line of posts to `p`.
fn nearest(posts: &[Post], p: DVec3) -> Option<DVec3> {
    let mut best: Option<(f64, DVec3)> = None;
    let mut keep = |q: DVec3| {
        let d = (q - p).length_squared();
        if best.is_none_or(|(least, _)| d < least) {
            best = Some((d, q));
        }
    };
    if let [only] = posts {
        keep(only.top);
    }
    for pair in posts.windows(2) {
        let (a, b) = (pair[0].top, pair[1].top);
        let along = b - a;
        let t = if along.length_squared() > 0.0 {
            ((p - a).dot(along) / along.length_squared()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        keep(a + along * t);
    }
    best.map(|(_, q)| q)
}

/// The posts of an edge within a stretch of it, with the edge's own points
/// at the stretch's two ends.
fn within(posts: &[Post], span: (f64, f64)) -> Vec<Post> {
    let mut kept: Vec<Post> = Vec::new();
    if let Some(top) = at(posts, span.0) {
        kept.push(Post { along: span.0, top });
    }
    kept.extend(
        posts
            .iter()
            .filter(|p| p.along > span.0 + ON_THE_EDGE && p.along < span.1 - ON_THE_EDGE),
    );
    if let Some(top) = at(posts, span.1) {
        kept.push(Post { along: span.1, top });
    }
    kept
}

/// **What is left between the tiles a frame draws.**
///
/// For every stretch of edge two of the tiles share — whatever their levels
/// — the furthest any vertex of either edge stands from the other tile's
/// edge, as the two are drawn: in metres, split into height and horizontal,
/// and in pixels from `eye`. Two tiles that state one line give 0; a step,
/// a vertex off its neighbour's segment, an edge placed elsewhere each give
/// their size. Skirts are not looked at: this says whether the two edges
/// coincide, not whether something hides that they do not.
///
/// Edges across which no tile is drawn are counted apart, as `uncovered`.
///
/// `roots` is how many tiles the tree has across and up at level 0 (2 and 1
/// for the geographic globe): the tree wraps east to west, and a tile on its
/// top or bottom row has no neighbour beyond the pole.
///
/// The content is read as it is handed in — `local_origin_ecef` plus each
/// `f32` position — so a caller that holds the meshes a renderer was given
/// measures those, narrowing included.
pub fn residuals(
    drawn: &[Drawn<'_>],
    roots: (u64, u64),
    eye: Option<Eye>,
    tolerance: Tolerance,
) -> Residuals {
    /// Places on the grid are compared at this level.
    const DEEP: u32 = 40;
    const SIDES: [Side; 4] = [Side::West, Side::South, Side::East, Side::North];
    // The line a side lies on and the stretch of it the side covers; `None`
    // for a side on a pole.
    let span = |tile: &Drawn<'_>, side: Side| -> Option<(bool, u64, (u64, u64))> {
        let shift = DEEP.checked_sub(tile.level)?;
        let (x, y, one) = (tile.x << shift, tile.y << shift, 1u64 << shift);
        let (round, top) = (roots.0 << DEEP, roots.1 << DEEP);
        match side {
            Side::West => Some((true, x, (y, y + one))),
            Side::East => Some((true, (x + one) % round, (y, y + one))),
            Side::South => (y > 0).then_some((false, y, (x, x + one))),
            Side::North => (y + one < top).then_some((false, y + one, (x, x + one))),
        }
    };
    let posted: Vec<[Vec<Post>; 4]> = drawn
        .iter()
        .map(|tile| SIDES.map(|side| posts(tile.content, side)))
        .collect();
    /// On one line of the grid: each side lying on it — which tile, which
    /// side, and the stretch of the line it covers.
    type OnALine = Vec<(usize, usize, (u64, u64))>;
    let mut lines: std::collections::HashMap<(bool, u64), OnALine> =
        std::collections::HashMap::new();
    for (n, tile) in drawn.iter().enumerate() {
        for (s, side) in SIDES.into_iter().enumerate() {
            if let Some((meridian, line, stretch)) = span(tile, side) {
                lines
                    .entry((meridian, line))
                    .or_default()
                    .push((n, s, stretch));
            }
        }
    }

    let mut pairs: Vec<Residual> = Vec::new();
    let (mut uncovered, mut uncovered_metres) = (0usize, 0.0f64);
    let mut bare: Vec<Bare> = Vec::new();
    for ((meridian, _), sides) in &lines {
        for &(a, side_a, stretch_a) in sides {
            // West and south sides (0, 1) face the east and north sides
            // (2, 3) on their line.
            let mut covered = 0u64;
            for &(b, side_b, stretch_b) in sides.iter().filter(|s| s.1 / 2 != side_a / 2) {
                let (from, to) = (stretch_a.0.max(stretch_b.0), stretch_a.1.min(stretch_b.1));
                if from >= to {
                    continue;
                }
                covered += to - from;
                // Each pair once: from its east or north side.
                if side_a < 2 {
                    continue;
                }
                let part = |s: (u64, u64)| {
                    let whole = (s.1 - s.0) as f64;
                    ((from - s.0) as f64 / whole, (to - s.0) as f64 / whole)
                };
                let edge_a = within(&posted[a][side_a], part(stretch_a));
                let edge_b = within(&posted[b][side_b], part(stretch_b));
                let mut worst = Residual {
                    a: (drawn[a].level, drawn[a].x, drawn[a].y),
                    b: (drawn[b].level, drawn[b].x, drawn[b].y),
                    meridian: *meridian,
                    length: edge_a
                        .windows(2)
                        .map(|w| (w[1].top - w[0].top).length())
                        .sum(),
                    metres: 0.0,
                    height: 0.0,
                    horizontal: 0.0,
                    at: edge_a.first().map_or(DVec3::ZERO, |p| p.top),
                    pixels: 0.0,
                };
                for (own, other) in [(&edge_a, &edge_b), (&edge_b, &edge_a)] {
                    for post in own.iter() {
                        let Some(on) = nearest(other, post.top) else {
                            continue;
                        };
                        let apart = post.top - on;
                        let metres = apart.length();
                        let pixels = eye.map_or(0.0, |eye| {
                            metres * eye.pixels_per_radian
                                / (post.top - eye.position).length().max(f64::MIN_POSITIVE)
                        });
                        let more = if eye.is_some() {
                            pixels > worst.pixels
                        } else {
                            metres > worst.metres
                        };
                        if more {
                            let height = apart.dot(post.top.normalize());
                            worst.metres = metres;
                            worst.height = height;
                            worst.horizontal = (metres * metres - height * height).max(0.0).sqrt();
                            worst.at = post.top;
                            worst.pixels = pixels;
                        }
                    }
                }
                pairs.push(worst);
            }
            let whole = stretch_a.1 - stretch_a.0;
            if covered < whole {
                uncovered += 1;
                let length: f64 = posted[a][side_a]
                    .windows(2)
                    .map(|w| (w[1].top - w[0].top).length())
                    .sum();
                let metres = length * (whole - covered) as f64 / whole as f64;
                uncovered_metres += metres;
                if let Some(at) = at(&posted[a][side_a], 0.5) {
                    bare.push(Bare {
                        tile: (drawn[a].level, drawn[a].x, drawn[a].y),
                        side: SIDES[side_a],
                        at,
                        metres,
                    });
                }
            }
        }
    }

    let by = |value: fn(&Residual) -> f64| -> [f64; 3] {
        let mut all: Vec<f64> = pairs.iter().map(value).collect();
        all.sort_by(f64::total_cmp);
        let centile = |c: f64| match all.len() {
            0 => 0.0,
            n => all[((n - 1) as f64 * c).round() as usize],
        };
        [centile(1.0), centile(0.5), centile(0.99)]
    };
    let over = pairs
        .iter()
        .filter(|p| match tolerance {
            Tolerance::Metres(most) => p.metres > most,
            Tolerance::Pixels(most) => eye.is_some() && p.pixels > most,
        })
        .count();
    let same: Vec<f64> = pairs
        .iter()
        .filter(|p| p.a.0 == p.b.0)
        .map(|p| p.metres)
        .collect();
    let (metres, pixels) = (by(|p| p.metres), by(|p| p.pixels));
    if eye.is_some() {
        pairs.sort_by(|a, b| b.pixels.total_cmp(&a.pixels));
    } else {
        pairs.sort_by(|a, b| b.metres.total_cmp(&a.metres));
    }
    Residuals {
        shared: pairs.len(),
        over,
        same_level: same.len(),
        same_level_max: same.iter().copied().fold(0.0, f64::max),
        uncovered,
        uncovered_metres,
        bare,
        metres,
        pixels,
        pairs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{DecodedMesh, MaterialDesc};
    use crate::geo::WGS84_A;
    use glam::Mat4;

    /// A level square of ground 1 km a side on the equator, `east` metres
    /// east of the prime meridian and `height` over the ellipsoid, with, if
    /// asked, a wall hung `wall` metres down from the side named.
    fn square(east: f64, height: f64, wall: Option<(Side, f64)>) -> DecodedTileContent {
        // At (lon 0, lat 0): up is +X, east +Y, north +Z.
        let mut positions = vec![
            [0.0, 0.0, 0.0],
            [0.0, 1000.0, 0.0],
            [0.0, 0.0, 1000.0],
            [0.0, 1000.0, 1000.0],
        ];
        let mut uvs = vec![[0.0, 1.0], [1.0, 1.0], [0.0, 0.0], [1.0, 0.0]];
        let mut indices = vec![0, 1, 2, 2, 1, 3];
        if let Some((side, depth)) = wall {
            let (m, n) = match side {
                Side::West => (0, 2),
                Side::East => (1, 3),
                Side::South => (0, 1),
                Side::North => (2, 3),
            };
            for top in [m, n] {
                let p: [f32; 3] = positions[top];
                positions.push([p[0] - depth as f32, p[1], p[2]]);
                uvs.push(uvs[top]);
            }
            indices.extend_from_slice(&[m as u32, n as u32, 4, 4, n as u32, 5]);
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
            local_origin_ecef: DVec3::new(WGS84_A + height, east, 0.0),
            transform_local: Mat4::IDENTITY,
        }
    }

    fn between(west: &DecodedTileContent, east: &DecodedTileContent) -> Seam {
        seam(
            Meeting::whole(west, Side::East),
            Meeting::whole(east, Side::West),
            16,
        )
    }

    #[test]
    fn two_tiles_at_one_height_leave_nothing_to_pass_through() {
        let found = between(&square(0.0, 100.0, None), &square(1000.0, 100.0, None));
        assert!(found.is_closed());
        assert_eq!(found.rays, 0, "no step, so nothing to cast through");
        assert!(found.step < 1.0e-3);
    }

    #[test]
    fn a_step_with_no_wall_is_passed_through_everywhere() {
        let found = between(&square(0.0, 110.0, None), &square(1000.0, 100.0, None));
        assert_eq!(found.through, found.rays);
        assert!(found.rays > 0);
        assert!((found.open - 10.0).abs() < 1.0e-3, "{found:?}");
        assert!((found.length - 937.5).abs() < 1.0, "{found:?}");
    }

    #[test]
    fn a_wall_hung_from_the_higher_tile_closes_the_step() {
        let high = square(0.0, 110.0, Some((Side::East, 12.0)));
        let found = between(&high, &square(1000.0, 100.0, None));
        assert!(found.is_closed(), "{found:?}");
        assert!((found.step - 10.0).abs() < 1.0e-3);
    }

    #[test]
    fn a_wall_hung_from_the_lower_tile_closes_nothing() {
        let low = square(1000.0, 100.0, Some((Side::West, 12.0)));
        let found = between(&square(0.0, 110.0, None), &low);
        assert_eq!(found.through, found.rays, "{found:?}");
    }

    #[test]
    fn a_wall_shorter_than_the_step_leaves_its_foot_open() {
        let high = square(0.0, 110.0, Some((Side::East, 6.0)));
        let found = between(&high, &square(1000.0, 100.0, None));
        // Rays at 0.1 and 0.3 of the step pass under a wall that stops at 0.4.
        assert_eq!(found.through * 5, found.rays * 2, "{found:?}");
    }

    #[test]
    fn a_tile_meets_two_finer_ones_each_on_its_half() {
        let coarse = square(0.0, 110.0, None);
        let mut fine = square(1000.0, 100.0, None);
        // Half the size: its west side is the southern half of the coarse
        // tile's east side.
        for p in &mut fine.meshes[0].positions {
            p[1] *= 0.5;
            p[2] *= 0.5;
        }
        let found = seam(
            Meeting {
                content: &coarse,
                side: Side::East,
                span: (0.0, 0.5),
            },
            Meeting::whole(&fine, Side::West),
            8,
        );
        assert_eq!(found.through, found.rays);
        assert!((found.length - 437.5).abs() < 1.0, "{found:?}");
    }

    /// Two squares side by side as tiles 5/x/8 and 5/x+1/8 of a tree — the
    /// numbers only say they are neighbours.
    fn pair<'a>(west: &'a DecodedTileContent, east: &'a DecodedTileContent) -> [Drawn<'a>; 2] {
        [
            Drawn {
                level: 5,
                x: 10,
                y: 8,
                content: west,
            },
            Drawn {
                level: 5,
                x: 11,
                y: 8,
                content: east,
            },
        ]
    }

    #[test]
    fn two_edges_that_state_one_line_leave_nothing() {
        let (west, east) = (square(0.0, 100.0, None), square(1000.0, 100.0, None));
        let found = residuals(&pair(&west, &east), (2, 1), None, Tolerance::Metres(0.001));
        assert_eq!(found.shared, 1);
        assert_eq!(found.over, 0);
        assert!(found.metres[0] < 1.0e-6, "{found:?}");
        assert_eq!((found.same_level, found.same_level_max < 1.0e-6), (1, true));
    }

    #[test]
    fn a_step_is_reported_at_its_height() {
        let (west, east) = (square(0.0, 110.0, None), square(1000.0, 100.0, None));
        let found = residuals(&pair(&west, &east), (2, 1), None, Tolerance::Metres(0.5));
        assert_eq!(found.over, 1);
        let worst = found.pairs[0];
        assert!((worst.metres - 10.0).abs() < 1.0e-3, "{worst:?}");
        assert!((worst.height.abs() - 10.0).abs() < 1.0e-3, "{worst:?}");
        // The squares are flat and a kilometre from where the vertical is
        // their normal: a tenth of a milliradian of 10 m.
        assert!(worst.horizontal < 1.0e-2, "{worst:?}");
        assert_eq!((worst.a, worst.b), ((5, 10, 8), (5, 11, 8)));
        // A wall across the step changes nothing: the edges still part.
        let walled = square(0.0, 110.0, Some((Side::East, 12.0)));
        let still = residuals(&pair(&walled, &east), (2, 1), None, Tolerance::Metres(0.5));
        assert!((still.pairs[0].metres - 10.0).abs() < 1.0e-3, "{still:?}");
    }

    #[test]
    fn an_edge_placed_aside_is_reported_as_horizontal() {
        // The eastern square starts 3 m east of where the western one ends.
        let (west, east) = (square(0.0, 100.0, None), square(1003.0, 100.0, None));
        let found = residuals(&pair(&west, &east), (2, 1), None, Tolerance::Metres(0.5));
        let worst = found.pairs[0];
        assert!((worst.horizontal - 3.0).abs() < 1.0e-3, "{worst:?}");
        assert!(worst.height.abs() < 1.0e-3, "{worst:?}");
    }

    #[test]
    fn a_vertex_off_its_neighbours_segment_is_found() {
        // The eastern tile has a vertex half way up its west edge, 2 m over
        // the straight edge of the western tile: a T-junction that is off.
        let west = square(0.0, 100.0, None);
        let mut east = square(1000.0, 100.0, None);
        let mesh = &mut east.meshes[0];
        mesh.positions.push([2.0, 0.0, 500.0]);
        mesh.uvs.as_mut().expect("uvs").push([0.0, 0.5]);
        mesh.indices = vec![0, 1, 4, 4, 1, 3, 4, 3, 2];
        let found = residuals(&pair(&west, &east), (2, 1), None, Tolerance::Metres(0.5));
        assert_eq!(found.over, 1);
        assert!((found.pairs[0].metres - 2.0).abs() < 1.0e-3, "{found:?}");
        // The same vertex on the segment is no residual.
        east.meshes[0].positions[4] = [0.0, 0.0, 500.0];
        let on = residuals(&pair(&west, &east), (2, 1), None, Tolerance::Metres(0.5));
        assert_eq!(on.over, 0, "{on:?}");
    }

    #[test]
    fn a_residual_is_stated_in_pixels_from_the_eye() {
        let (west, east) = (square(0.0, 110.0, None), square(1000.0, 100.0, None));
        // 1 km over the step, a picture 1000 pixels a radian: 10 m is 10
        // pixels at the nearest point of the edge, less further along it.
        let eye = Eye {
            position: DVec3::new(WGS84_A + 1110.0, 1000.0, 0.0),
            pixels_per_radian: 1000.0,
        };
        let found = residuals(
            &pair(&west, &east),
            (2, 1),
            Some(eye),
            Tolerance::QUARTER_PIXEL,
        );
        assert_eq!(found.over, 1);
        assert!((found.pixels[0] - 10.0).abs() < 0.2, "{found:?}");
        let far = Eye {
            position: DVec3::new(WGS84_A + 100_110.0, 1000.0, 0.0),
            ..eye
        };
        let small = residuals(
            &pair(&west, &east),
            (2, 1),
            Some(far),
            Tolerance::QUARTER_PIXEL,
        );
        assert_eq!(
            small.over, 0,
            "a tenth of a pixel is under a quarter: {small:?}"
        );
    }

    #[test]
    fn an_edge_with_nothing_across_it_is_counted_apart() {
        let (west, east) = (square(0.0, 100.0, None), square(1000.0, 100.0, None));
        let found = residuals(&pair(&west, &east), (2, 1), None, Tolerance::Metres(0.001));
        // Two tiles, eight sides, two of them facing each other.
        assert_eq!(found.uncovered, 6);
        assert!((found.uncovered_metres - 6000.0).abs() < 1.0, "{found:?}");
        assert_eq!(found.bare.len(), 6);
        assert!(found.bare.iter().all(|b| (b.metres - 1000.0).abs() < 1.0));
        assert!(!found
            .bare
            .iter()
            .any(|b| b.tile == (5, 10, 8) && b.side == Side::East));
        // A tile on the top row has no neighbour over the pole: its north
        // side is not an edge onto nothing.
        let top = [Drawn {
            level: 0,
            x: 0,
            y: 0,
            content: &west,
        }];
        let alone = residuals(&top, (2, 1), None, Tolerance::Metres(0.001));
        assert_eq!(alone.uncovered, 2, "west and east only: {alone:?}");
    }
}
