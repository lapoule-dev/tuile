// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A reference picture under a film's imagery: another layer of the tile
//! store, homogeneous over the whole ground, which each of the film's tiles
//! is set against place for place.
//!
//! The reference is cut on a grid of its own, so a tile of the film lies
//! over parts of up to four of its tiles. A place of the film's tile is
//! carried to the reference by where it is on the ground — longitude and
//! latitude — as a drape is.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use tuile_core::raster::{ImageryCoord, TilingScheme};
use tuile_radiometry::{linear_of, Observed, PAIRS};
use tuile_repository::{ArchivedTiles, TileRepository};

use crate::Error;

type Coord = (u8, u32, u32);

/// Decoded tiles of the reference held at once.
const HELD: usize = 256;

/// A tile of the reference, decoded: stored bytes, three a texel.
struct Decoded {
    wide: usize,
    high: usize,
    rgb: Vec<u8>,
}

/// Linear light as it is stored: the byte of its sRGB encoding.
fn stored(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// What became of setting a film's tiles against the reference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetAgainst {
    /// Tiles of the film.
    pub tiles: usize,
    /// Of them, tiles with reference under all their places.
    pub whole: usize,
    /// …and tiles with none under any.
    pub bare: usize,
    /// Tiles of the reference read, and those the store has not.
    pub read: usize,
    pub absent: usize,
}

/// A reference layer of the store: see the module.
pub struct Reference {
    layer: String,
    scheme: TilingScheme,
    /// The film's level past which the reference gets no finer.
    cap: u8,
    held: HashMap<Coord, Option<Decoded>>,
    order: VecDeque<Coord>,
}

fn coord(at: Coord) -> ImageryCoord {
    ImageryCoord {
        level: u32::from(at.0),
        x: u64::from(at.1),
        y: u64::from(at.2),
    }
}

