// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Prints the cameras a pack was baked for, from its table alone.
//!
//! ```bash
//! cargo run -p tuile-film --example pack_cameras -- head.bin
//! ```
//!
//! `head.bin` is the pack's first bytes up to its blob region (or the whole
//! pack): every frame's camera is in the table, so nothing else is read.

use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_pack::{blob_start, Pack};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pack_cameras <head or pack>");
    let bytes = std::fs::read(&path).expect("read");
    let start = blob_start(&bytes).expect("a pack") as usize;
    let pack = Pack::open_table(&bytes[..start]).expect("table");
    let (first, last) = pack.frame_range();
    println!(
        "scene {}, frames {first}..={last}, {} tiles, table {} bytes",
        pack.scene_digest(),
        pack.tile_count(),
        start
    );
    let step = ((last - first) / 8).max(1);
    for frame in (first..=last).step_by(step as usize) {
        let v = pack.view_of(frame).expect("view");
        let p = glam::DVec3::from_array(v.position);
        let g = ecef_to_geodetic(p);
        let d = glam::DVec3::from_array(v.direction).normalize();
        let pitch = d
            .dot(enu_frame(g).z_axis)
            .clamp(-1.0, 1.0)
            .asin()
            .to_degrees();
        println!(
            "frame {frame:>6}: lon {:>10.5} lat {:>9.5} height {:>9.1} m, pitch {:>6.1}°, fovy {:.1}°, viewport {}x{}, {} tiles",
            g.lon.to_degrees(), g.lat.to_degrees(), g.height, pitch, v.fovy_rad.to_degrees(),
            v.viewport_px[0], v.viewport_px[1], pack.frame(frame).expect("frame").len()
        );
    }
}
