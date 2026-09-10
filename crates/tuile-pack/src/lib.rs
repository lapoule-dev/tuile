// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The pre-baked scene container.
//!
//! # Why this exists
//!
//! Frame 1 of a farm job costs about 500 seconds and is **89 % of a 48-frame
//! job**: the globe from cold, entirely on the CPU — fetching from ion and
//! Bing, decoding quantized-mesh, resampling, draping, baking, encoding PNG.
//! Sixteen processes on one pod each pay it from zero, on a machine rented for
//! its GPUs. And it puts the network, an access token and a convergence race
//! on the critical path of a render.
//!
//! So the work moves. One machine bakes a frame range once and writes it here;
//! the render opens a file. What that buys is not only the time: a render with
//! no network and no traversal is reproducible **by construction** — not
//! because a race was stabilised, but because there is no race.
//!
//! # The shape of a file
//!
//! ```text
//! [ "TUILEPK\0" ][ u64 table length ][ FlatBuffers table ][ blob region ]
//! ```
//!
//! The table is read **in place**: `mmap` it, or hand this a `&[u8]`, and
//! nothing is deserialised — no allocation proportional to the tile count, no
//! parse at open time. That is what matters at several thousand entries.
//!
//! Payloads live after the table, each an independent LZ4 block. Compression
//! and zero-copy stop contradicting each other once they are separated like
//! this: the structure is walked in place, and a payload costs a memcpy only
//! when something asks for it. LZ4 rather than zstd on purpose — this is read
//! on the critical path and written once on a machine that has the time.
//!
//! # What is deliberately *not* here
//!
//! Anything a reader would have to compute. In particular the selection: a
//! pack records, per frame, exactly which tiles were selected. A render does
//! not traverse. The consequence is stated where it hurts — see
//! [`PackWriter::culling`] — because a hole baked in is a hole for ever.

#![allow(clippy::needless_lifetimes, reason = "flatbuffers generates them")]

mod generated {
    #![allow(
        clippy::all,
        clippy::pedantic,
        clippy::unwrap_used,
        clippy::panic,
        dead_code,
        unused_imports,
        non_snake_case,
        non_camel_case_types,
        reason = "flatc output, committed verbatim"
    )]
    include!("tuile_pack_generated.rs");
}

pub use generated::tuile::pack::TextureFormat;
use generated::tuile::pack as fb;

/// What every pack starts with, so a wrong file says so instead of parsing.
pub const MAGIC: &[u8; 8] = b"TUILEPK\0";

/// The layout version. Bumped when an old reader would misread a new file.
pub const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("not a tuile pack (bad magic)")]
    NotAPack,
    #[error("pack layout version {found}, this build reads {VERSION}")]
    Version { found: u32 },
    #[error("truncated: the {what} runs past the end of the file")]
    Truncated { what: &'static str },
    #[error("malformed table: {0}")]
    Malformed(String),
    #[error("this pack is a bake of scene {found}, and the scene is {wanted}")]
    WrongScene { found: String, wanted: String },
    #[error("frame {0} is not in this pack")]
    NoSuchFrame(u32),
    #[error("decompressing {what}: {source}")]
    Decompress {
        what: &'static str,
        #[source]
        source: lz4_flex::block::DecompressError,
    },
}

/// One tile's geometry and imagery, as the writer was handed it.
///
/// Owned, because this is what crosses into a bake: the fields mirror
/// `TuileTile` on the ABI so a round-trip is a comparison of like with like.
#[derive(Debug, Clone, PartialEq)]
pub struct BakedTile {
    pub id: u64,
    pub drape: u64,
    pub origin_ecef: [f64; 3],
    pub positions: Vec<u8>,
    pub normals: Vec<u8>,
    pub uvs: Vec<u8>,
    pub indices: Vec<u8>,
    pub vertex_count: u32,
    pub index_count: u32,
    pub base_color_factor: [f32; 4],
    pub texture: Option<Vec<u8>>,
    pub texture_format: TextureFormat,
}

/// Builds a pack. Payloads are compressed here, on the machine that has time.
pub struct PackWriter {
    scene_digest: String,
    culling: String,
    render_origin: [f64; 3],
    /// The deduplicated pool, in insertion order; `index` maps identity to
    /// position so a tile seen on forty frames is stored once.
    tiles: Vec<BakedTile>,
    index: std::collections::HashMap<(u64, u64), u32>,
    frames: Vec<(u32, Vec<u32>)>,
}

