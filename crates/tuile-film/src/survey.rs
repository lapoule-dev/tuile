// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a pack holds, for someone looking rather than rendering: the path
//! its camera flies and the tiles a frame draws. All of it from the table.

use glam::DVec3;
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_pack::{Pack, PackError, TextureFormat};

/// Where a frame's camera is and where it looks, on the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraSample {
    pub frame: u32,
    pub lon_deg: f64,
    pub lat_deg: f64,
    /// Above the ellipsoid, metres.
    pub height_m: f64,
    /// Clockwise from north.
    pub heading_deg: f64,
    /// Above the horizon; −90 looks straight down.
    pub pitch_deg: f64,
    pub fovy_deg: f64,
}

/// The pack's camera path, thinned to at most `max` evenly spaced frames
/// (the last frame is always kept, so the path ends where the film does).
pub fn cameras(pack: &Pack<'_>, max: usize) -> Vec<CameraSample> {
    let views = pack.views();
    let step = views.len().div_ceil(max.max(1)).max(1);
    let last = views.len().saturating_sub(1);
    views
        .iter()
        .enumerate()
        .filter(|(i, _)| i % step == 0 || *i == last)
        .map(|(_, (frame, view))| {
            let g = ecef_to_geodetic(DVec3::from_array(view.position));
            let enu = enu_frame(g);
            let d = DVec3::from_array(view.direction).normalize();
            CameraSample {
                frame: *frame,
                lon_deg: g.lon.to_degrees(),
                lat_deg: g.lat.to_degrees(),
                height_m: g.height,
                heading_deg: d.dot(enu.x_axis).atan2(d.dot(enu.y_axis)).to_degrees(),
                pitch_deg: d.dot(enu.z_axis).clamp(-1.0, 1.0).asin().to_degrees(),
                fovy_deg: view.fovy_rad.to_degrees(),
            }
        })
        .collect()
}

/// One tile a frame draws, as the table describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct TileInfo {
    pub id: u64,
    pub drape: u64,
    pub vertices: u32,
    pub triangles: u32,
    /// The encoded texture's size, 0 when the tile has none.
    pub texture_bytes: u32,
    /// Where the tile sits: its origin on the ellipsoid.
    pub lon_deg: f64,
    pub lat_deg: f64,
}

/// The tiles one frame draws, in the order the bake recorded them.
pub fn frame_tiles(pack: &Pack<'_>, frame: u32) -> Result<Vec<TileInfo>, PackError> {
    Ok(pack
        .frame(frame)?
        .iter()
        .map(|tile| {
            let origin = tile
                .origin_ecef()
                .filter(|v| v.len() == 3)
                .map(|v| DVec3::new(v.get(0), v.get(1), v.get(2)));
            let g = origin.map(ecef_to_geodetic);
            TileInfo {
                id: tile.id(),
                drape: tile.drape(),
                vertices: tile.vertex_count(),
                triangles: tile.index_count() / 3,
                texture_bytes: match tile.texture_format() {
                    TextureFormat::None => 0,
                    _ => tile.texture().map_or(0, |b| b.stored()),
                },
                lon_deg: g.map_or(f64::NAN, |g| g.lon.to_degrees()),
                lat_deg: g.map_or(f64::NAN, |g| g.lat.to_degrees()),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::tests::three_frames;
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};
    use tuile_pack::{BakedView, PackWriter};

    #[test]
    fn a_camera_looking_down_reads_minus_ninety_where_it_is() {
        let g = Geodetic {
            lon: 0.1,
            lat: 0.7,
            height: 5000.0,
        };
        let (p, enu) = (geodetic_to_ecef(g), enu_frame(g));
        let dir = tempfile::tempdir().expect("tempdir");
        let mut w = PackWriter::new("s", [0.0; 3], dir.path().join("blob")).expect("writer");
        for frame in 1..=10 {
            let view = BakedView {
                position: p.to_array(),
                direction: (-enu.z_axis * 3.0).to_array(),
                up: enu.y_axis.to_array(),
                viewport_px: [640.0, 480.0],
                fovy_rad: 1.0,
            };
            w.frame(frame, view, []);
        }
        let path = dir.path().join("p.tuilepack");
        w.finish_to(&path).expect("finish");
        let bytes = std::fs::read(path).expect("read");
        let pack = Pack::open(&bytes).expect("open");

        let all = cameras(&pack, 100);
        assert_eq!(all.len(), 10);
        let c = all[0];
        assert!((c.pitch_deg + 90.0).abs() < 1e-6, "{}", c.pitch_deg);
        assert!((c.height_m - 5000.0).abs() < 1e-3);
        assert!((c.lat_deg - 0.7f64.to_degrees()).abs() < 1e-9);
        assert!((c.fovy_deg - 1.0f64.to_degrees()).abs() < 1e-9);

        // Thinned, the path still starts and ends where the film does.
        let thin = cameras(&pack, 3);
        assert!(thin.len() <= 4, "{}", thin.len());
        assert_eq!(thin.first().map(|c| c.frame), Some(1));
        assert_eq!(thin.last().map(|c| c.frame), Some(10));
    }

    #[test]
    fn a_frames_tiles_are_listed_as_baked() {
        let bytes = three_frames();
        let pack = Pack::open(&bytes).expect("open");
        let tiles = frame_tiles(&pack, 12).expect("tiles");
        assert_eq!(
            tiles.iter().map(|t| (t.id, t.drape)).collect::<Vec<_>>(),
            [(2, 0), (3, 7)]
        );
        assert_eq!(tiles[0].triangles, 1);
        assert_eq!(tiles[0].texture_bytes, 0);
        assert!(tiles[0].lon_deg.abs() < 1e-9 && tiles[0].lat_deg.abs() < 1e-9);
        assert!(frame_tiles(&pack, 99).is_err());
    }
}
