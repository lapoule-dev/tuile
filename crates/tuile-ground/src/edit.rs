// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Edits: volume operations applied in a contiguous order the caller gives.
//!
//! An edit adds or removes solid inside a [`Brush`], expressed in the
//! ground's frame (radii in metres). Edits carry a sequence number
//! ([`EditSeq`]) chosen by whoever is authoritative (a server). A fresh
//! ground expects `EditSeq(1)`, then every following number with no gap:
//!
//! - an edit whose number is the expected one is **consumed**: applied, or
//!   refused for its content ([`GroundError::OutOfBounds`],
//!   [`GroundError::InvalidBrush`]). A refusal for content is the same on
//!   every replica, so it consumes the number too and changes nothing else;
//! - an edit from the past ([`GroundError::Stale`]) or from beyond the next
//!   number ([`GroundError::Gap`]) is refused and changes nothing, not even
//!   the expected number: the caller delivers the missing edits first.
//!
//! So every replica that is handed the same numbered edits ends with the same
//! bits however the network delivered them, and a lost edit stops a replica
//! instead of letting it diverge silently.
//!
//! An applied edit bumps the [`Version`] of every edit-level tile whose
//! answers may have changed and reports it as a [`TileChanged`] event; an edit
//! that changes nothing reports nothing.
//!
//! [`GroundError::OutOfBounds`]: crate::GroundError::OutOfBounds
//! [`GroundError::InvalidBrush`]: crate::GroundError::InvalidBrush
//! [`GroundError::Stale`]: crate::GroundError::Stale
//! [`GroundError::Gap`]: crate::GroundError::Gap
//! [`Version`]: crate::Version

use glam::DVec3;

use crate::frame::{Column, Frame, Position, TileKey, Version};

/// The caller's contiguous order of edits; a fresh ground expects
/// `EditSeq(1)`, and `EditSeq(0)` means "none yet".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditSeq(pub u64);

/// Add solid, or carve it away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditOp {
    Add,
    Remove,
}

/// The volume an edit acts on, in the frame's coordinates; radii in metres.
///
/// A brush is measured column by column in that column's tangent plane
/// ([`Frame::tangent_offset`]): exact in a planar frame; in a geodetic frame
/// horizontal distances are those of the sphere (short by `θ²/6` of the
/// arc, 4·10⁻⁵ at 100 km), and heights stay along each column (the earth's
/// curvature under a brush, `d²/2R`, is not folded in). A ball at a pole
/// carves a disc around it. A capsule's axis is taken in each column's
/// tangent plane, so it runs the short way across the antimeridian.
///
/// The fields are public: a brush is validated when an edit is applied
/// ([`Brush::is_valid_in`]), and an invalid one is refused there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Brush {
    /// A ball; `radius` finite and positive.
    Sphere { centre: Position, radius: f64 },
    /// The segment `a`–`b` swept by a ball of `radius` (finite and
    /// positive), at any slope.
    Capsule {
        a: Position,
        b: Position,
        radius: f64,
    },
    /// A box aligned with the frame's axes, corners included; `min <= max`
    /// on every axis. Its coordinates are taken as given: on a frame that
    /// wraps, a box does not wrap (split one across the antimeridian in two).
    Box { min: Position, max: Position },
}

impl Brush {
    /// Whether the brush is a volume: every coordinate finite, radii finite
    /// and positive (and small enough to square), a box's min at most its
    /// max, and its vertical extent finite.
    pub fn is_valid(&self) -> bool {
        let radius_ok = |z: f64, r: f64| {
            r.is_finite()
                && r > 0.0
                && (r * r).is_finite()
                && (z - r).is_finite()
                && (z + r).is_finite()
        };
        match *self {
            Brush::Sphere { centre, radius } => centre.is_finite() && radius_ok(centre.z, radius),
            Brush::Capsule { a, b, radius } => {
                a.is_finite() && b.is_finite() && radius_ok(a.z, radius) && radius_ok(b.z, radius)
            }
            Brush::Box { min, max } => min.is_finite() && max.is_finite() && min.cmple(max).all(),
        }
    }

