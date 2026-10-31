// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Edits: volume operations applied in a total order the caller gives.
//!
//! An edit adds or removes solid inside a [`Brush`], expressed in the
//! ground's frame (radii in metres). Edits carry a sequence number
//! ([`EditSeq`]) chosen by whoever is authoritative (a server); a ground
//! applies them in strictly increasing order and refuses anything else, so
//! every replica that receives the same edits ends with the same bits however
//! the network delivered them. Each applied edit bumps the
//! [`Version`] of every edit-level tile its brush covers and
//! reports them as [`TileChanged`] events.

use glam::DVec2;

use crate::frame::{Column, Frame, Position, TileKey, Version};

/// The caller's total order of edits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditSeq(pub u64);

/// Add solid, or carve it away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditOp {
    Add,
    Remove,
}

/// The volume an edit acts on, in the frame's coordinates; radii in metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Brush {
    Sphere {
        centre: Position,
        radius: f64,
    },
    /// The segment `a`–`b` swept by a sphere of `radius`.
    Capsule {
        a: Position,
        b: Position,
        radius: f64,
    },
    /// A box aligned with the frame's axes, corners included.
    Box {
        min: Position,
        max: Position,
    },
}

impl Brush {
    /// The rectangle of columns the brush can reach.
    pub fn footprint<F: Frame + ?Sized>(&self, frame: &F) -> (Column, Column) {
        let reach = |c: Position, r: f64| {
            let m = frame.metres_per_unit(c.truncate());
            DVec2::new(r / m.x, r / m.y)
        };
        match *self {
            Brush::Sphere { centre, radius } => {
                let d = reach(centre, radius);
                (centre.truncate() - d, centre.truncate() + d)
            }
            Brush::Capsule { a, b, radius } => {
                let d = reach(a, radius).max(reach(b, radius));
                (
                    a.truncate().min(b.truncate()) - d,
                    a.truncate().max(b.truncate()) + d,
                )
            }
            Brush::Box { min, max } => (min.truncate(), max.truncate()),
        }
    }

    /// The vertical extent `[bottom, top]` of the brush in a column, `None`
    /// where it does not cover it. Uses only `+ − × ÷ sqrt`.
    pub fn column_extent<F: Frame + ?Sized>(
        &self,
        frame: &F,
        column: Column,
    ) -> Option<(f64, f64)> {
        match *self {
            Brush::Sphere { centre, radius } => vertical_chord(frame, centre, radius, column),
            Brush::Capsule { a, b, radius } => {
                // The chord of the sphere at the closest point of the axis'
                // horizontal projection: exact when a and b share a height.
                vertical_chord(
                    frame,
                    closest_on_segment(frame, a, b, column),
                    radius,
                    column,
                )
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

/// An edit-level tile changed: derived data built on an older version is
/// stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileChanged {
    pub tile: TileKey,
    pub version: Version,
}

/// Horizontal offset `to − from` in metres, measured with the metric at `from`.
fn metres<F: Frame + ?Sized>(frame: &F, from: Column, to: Column) -> DVec2 {
    (to - from) * frame.metres_per_unit(from)
}

fn vertical_chord<F: Frame + ?Sized>(
    frame: &F,
    centre: Position,
    radius: f64,
    column: Column,
) -> Option<(f64, f64)> {
    let d = metres(frame, centre.truncate(), column);
    let h2 = radius * radius - d.x * d.x - d.y * d.y;
    if h2 < 0.0 {
        return None;
    }
    let h = h2.sqrt();
    Some((centre.z - h, centre.z + h))
}

fn closest_on_segment<F: Frame + ?Sized>(
    frame: &F,
    a: Position,
    b: Position,
    column: Column,
) -> Position {
    let ab = metres(frame, a.truncate(), b.truncate());
    let ap = metres(frame, a.truncate(), column);
    let len2 = ab.x * ab.x + ab.y * ab.y;
    if len2 == 0.0 {
        return a;
    }
    let t = ((ap.x * ab.x + ap.y * ab.y) / len2).clamp(0.0, 1.0);
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use glam::DVec3;

    use super::*;
    use crate::frame::PlanarFrame;

    fn f() -> PlanarFrame {
        PlanarFrame {
            half_extent: 4096.0,
            edit_level: 4,
            anchor: None,
        }
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
        assert_eq!(
            c.column_extent(&f(), DVec2::new(50.0, 0.0)),
            Some((-4.0, 4.0))
        );
        assert_eq!(c.column_extent(&f(), DVec2::new(50.0, 5.0)), None);
        assert!(
            c.column_extent(&f(), DVec2::new(-3.0, 0.0)).is_some(),
            "the rounded end"
        );
    }
}