impl PackWriter {
    pub fn new(scene_digest: impl Into<String>, render_origin: [f64; 3]) -> Self {
        Self {
            scene_digest: scene_digest.into(),
            // The honest default. A writer that says nothing about how it
            // culled has to be assumed to have culled the usual way, and
            // `culling` is what a reader complains about.
            culling: "full".into(),
            render_origin,
            tiles: Vec::new(),
            index: std::collections::HashMap::new(),
            frames: Vec::new(),
        }
    }

    /// States how the selection was culled while this was baked.
    ///
    /// **Read this before baking anything with it set to something else.** A
    /// pack freezes its selection. If the traversal that produced it left a
    /// patch of ground unselected, that patch is bare in every render made
    /// from the pack, for ever, and nothing downstream reports it — a count of
    /// tiles is blind to ground nobody selected. Recording how the cull was
    /// configured is what stops a pack baked under a known-imperfect cull from
    /// later passing for a good one.
    pub fn culling(mut self, how: impl Into<String>) -> Self {
        self.culling = how.into();
        self
    }

    /// Adds one frame's selection, deduplicating tiles against every frame
    /// already added.
    pub fn frame(&mut self, frame: u32, tiles: impl IntoIterator<Item = BakedTile>) {
        let mut refs = Vec::new();
        for tile in tiles {
            let key = (tile.id, tile.drape);
            let at = match self.index.get(&key) {
                Some(&at) => at,
                None => {
                    let at = self.tiles.len() as u32;
                    self.index.insert(key, at);
                    self.tiles.push(tile);
                    at
                }
            };
            refs.push(at);
        }
        self.frames.push((frame, refs));
    }

    /// Serialises the whole container.
    ///
    /// Two passes by construction: the blob region is built first, because a
    /// [`fb::Block`] cannot be written until its offset and its compressed
    /// length are known.
    pub fn finish(self) -> Vec<u8> {
        let mut blobs: Vec<u8> = Vec::new();
        let put = |bytes: &[u8], blobs: &mut Vec<u8>| -> fb::Block {
            let offset = blobs.len() as u64;
            let stored = lz4_flex::block::compress(bytes);
            let block = fb::Block::new(offset, stored.len() as u32, bytes.len() as u32);
            blobs.extend_from_slice(&stored);
            block
        };

        let mut built = Vec::with_capacity(self.tiles.len());
        for tile in &self.tiles {
            built.push((
                tile,
                put(&tile.positions, &mut blobs),
                put(&tile.normals, &mut blobs),
                put(&tile.uvs, &mut blobs),
                put(&tile.indices, &mut blobs),
                tile.texture.as_ref().map(|t| put(t, &mut blobs)),
            ));
        }

        let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(1 << 20);
        let tiles: Vec<_> = built
            .iter()
            .map(|(tile, pos, nrm, uv, idx, tex)| {
                let origin = fbb.create_vector(&tile.origin_ecef);
                let factor = fbb.create_vector(&tile.base_color_factor);
                let mut b = fb::TileBuilder::new(&mut fbb);
                b.add_id(tile.id);
                b.add_drape(tile.drape);
                b.add_origin_ecef(origin);
                b.add_vertex_count(tile.vertex_count);
                b.add_index_count(tile.index_count);
                b.add_base_color_factor(factor);
                b.add_positions(pos);
                b.add_normals(nrm);
                b.add_uvs(uv);
                b.add_indices(idx);
                if let Some(tex) = tex {
                    b.add_texture(tex);
                }
                b.add_texture_format(tile.texture_format);
                b.finish()
            })
            .collect();
        let tiles = fbb.create_vector(&tiles);

        let frames: Vec<_> = self
            .frames
            .iter()
            .map(|(n, refs)| {
                let refs = fbb.create_vector(refs);
                let mut b = fb::FrameBuilder::new(&mut fbb);
                b.add_frame(*n);
                b.add_tiles(refs);
                b.finish()
            })
            .collect();
        let frames = fbb.create_vector(&frames);

        let digest = fbb.create_string(&self.scene_digest);
        let culling = fbb.create_string(&self.culling);
        let origin = fbb.create_vector(&self.render_origin);
        let first = self.frames.first().map_or(0, |(n, _)| *n);
        let last = self.frames.last().map_or(0, |(n, _)| *n);
        let mut b = fb::PackBuilder::new(&mut fbb);
        b.add_version(VERSION);
        b.add_scene_digest(digest);
        b.add_first_frame(first);
        b.add_last_frame(last);
        b.add_render_origin(origin);
        b.add_culling(culling);
        b.add_tiles(tiles);
        b.add_frames(frames);
        let root = b.finish();
        fbb.finish(root, Some("TUIL"));
        let table = fbb.finished_data();

        let mut out = Vec::with_capacity(MAGIC.len() + 8 + table.len() + blobs.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(table.len() as u64).to_le_bytes());
        out.extend_from_slice(table);
        out.extend_from_slice(&blobs);
        out
    }
}