    /// Whether the brush is a volume ([`Self::is_valid`]) at places of the
    /// frame ([`Frame::is_place`]: a latitude beyond ±90° is none).
    pub fn is_valid_in<F: Frame + ?Sized>(&self, frame: &F) -> bool {
        let places = match *self {
            Brush::Sphere { centre, .. } => [centre, centre],
            Brush::Capsule { a, b, .. } => [a, b],
            Brush::Box { min, max } => [min, max],
        };
        self.is_valid() && places.iter().all(|p| frame.is_place(p.truncate()))
    }

    /// The rectangle of columns the brush can reach (both corners
    /// included), from [`Frame::reach`]: it may extend beyond the bounds,
    /// and spans a whole period of `u` when the brush covers a pole.
    pub fn footprint<F: Frame + ?Sized>(&self, frame: &F) -> (Column, Column) {
        match *self {
            Brush::Sphere { centre, radius } => {
                frame.reach(centre.truncate(), centre.truncate(), radius)
            }
            Brush::Capsule { a, b, radius } => frame.reach(
                a.truncate().min(b.truncate()),
                a.truncate().max(b.truncate()),
                radius,
            ),
            Brush::Box { min, max } => (min.truncate(), max.truncate()),
        }
    }

    /// The vertical extent `[bottom, top]` of the brush on a column's
    /// vertical line, `None` where it misses it. Exact (up to rounding) in
    /// the column's tangent plane, for every brush and slope; `+ − × ÷ sqrt`
    /// and the frame's [`Frame::tangent_offset`].
    pub fn column_extent<F: Frame + ?Sized>(
        &self,
        frame: &F,
        column: Column,
    ) -> Option<(f64, f64)> {
        // A point relative to the column's vertical line, in metres.
        let local = |p: Position| {
            frame
                .tangent_offset(column, p.truncate())
                .map(|o| DVec3::new(o.x, o.y, p.z))
        };
        match *self {
            Brush::Sphere { centre, radius } => ball_chord(local(centre)?, radius),
            Brush::Capsule { a, b, radius } => {
                let (a, b) = (local(a), local(b));
                let axis = a.zip(b).and_then(|(a, b)| cylinder_chord(a, b, radius));
                // A capsule is the two end balls and the cylinder between
                // them; its chord is the hull of theirs (a capsule is convex).
                [
                    a.and_then(|a| ball_chord(a, radius)),
                    b.and_then(|b| ball_chord(b, radius)),
                    axis,
                ]
                .into_iter()
                .flatten()
                .reduce(|x, y| (x.0.min(y.0), x.1.max(y.1)))
            }
            Brush::Box { min, max } => {
                (column.x >= min.x && column.x <= max.x && column.y >= min.y && column.y <= max.y)
                    .then_some((min.z, max.z))
            }
        }
    }
}

/// One edit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edit {
    pub seq: EditSeq,
    pub op: EditOp,
    pub brush: Brush,
}

/// An edit-level tile whose answers may have changed: derived data built on
/// an older version is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileChanged {
    pub tile: TileKey,
    pub version: Version,
}

/// The chord of a ball centred at `c` (relative to the line `x = y = 0`) on
/// that line.
fn ball_chord(c: DVec3, r: f64) -> Option<(f64, f64)> {
    let h2 = r * r - c.x * c.x - c.y * c.y;
    if h2 < 0.0 {
        return None;
    }
    let h = h2.sqrt();
    Some((c.z - h, c.z + h))
}