impl Reference {
    /// The reference `layer`, cut on `scheme`. A film's tile is set against
    /// the finest level of it whose tiles are no narrower than the film's
    /// own at `min(its level, cap)`.
    pub fn new(layer: impl Into<String>, scheme: TilingScheme, cap: u8) -> Self {
        Self {
            layer: layer.into(),
            scheme,
            cap,
            held: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// The level of the reference a film's tile of `level` is set against.
    pub fn level_under(&self, film: &TilingScheme, level: u8) -> u8 {
        let wide = |scheme: &TilingScheme, level: u32| {
            let (x0, _, x1, _) = scheme.tile_extent(ImageryCoord { level, x: 0, y: 0 });
            x1 - x0
        };
        let as_wide = wide(film, u32::from(level.min(self.cap)));
        (0..=TilingScheme::MAX_LEVEL)
            .rev()
            .find(|l| wide(&self.scheme, *l) >= as_wide * 0.999)
            .unwrap_or(0) as u8
    }

    /// A rectangle of a film's tile — `(u0, v0, u1, v1)`, across and down,
    /// 0 to 1 — as texels of the reference at `level`: from, to, each
    /// across and down, over the whole world.
    fn texels(
        &self,
        film: &TilingScheme,
        at: Coord,
        rect: (f32, f32, f32, f32),
        level: u8,
        side: f64,
    ) -> (f64, f64, f64, f64) {
        let (x0, y0, x1, y1) = film.tile_extent(coord(at));
        let carried = |u: f32, v: f32| {
            let on_ground = film
                .projection
                .from_normalized(x0 + (x1 - x0) * f64::from(u), y0 + (y1 - y0) * f64::from(v));
            let (x, y) = self.scheme.projection.to_normalized(on_ground);
            let (across, down) = self.scheme.tiles_at(u32::from(level));
            (x * across as f64 * side, y * down as f64 * side)
        };
        let (from, to) = (carried(rect.0, rect.1), carried(rect.2, rect.3));
        (from.0, from.1, to.0, to.1)
    }

    /// The reference's tiles under a film's tile.
    fn tiles_under(&self, film: &TilingScheme, at: Coord) -> Vec<Coord> {
        let level = self.level_under(film, at.0);
        let rect = film.tile_rect(coord(at));
        self.scheme
            .tiles_in_rectangle(&rect, u32::from(level))
            .into_iter()
            .map(|c| (c.level as u8, c.x as u32, c.y as u32))
            .collect()
    }

    /// The reference over a rectangle of a film's tile, in linear light:
    /// the mean of its texels whose middle lies in it, or the one nearest
    /// its middle if it is smaller than a texel. `None` where the reference
    /// held has none.
    fn under(
        &self,
        film: &TilingScheme,
        at: Coord,
        rect: (f32, f32, f32, f32),
    ) -> Option<[f32; 3]> {
        let level = self.level_under(film, at.0);
        let side = f64::from(self.scheme.tile_size);
        let (px0, py0, px1, py1) = self.texels(film, at, rect, level, side);
        // Texels whose middle is inside; at least the one at the middle.
        let span = |from: f64, to: f64| {
            let (first, last) = ((from - 0.5).ceil() as i64, (to - 0.5).ceil() as i64);
            if last > first {
                first..last
            } else {
                let middle = ((from + to) / 2.0).floor() as i64;
                middle..middle + 1
            }
        };
        let (mut sum, mut counted) = ([0.0f64; 3], 0u32);
        for py in span(py0, py1) {
            for px in span(px0, px1) {
                if px < 0 || py < 0 {
                    continue;
                }
                let side = side as i64;
                let tile = (level, (px / side) as u32, (py / side) as u32);
                let Some(Some(decoded)) = self.held.get(&tile) else {
                    continue;
                };
                // A tile of another size than its grid says is read at the
                // same place of it.
                let x = (px % side) as usize * decoded.wide / side as usize;
                let y = (py % side) as usize * decoded.high / side as usize;
                let texel = &decoded.rgb[(y * decoded.wide + x) * 3..][..3];
                for c in 0..3 {
                    sum[c] += f64::from(linear_of(texel[c]));
                }
                counted += 1;
            }
        }
        (counted > 0).then(|| sum.map(|v| (v / f64::from(counted)) as f32))
    }

    /// Reads the tiles of the reference under a film's tile that are not
    /// held yet. Returns how many came, and how many the store has not.
    async fn hold(
        &mut self,
        store: &ArchivedTiles,
        under: &[Coord],
    ) -> Result<(usize, usize), Error> {
        let missing: Vec<Coord> = under
            .iter()
            .filter(|c| !self.held.contains_key(c))
            .copied()
            .collect();
        let read = futures_util::future::join_all(
            missing
                .iter()
                .map(|c| store.tile(&self.layer, c.0, c.1, c.2)),
        )
        .await;
        let (mut came, mut absent) = (0, 0);
        for (c, tile) in missing.iter().zip(read) {
            let decoded = tile?
                .and_then(|tile| image::load_from_memory(&tile.bytes).ok())
                .map(|picture| {
                    let rgb = picture.to_rgb8();
                    Decoded {
                        wide: rgb.width() as usize,
                        high: rgb.height() as usize,
                        rgb: rgb.into_raw(),
                    }
                });
            came += usize::from(decoded.is_some());
            absent += usize::from(decoded.is_none());
            self.held.insert(*c, decoded);
            self.order.push_back(*c);
        }
        Ok((came, absent))
    }

    /// Lets go of the tiles held longest, past what is held at once —
    /// never one of `under`.
    fn let_go(&mut self, under: &[Coord]) {
        let mut kept = Vec::new();
        while self.order.len() + kept.len() > HELD {
            let Some(gone) = self.order.pop_front() else {
                break;
            };
            if under.contains(&gone) {
                kept.push(gone);
            } else {
                self.held.remove(&gone);
            }
        }
        self.order.extend(kept);
    }

    /// The reference as a mosaic of a film's tiles: under each of `tiles`,
    /// `side` texels a side, where the tile lies from `(x0, y0)` — as the
    /// film's own mosaic is drawn, to be set beside it. Lifted by `stops`.
    pub async fn mosaic(
        &mut self,
        store: &ArchivedTiles,
        film: &TilingScheme,
        tiles: &[Coord],
        extent: (u32, u32, u32, u32),
        side: u32,
        stops: f32,
    ) -> Result<image::RgbImage, Error> {
        let (x0, y0, x1, y1) = extent;
        let mut picture = image::RgbImage::from_pixel(
            (x1 - x0 + 1) * side,
            (y1 - y0 + 1) * side,
            image::Rgb([24, 24, 24]),
        );
        let lift = stops.exp2();
        let mut ordered: Vec<(Vec<Coord>, Coord)> = tiles
            .iter()
            .map(|at| (self.tiles_under(film, *at), *at))
            .collect();
        ordered.sort();
        for (under, at) in ordered {
            self.hold(store, &under).await?;
            for j in 0..side {
                for i in 0..side {
                    let rect = (
                        i as f32 / side as f32,
                        j as f32 / side as f32,
                        (i + 1) as f32 / side as f32,
                        (j + 1) as f32 / side as f32,
                    );
                    if let Some(colour) = self.under(film, at, rect) {
                        picture.put_pixel(
                            (at.1 - x0) * side + i,
                            (at.2 - y0) * side + j,
                            image::Rgb(colour.map(|v| stored(v * lift))),
                        );
                    }
                }
            }
            self.let_go(&under);
        }
        Ok(picture)
    }

    /// Sets every tile of a film against the reference, read from the
    /// store as it goes: `film` is the grid the film's imagery is cut on.
    pub async fn set_under(
        &mut self,
        store: &ArchivedTiles,
        film: &TilingScheme,
        observed: &mut Observed,
    ) -> Result<SetAgainst, Error> {
        let mut done = SetAgainst::default();
        // In the order of the reference's own tiles, so that few are held.
        let mut tiles: Vec<(Vec<Coord>, Coord)> = observed
            .tiles
            .keys()
            .map(|at| (self.tiles_under(film, *at), *at))
            .collect();
        tiles.sort();
        for (under, at) in tiles {
            let (came, absent) = self.hold(store, &under).await?;
            done.read += came;
            done.absent += absent;
            let Some(seen) = observed.tiles.get_mut(&at) else {
                continue;
            };
            seen.set_against(|u0, v0, u1, v1| self.under(film, at, (u0, v0, u1, v1)));
            let there = seen.paired.as_deref().map_or(0, |p| {
                p.reference.iter().filter(|c| c[0].is_finite()).count()
            });
            done.tiles += 1;
            done.whole += usize::from(there == PAIRS * PAIRS);
            done.bare += usize::from(there == 0);
            self.let_go(&under);
        }
        Ok(done)
    }
}

/// A picture of one level of a film as it was set against the reference:
/// each tile where it lies, [`PAIRS`] texels a side — its own places on the
/// left, the reference under them on the right, both lifted by `stops`.
/// The two show the same ground, or the reference is not under the film.
/// Returns the picture's size; `None` for a level nothing was seen of.
pub fn picture(
    observed: &Observed,
    level: u8,
    stops: f32,
    path: &Path,
) -> Result<Option<(u32, u32)>, Error> {
    let tiles: Vec<(&Coord, &tuile_radiometry::Paired)> = observed
        .tiles
        .iter()
        .filter(|(at, _)| at.0 == level)
        .filter_map(|(at, tile)| Some((at, tile.paired.as_deref()?)))
        .collect();
    let (Some(x0), Some(x1), Some(y0), Some(y1)) = (
        tiles.iter().map(|(c, _)| c.1).min(),
        tiles.iter().map(|(c, _)| c.1).max(),
        tiles.iter().map(|(c, _)| c.2).min(),
        tiles.iter().map(|(c, _)| c.2).max(),
    ) else {
        return Ok(None);
    };
    let side = PAIRS as u32;
    let (wide, high) = ((x1 - x0 + 1) * side, (y1 - y0 + 1) * side);
    if u64::from(wide) * 2 * u64::from(high) > 64_000_000 {
        return Ok(None);
    }
    let lift = stops.exp2();
    // A place with nothing is drawn as what no ground is.
    let mut picture = image::RgbImage::from_pixel(wide * 2 + side, high, image::Rgb([24, 24, 24]));
    for (at, paired) in tiles {
        for k in 0..PAIRS * PAIRS {
            let (i, j) = ((k % PAIRS) as u32, (k / PAIRS) as u32);
            for (of, across) in [(&paired.tile, 0), (&paired.reference, wide + side)] {
                let colour = if of[k][0].is_finite() {
                    of[k].map(|v| stored(v * lift))
                } else {
                    [255, 0, 255]
                };
                picture.put_pixel(
                    across + (at.1 - x0) * side + i,
                    (at.2 - y0) * side + j,
                    image::Rgb(colour),
                );
            }
        }
    }
    picture.save(path)?;
    Ok(Some((wide * 2 + side, high)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference whose every texel says where it is: red by its column
    /// in the world, green by its row, both in sixteenths of a tile.
    fn held(reference: &mut Reference, level: u8, x: u32, y: u32) {
        let rgb = (0..256usize * 256)
            .flat_map(|k| {
                let (i, j) = (k % 256, k / 256);
                [
                    (x as usize * 16 + i / 16) as u8,
                    (y as usize * 16 + j / 16) as u8,
                    0,
                ]
            })
            .collect();
        reference.held.insert(
            (level, x, y),
            Some(Decoded {
                wide: 256,
                high: 256,
                rgb,
            }),
        );
    }

    #[test]
    fn a_tile_cut_on_one_grid_is_set_against_the_ground_of_another() {
        let (film, scheme) = (TilingScheme::web_mercator(), TilingScheme::geographic());
        let mut reference = Reference::new("reference", scheme, 14);
        // Half as many tiles across the world a level, for the same width.
        assert_eq!(reference.level_under(&film, 5), 4);
        assert_eq!(reference.level_under(&film, 14), 13);
        assert_eq!(reference.level_under(&film, 19), 13);

        // A tile of the film in the northern half, east of Greenwich, and
        // all the reference there could be under it.
        let at = (5u8, 17u32, 11u32);
        for (level, x, y) in reference.tiles_under(&film, at) {
            held(&mut reference, level, x, y);
        }
        assert!(!reference.held.is_empty());
        // Where a place of it is on the ground, worked out from what the
        // two grids are: longitude straight across, latitude through the
        // projection of the film's.
        let place = (0.3f32, 0.7f32);
        let lon = (f64::from(at.1) + f64::from(place.0)) / 32.0 * 360.0 - 180.0;
        let north = 1.0 - 2.0 * (f64::from(at.2) + f64::from(place.1)) / 32.0;
        let lat = (north * std::f64::consts::PI).sinh().atan().to_degrees();
        // Level 4 of the reference: 32 tiles across, 16 down, 16 sixteenths
        // each.
        let column = ((lon + 180.0) / 360.0 * 32.0 * 16.0).floor();
        let row = ((90.0 - lat) / 180.0 * 16.0 * 16.0).floor();
        let found = reference
            .under(
                &film,
                at,
                (
                    place.0 - 0.001,
                    place.1 - 0.001,
                    place.0 + 0.001,
                    place.1 + 0.001,
                ),
            )
            .expect("reference there");
        // A byte holds the sixteenths of sixteen tiles, then goes round.
        let stored = |byte: f64| linear_of((byte as u32 % 256) as u8);
        assert!(
            (found[0] - stored(column)).abs() < 1e-6,
            "{found:?} for column {column}"
        );
        assert!(
            (found[1] - stored(row)).abs() < 1e-6,
            "{found:?} for row {row}"
        );
        // North is up in both: a place further down the film's tile is
        // further down the reference.
        let lower = reference
            .under(&film, at, (0.25, 0.95, 0.26, 0.96))
            .expect("reference there");
        assert!(lower[1] > found[1]);
        // Over a whole tile it is the mean of what is under it.
        let whole = reference
            .under(&film, at, (0.0, 0.0, 1.0, 1.0))
            .expect("reference there");
        assert!(whole[0] > 0.0 && whole[1] > found[1] * 0.5);
        // And where no reference is held, there is none.
        reference.held.clear();
        assert_eq!(reference.under(&film, at, (0.0, 0.0, 1.0, 1.0)), None);
    }
}
