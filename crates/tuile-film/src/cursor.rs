// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::HashSet;

use tuile_pack::{fb, BakedView, Pack, PackError};

/// A tile's identity across frames: the same key the bake deduplicates on.
///
/// The drape is part of it on purpose. The same mesh under a sharper mosaic is
/// a different tile to draw, and keying on the id alone would keep the blurry
/// texture on screen for the rest of the film.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub id: u64,
    pub drape: u64,
}

impl TileKey {
    pub fn of(tile: &fb::Tile<'_>) -> Self {
        Self {
            id: tile.id(),
            drape: tile.drape(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FilmError {
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error("frame {frame} is outside the pack's {first}..={last}")]
    OutOfRange { frame: u32, first: u32, last: u32 },
}

/// What changes between the previous frame and this one.
///
/// A renderer applies it in this order: create every `enter`, draw `selection`,
/// then release every `leave`. Releasing first would be cheaper on memory and
/// is the one order that can leave a frame without its ground.
pub struct FrameDiff<'a> {
    pub frame: u32,
    pub view: BakedView,
    /// Tiles this frame draws that the previous frame did not.
    pub enter: Vec<fb::Tile<'a>>,
    /// Tiles the previous frame drew that this one does not.
    pub leave: Vec<TileKey>,
    /// Everything this frame draws, in the order the bake recorded it.
    pub selection: Vec<TileKey>,
}

/// Walks a pack forward from any frame, one diff at a time.
///
/// Starting mid-film is the normal case, not an edge: each worker of a
/// parallel render owns a slice, and its first diff simply enters everything.
///
/// It holds no reference to the pack, only what was resident, so its owner can
/// hold the pack's bytes alongside it and lend them per frame.
pub struct Cursor {
    next: u32,
    last: u32,
    resident: HashSet<TileKey>,
}

impl Cursor {
    /// A cursor over `first..=last`, which must lie inside the pack's range.
    pub fn new(pack: &Pack<'_>, first: u32, last: u32) -> Result<Self, FilmError> {
        let (lo, hi) = pack.frame_range();
        for frame in [first, last] {
            if frame < lo || frame > hi {
                return Err(FilmError::OutOfRange {
                    frame,
                    first: lo,
                    last: hi,
                });
            }
        }
        Ok(Self {
            next: first,
            last,
            resident: HashSet::new(),
        })
    }

    /// Tiles currently resident, i.e. drawn by the last frame returned.
    pub fn resident(&self) -> usize {
        self.resident.len()
    }

    /// Frames left in the range.
    pub fn remaining(&self) -> u32 {
        (self.last + 1).saturating_sub(self.next)
    }

    /// The next frame's diff, or `None` past the end of the range. `pack`
    /// must be the pack the cursor was made for.
    pub fn advance<'a>(&mut self, pack: &Pack<'a>) -> Option<Result<FrameDiff<'a>, FilmError>> {
        if self.next > self.last {
            return None;
        }
        let frame = self.next;
        self.next += 1;
        Some(self.diff(pack, frame))
    }

    fn diff<'a>(&mut self, pack: &Pack<'a>, frame: u32) -> Result<FrameDiff<'a>, FilmError> {
        let view = pack.view_of(frame)?;
        let tiles = pack.frame(frame)?;
        let mut now = HashSet::with_capacity(tiles.len());
        let mut enter = Vec::new();
        let mut selection = Vec::with_capacity(tiles.len());
        for tile in tiles {
            let key = TileKey::of(&tile);
            if !now.insert(key) {
                continue;
            }
            selection.push(key);
            if !self.resident.contains(&key) {
                enter.push(tile);
            }
        }
        let mut leave: Vec<TileKey> = self.resident.difference(&now).copied().collect();
        leave.sort_unstable();
        self.resident = now;
        Ok(FrameDiff {
            frame,
            view,
            enter,
            leave,
            selection,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tuile_pack::{BakedTile, PackWriter, TextureFormat};

    pub(crate) fn tile(id: u64, drape: u64) -> BakedTile {
        let positions: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let normals: Vec<u8> = [0.0f32, 0.0, 1.0]
            .repeat(3)
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let uvs: Vec<u8> = [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        BakedTile {
            id,
            drape,
            origin_ecef: [6_378_137.0, 0.0, 0.0],
            positions,
            normals,
            uvs,
            indices: [0u32, 1, 2].iter().flat_map(|v| v.to_le_bytes()).collect(),
            vertex_count: 3,
            index_count: 3,
            base_color_factor: [1.0; 4],
            texture: None,
            texture_format: TextureFormat::None,
            refs: None,
        }
    }

    pub(crate) fn view(x: f64) -> BakedView {
        BakedView {
            position: [x, 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            viewport_px: [640.0, 480.0],
            fovy_rad: 1.0,
        }
    }

    /// Frames 10..=12: {a, b}, {b, c}, {b, c'} where c' is c re-draped.
    pub(crate) fn three_frames() -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut w = PackWriter::new("scene", [6_378_137.0, 0.0, 0.0], dir.path().join("blob"))
            .expect("writer");
        w.frame(10, view(6_379_000.0), [tile(1, 0), tile(2, 0)]);
        w.frame(11, view(6_378_900.0), [tile(2, 0), tile(3, 0)]);
        w.frame(12, view(6_378_800.0), [tile(2, 0), tile(3, 7)]);
        let path = dir.path().join("film.tuilepack");
        w.finish_to(&path).expect("finish");
        std::fs::read(path).expect("read")
    }

    fn key(id: u64, drape: u64) -> TileKey {
        TileKey { id, drape }
    }

    #[test]
    fn diffs_enter_and_leave() {
        let bytes = three_frames();
        let pack = Pack::open(&bytes).expect("open");
        let mut c = Cursor::new(&pack, 10, 12).expect("cursor");

        let f = c.advance(&pack).expect("10").expect("ok");
        assert_eq!(f.frame, 10);
        assert_eq!(
            f.enter.iter().map(TileKey::of).collect::<Vec<_>>(),
            [key(1, 0), key(2, 0)]
        );
        assert!(f.leave.is_empty());

        let f = c.advance(&pack).expect("11").expect("ok");
        assert_eq!(
            f.enter.iter().map(TileKey::of).collect::<Vec<_>>(),
            [key(3, 0)]
        );
        assert_eq!(f.leave, [key(1, 0)]);
        assert_eq!(f.selection, [key(2, 0), key(3, 0)]);

        // A new drape over the same mesh is a new tile, and the old one leaves.
        let f = c.advance(&pack).expect("12").expect("ok");
        assert_eq!(
            f.enter.iter().map(TileKey::of).collect::<Vec<_>>(),
            [key(3, 7)]
        );
        assert_eq!(f.leave, [key(3, 0)]);
        assert_eq!(c.resident(), 2);

        assert!(c.advance(&pack).is_none());
    }

    #[test]
    fn a_slice_starting_mid_film_enters_everything() {
        let bytes = three_frames();
        let pack = Pack::open(&bytes).expect("open");
        let mut c = Cursor::new(&pack, 11, 11).expect("cursor");
        let f = c.advance(&pack).expect("11").expect("ok");
        assert_eq!(f.enter.len(), 2);
        assert!(f.leave.is_empty());
        assert!(c.advance(&pack).is_none());
    }

    #[test]
    fn a_range_outside_the_pack_is_refused() {
        let bytes = three_frames();
        let pack = Pack::open(&bytes).expect("open");
        assert!(matches!(
            Cursor::new(&pack, 9, 12),
            Err(FilmError::OutOfRange { frame: 9, .. })
        ));
    }
}