/// The chord, on the line `x = y = 0`, of the finite cylinder of radius `r`
/// around the segment `a`–`b`: the points whose projection on the axis falls
/// between `a` and `b` and whose distance to the axis is at most `r`.
fn cylinder_chord(a: DVec3, b: DVec3, r: f64) -> Option<(f64, f64)> {
    let d = b - a;
    let horizontal = d.x * d.x + d.y * d.y;
    let len2 = horizontal + d.z * d.z;
    if len2 == 0.0 {
        // A point: the end balls are the whole capsule.
        return None;
    }
    if horizontal == 0.0 {
        // A vertical axis: the line is parallel to it, inside the cylinder
        // along the whole segment or nowhere.
        return (a.x * a.x + a.y * a.y <= r * r).then_some((a.z.min(b.z), a.z.max(b.z)));
    }
    // A point of the line is `p = (0, 0, z)`; `w + z·ẑ = p − a`.
    let w = -a;
    let wd = w.x * d.x + w.y * d.y + w.z * d.z;
    let ww = w.x * w.x + w.y * w.y + w.z * w.z;
    // Distance² to the axis ≤ r², times len2:
    // horizontal·z² + 2·(w.z·len2 − wd·d.z)·z + (ww·len2 − wd² − r²·len2) ≤ 0.
    let half_b = w.z * len2 - wd * d.z;
    let c = ww * len2 - wd * wd - r * r * len2;
    let disc = half_b * half_b - horizontal * c;
    if disc < 0.0 {
        return None;
    }
    // The two roots without cancellation (an axis close to vertical makes
    // `half_b` large against `horizontal`).
    let s = disc.sqrt();
    let m = -(half_b + if half_b >= 0.0 { s } else { -s });
    let r0 = m / horizontal;
    let r1 = if m == 0.0 { r0 } else { c / m };
    let (mut z0, mut z1) = (r0.min(r1), r0.max(r1));
    // The projection's parameter, (wd + z·d.z) / len2, must lie in [0, 1].
    if d.z == 0.0 {
        if wd < 0.0 || wd > len2 {
            return None;
        }
    } else {
        let (za, zb) = (-wd / d.z, (len2 - wd) / d.z);
        z0 = z0.max(za.min(zb));
        z1 = z1.min(za.max(zb));
    }
    (z0 <= z1).then_some((z0, z1))
}

#[cfg(test)]
mod tests {
    use glam::DVec2;

    use super::*;
    use crate::frame::PlanarFrame;

    fn f() -> PlanarFrame {
        PlanarFrame {
            half_extent: 4096.0,
            edit_level: 4,
            anchor: None,
        }
    }

    fn close(a: Option<(f64, f64)>, b: (f64, f64)) -> bool {
        a.is_some_and(|a| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9)
    }

    #[test]
    fn a_sphere_covers_its_disc_only() {
        let s = Brush::Sphere {
            centre: DVec3::new(0.0, 0.0, 10.0),
            radius: 5.0,
        };
        assert_eq!(s.column_extent(&f(), DVec2::ZERO), Some((5.0, 15.0)));
        assert_eq!(
            s.column_extent(&f(), DVec2::new(3.0, 0.0)),
            Some((6.0, 14.0)),
            "3-4-5 triangle"
        );
        assert_eq!(s.column_extent(&f(), DVec2::new(6.0, 0.0)), None);
    }

    #[test]
    fn a_box_covers_its_rectangle() {
        let b = Brush::Box {
            min: DVec3::ZERO,
            max: DVec3::new(2.0, 2.0, 1.0),
        };
        assert_eq!(
            b.column_extent(&f(), DVec2::new(2.0, 1.0)),
            Some((0.0, 1.0))
        );
        assert_eq!(b.column_extent(&f(), DVec2::new(3.0, 1.0)), None);
    }