/// A pack, read in place.
///
/// Borrows the bytes: hand it a `mmap`, a `Vec`, or a slice of either. Nothing
/// is copied until [`Pack::payload`] is asked for one.
pub struct Pack<'a> {
    root: fb::Pack<'a>,
    blobs: &'a [u8],
}

impl<'a> Pack<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, PackError> {
        if bytes.len() < MAGIC.len() + 8 || &bytes[..MAGIC.len()] != MAGIC {
            return Err(PackError::NotAPack);
        }
        let mut len = [0u8; 8];
        len.copy_from_slice(&bytes[MAGIC.len()..MAGIC.len() + 8]);
        let table_len = u64::from_le_bytes(len) as usize;
        let start = MAGIC.len() + 8;
        let end = start
            .checked_add(table_len)
            .filter(|e| *e <= bytes.len())
            .ok_or(PackError::Truncated { what: "table" })?;
        let root = fb::root_as_pack(&bytes[start..end])
            .map_err(|e| PackError::Malformed(e.to_string()))?;
        if root.version() != VERSION {
            return Err(PackError::Version {
                found: root.version(),
            });
        }
        Ok(Self {
            root,
            blobs: &bytes[end..],
        })
    }

    /// Fails unless this pack is the bake of the scene the caller means.
    ///
    /// A render that silently accepts a pack from another scene renders the
    /// wrong ground and reports success, which is the one failure mode a
    /// pre-baked pipeline adds that a live one does not have.
    pub fn expect_scene(&self, wanted: &str) -> Result<(), PackError> {
        let found = self.root.scene_digest().unwrap_or_default();
        if found == wanted {
            Ok(())
        } else {
            Err(PackError::WrongScene {
                found: found.to_string(),
                wanted: wanted.to_string(),
            })
        }
    }

    pub fn scene_digest(&self) -> &'a str {
        self.root.scene_digest().unwrap_or_default()
    }

    /// How the selection in this pack was culled. See [`PackWriter::culling`].
    pub fn culling(&self) -> &'a str {
        self.root.culling().unwrap_or_default()
    }

    pub fn render_origin(&self) -> [f64; 3] {
        let v = self.root.render_origin();
        match v {
            Some(v) if v.len() == 3 => [v.get(0), v.get(1), v.get(2)],
            _ => [0.0; 3],
        }
    }

    pub fn frame_range(&self) -> (u32, u32) {
        (self.root.first_frame(), self.root.last_frame())
    }

    pub fn tile_count(&self) -> usize {
        self.root.tiles().map_or(0, |t| t.len())
    }

    /// The tiles one frame selected, in the order the bake recorded them.
    pub fn frame(&self, frame: u32) -> Result<Vec<fb::Tile<'a>>, PackError> {
        let frames = self
            .root
            .frames()
            .ok_or_else(|| PackError::Malformed("no frames".into()))?;
        let tiles = self
            .root
            .tiles()
            .ok_or_else(|| PackError::Malformed("no tiles".into()))?;
        let entry = frames
            .iter()
            .find(|f| f.frame() == frame)
            .ok_or(PackError::NoSuchFrame(frame))?;
        let refs = entry
            .tiles()
            .ok_or_else(|| PackError::Malformed("a frame with no selection".into()))?;
        let mut out = Vec::with_capacity(refs.len());
        for at in refs.iter() {
            let at = at as usize;
            if at >= tiles.len() {
                return Err(PackError::Malformed(format!(
                    "frame {frame} names tile {at} of {}",
                    tiles.len()
                )));
            }
            out.push(tiles.get(at));
        }
        Ok(out)
    }

    /// Decompresses one payload. The only copy a pack ever makes.
    pub fn payload(&self, block: &fb::Block, what: &'static str) -> Result<Vec<u8>, PackError> {
        let at = block.offset() as usize;
        let end = at
            .checked_add(block.stored() as usize)
            .filter(|e| *e <= self.blobs.len())
            .ok_or(PackError::Truncated { what })?;
        lz4_flex::block::decompress(&self.blobs[at..end], block.raw() as usize)
            .map_err(|source| PackError::Decompress { what, source })
    }

    /// Everything one tile carries, back in the shape it was baked from.
    ///
    /// This is what the round-trip test compares, and it is why it can: the
    /// only faithful test of a substitute is that it hands back exactly what
    /// the thing it replaces would have.
    pub fn baked(&self, tile: &fb::Tile<'a>) -> Result<BakedTile, PackError> {
        let block = |b: Option<&fb::Block>, what: &'static str| match b {
            Some(b) => self.payload(b, what),
            None => Ok(Vec::new()),
        };
        let origin = tile.origin_ecef();
        let origin = match origin {
            Some(v) if v.len() == 3 => [v.get(0), v.get(1), v.get(2)],
            _ => return Err(PackError::Malformed("a tile with no origin".into())),
        };
        let factor = tile.base_color_factor();
        let factor = match factor {
            Some(v) if v.len() == 4 => [v.get(0), v.get(1), v.get(2), v.get(3)],
            _ => [1.0; 4],
        };
        Ok(BakedTile {
            id: tile.id(),
            drape: tile.drape(),
            origin_ecef: origin,
            positions: block(tile.positions(), "positions")?,
            normals: block(tile.normals(), "normals")?,
            uvs: block(tile.uvs(), "uvs")?,
            indices: block(tile.indices(), "indices")?,
            vertex_count: tile.vertex_count(),
            index_count: tile.index_count(),
            base_color_factor: factor,
            texture: match tile.texture() {
                Some(b) => Some(self.payload(b, "texture")?),
                None => None,
            },
            texture_format: tile.texture_format(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_tile(id: u64, drape: u64) -> BakedTile {
        BakedTile {
            id,
            drape,
            origin_ecef: [4_700_959.945_556_212, 186_133.405_820_756_92, 4_314_027.341_856_181],
            // Deliberately not round numbers and not compressible: a payload
            // that happens to compress to nothing proves nothing about the
            // block plumbing.
            positions: (0..1024u32).flat_map(|i| (i.wrapping_mul(2_654_435_761)).to_le_bytes()).collect(),
            normals: (0..512u32).flat_map(|i| (i ^ 0xdead_beef).to_le_bytes()).collect(),
            uvs: (0..256u32).flat_map(|i| i.to_le_bytes()).collect(),
            indices: (0..300u32).flat_map(|i| (i % 97).to_le_bytes()).collect(),
            vertex_count: 341,
            index_count: 300,
            base_color_factor: [1.0, 0.5, 0.25, 1.0],
            texture: Some((0..4096u32).map(|i| (i % 251) as u8).collect()),
            texture_format: TextureFormat::Png,
        }
    }

    /// The test the whole crate exists to pass.
    ///
    /// A pack is a substitute for a live session, and a substitute that hands
    /// back *nearly* the same geometry is not a substitute — it is a second
    /// implementation, and the difference shows up as a render that does not
    /// match the one it was supposed to reproduce. Byte for byte, or nothing.
    #[test]
    fn a_tile_survives_the_round_trip_byte_for_byte() {
        let mut w = PackWriter::new("scene-abc", [1.0, 2.0, 3.0]);
        let a = a_tile(7, 100);
        let b = a_tile(9, 200);
        w.frame(1, [a.clone(), b.clone()]);
        let bytes = w.finish();

        let pack = Pack::open(&bytes).expect("opens");
        assert_eq!(pack.scene_digest(), "scene-abc");
        assert_eq!(pack.render_origin(), [1.0, 2.0, 3.0]);
        assert_eq!(pack.frame_range(), (1, 1));
        let tiles = pack.frame(1).expect("frame 1");
        assert_eq!(tiles.len(), 2);
        assert_eq!(pack.baked(&tiles[0]).expect("tile 0"), a);
        assert_eq!(pack.baked(&tiles[1]).expect("tile 1"), b);
    }

    /// A tile seen on many frames is stored once — the only reason a pack of a
    /// long shot is not the sum of its frames.
    #[test]
    fn the_same_tile_at_the_same_draping_is_stored_once() {
        let mut w = PackWriter::new("s", [0.0; 3]);
        for frame in 1..=40u32 {
            w.frame(frame, [a_tile(7, 100)]);
        }
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert_eq!(pack.tile_count(), 1, "forty frames, one tile");
        assert_eq!(pack.frame_range(), (1, 40));
        for frame in 1..=40u32 {
            assert_eq!(pack.frame(frame).expect("every frame is present").len(), 1);
        }
    }

    /// …but a re-draping is a different picture and must not be folded in.
    ///
    /// `drape` is in the identity for exactly this reason: a tile keeps its id
    /// across frames while its imagery is recomposed as the camera moves.
    /// Deduplicating on the id alone would hand frame 40 the pixels of frame 1.
    #[test]
    fn a_redraped_tile_is_a_different_tile() {
        let mut w = PackWriter::new("s", [0.0; 3]);
        w.frame(1, [a_tile(7, 100)]);
        w.frame(2, [a_tile(7, 200)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert_eq!(pack.tile_count(), 2);
        assert_eq!(pack.frame(1).expect("f1")[0].drape(), 100);
        assert_eq!(pack.frame(2).expect("f2")[0].drape(), 200);
    }

    #[test]
    fn the_wrong_scene_is_refused_rather_than_rendered() {
        let bytes = PackWriter::new("scene-abc", [0.0; 3]).finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert!(pack.expect_scene("scene-abc").is_ok());
        assert!(matches!(
            pack.expect_scene("scene-xyz"),
            Err(PackError::WrongScene { .. })
        ));
    }

    /// A pack baked under a known-imperfect cull must never pass for a good
    /// one: the admission travels with the data.
    #[test]
    fn the_pack_carries_how_it_was_culled() {
        let bytes = PackWriter::new("s", [0.0; 3]).culling("disabled").finish();
        assert_eq!(Pack::open(&bytes).expect("opens").culling(), "disabled");
        let bytes = PackWriter::new("s", [0.0; 3]).finish();
        assert_eq!(Pack::open(&bytes).expect("opens").culling(), "full");
    }

    #[test]
    fn something_that_is_not_a_pack_says_so() {
        assert!(matches!(
            Pack::open(b"not a pack at all"),
            Err(PackError::NotAPack)
        ));
        let mut bytes = PackWriter::new("s", [0.0; 3]).finish();
        bytes.truncate(MAGIC.len() + 4);
        assert!(matches!(Pack::open(&bytes), Err(PackError::NotAPack)));
    }

    #[test]
    fn a_frame_the_pack_does_not_hold_is_an_error_not_an_empty_selection() {
        let mut w = PackWriter::new("s", [0.0; 3]);
        w.frame(1, [a_tile(1, 1)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert!(matches!(pack.frame(2), Err(PackError::NoSuchFrame(2))));
    }
}

/// A stable name for the scene a pack is the bake of.
///
/// FNV-1a over whatever the caller declares the scene to be — the camera path,
/// the asset ids, the viewport, the settings that move a selection. Stable
/// across processes and architectures, which `DefaultHasher` explicitly is
/// not, and a digest that changed for its own reasons would be worse than none.
///
/// It exists so the two halves of a split pipeline can disagree **loudly**.
/// A bake writes it; a render computes it from the scene it was asked for and
/// refuses a pack that answers differently. Without it, rendering last week's
/// bake of a different trajectory is a silent success.
pub fn scene_digest(parts: &[&[u8]]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (i, part) in parts.iter().enumerate() {
        // The separator is what stops ("ab", "c") and ("a", "bc") colliding.
        for byte in part.iter().copied().chain(std::iter::once(0xff)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= i as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The pack format is little-endian, and the payloads are handed back to a C
/// ABI as raw memory.
///
/// Stated as a compile error rather than left implicit: on a big-endian target
/// a reader would hand a renderer byte-swapped vertices, which does not fail —
/// it draws a scrambled globe. Every target this ships to is little-endian, so
/// this is a guard against a surprise, not a limitation anyone is living with.
#[cfg(target_endian = "big")]
compile_error!("tuile-pack payloads are little-endian; add a conversion first");

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn the_digest_is_the_same_string_every_run() {
        // Pinned: the whole point is that two processes agree.
        assert_eq!(scene_digest(&[b"orbit", b"1:48"]), "ef3ab95ffe76ab43");
    }

    #[test]
    fn the_parts_cannot_be_reshuffled_into_the_same_name() {
        assert_ne!(scene_digest(&[b"ab", b"c"]), scene_digest(&[b"a", b"bc"]));
        assert_ne!(scene_digest(&[b"a", b"b"]), scene_digest(&[b"b", b"a"]));
        assert_ne!(scene_digest(&[b"a"]), scene_digest(&[b"a", b""]));
    }
}