    #[test]
    fn a_capsule_is_a_sphere_swept_along_its_segment() {
        let c = Brush::Capsule {
            a: DVec3::ZERO,
            b: DVec3::new(100.0, 0.0, 0.0),
            radius: 4.0,
        };
        assert!(close(
            c.column_extent(&f(), DVec2::new(50.0, 0.0)),
            (-4.0, 4.0)
        ));
        assert!(close(
            c.column_extent(&f(), DVec2::new(50.0, 3.0)),
            (-7f64.sqrt(), 7f64.sqrt())
        ));
        assert_eq!(c.column_extent(&f(), DVec2::new(50.0, 5.0)), None);
        assert!(
            close(
                c.column_extent(&f(), DVec2::new(-3.0, 0.0)),
                (-7f64.sqrt(), 7f64.sqrt())
            ),
            "the rounded end"
        );
    }

    #[test]
    fn a_vertical_capsule_is_a_shaft() {
        let shaft = Brush::Capsule {
            a: DVec3::new(0.0, 0.0, 0.0),
            b: DVec3::new(0.0, 0.0, -50.0),
            radius: 2.0,
        };
        assert!(close(shaft.column_extent(&f(), DVec2::ZERO), (-52.0, 2.0)));
        let side = 3f64.sqrt();
        assert!(
            close(
                shaft.column_extent(&f(), DVec2::new(1.0, 0.0)),
                (-50.0 - side, side)
            ),
            "off the axis: the ends are rounded"
        );
        assert_eq!(shaft.column_extent(&f(), DVec2::new(2.5, 0.0)), None);
    }

    #[test]
    fn a_sloping_capsule_is_cut_along_its_slope() {
        // At 45°: the axis crosses the column x = 50 at z = 50, and a
        // vertical line meets the cylinder over r·√2 on each side.
        let ramp = Brush::Capsule {
            a: DVec3::ZERO,
            b: DVec3::new(100.0, 0.0, 100.0),
            radius: 2.0,
        };
        let half = 2.0 * 2f64.sqrt();
        assert!(close(
            ramp.column_extent(&f(), DVec2::new(50.0, 0.0)),
            (50.0 - half, 50.0 + half)
        ));
        // Past the top end, beyond the cylinder's end face: only the end
        // ball (h² = 4 − 1.5²).
        let h = 1.75f64.sqrt();
        assert!(close(
            ramp.column_extent(&f(), DVec2::new(101.5, 0.0)),
            (100.0 - h, 100.0 + h)
        ));
        // Just past the end, the cylinder's slanted end face still reaches
        // below the ball.
        assert!(close(
            ramp.column_extent(&f(), DVec2::new(101.0, 0.0)),
            (101.0 - half, 100.0 + 3f64.sqrt())
        ));
        // Near the low end, the cylinder's cut and the ball's merge.
        let e = ramp.column_extent(&f(), DVec2::new(0.5, 0.0)).unwrap();
        assert!(e.0 < -1.9 && e.1 > 0.5 + half - 1e-9, "{e:?}");
    }

    #[test]
    fn brushes_are_volumes() {
        let p = DVec3::ZERO;
        assert!(Brush::Sphere {
            centre: p,
            radius: 1.0
        }
        .is_valid());
        for bad in [
            Brush::Sphere {
                centre: p,
                radius: -1.0,
            },
            Brush::Sphere {
                centre: p,
                radius: 0.0,
            },
            Brush::Sphere {
                centre: p,
                radius: f64::NAN,
            },
            Brush::Sphere {
                centre: p,
                radius: 1e200,
            },
            Brush::Sphere {
                centre: DVec3::new(0.0, 0.0, f64::NAN),
                radius: 1.0,
            },
            Brush::Capsule {
                a: p,
                b: DVec3::new(0.0, 0.0, f64::INFINITY),
                radius: 1.0,
            },
            Brush::Box {
                min: p,
                max: DVec3::new(1.0, 1.0, f64::INFINITY),
            },
            Brush::Box {
                min: DVec3::new(0.0, 0.0, f64::NEG_INFINITY),
                max: DVec3::ONE,
            },
            Brush::Box {
                min: DVec3::ONE,
                max: p,
            },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }
}
