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

/// The generated accessors, for a consumer that walks a pack in place rather
/// than materialising it. `Pack::frame` hands these back.
pub use generated::tuile::pack as fb;

/// What every pack starts with, so a wrong file says so instead of parsing.
pub const MAGIC: &[u8; 8] = b"TUILEPK\0";

/// The layout version. Bumped when an old reader would misread a new file.
pub const VERSION: u32 = 2;

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
    #[error(
        "no baked frame answers this camera: the nearest is frame {frame}, \
         {metres:.3} m and {radians:.6} rad away (tolerances {VIEW_TOLERANCE_M} m, \
         {VIEW_TOLERANCE_RAD} rad). The stage and the pack are not of the same shot."
    )]
    NoSuchView {
        frame: u32,
        metres: f64,
        radians: f64,
    },
    #[error("this pack holds no frames at all")]
    Empty,
    #[error(
        "the payloads are not the ones this pack was written with \
         (digest {found:016x}, expected {expected:016x}) — it was damaged in \
         transit or on disk"
    )]
    Corrupt { found: u64, expected: u64 },
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

/// One camera, twelve doubles — the same shape the ABI's `TuileViewState`
/// carries, and the key a frame is addressed by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BakedView {
    pub position: [f64; 3],
    pub direction: [f64; 3],
    pub up: [f64; 3],
    pub viewport_px: [f64; 2],
    pub fovy_rad: f64,
}

impl BakedView {
    fn to_fb(self) -> fb::View {
        fb::View::new(
            self.position[0],
            self.position[1],
            self.position[2],
            self.direction[0],
            self.direction[1],
            self.direction[2],
            self.up[0],
            self.up[1],
            self.up[2],
            self.viewport_px[0],
            self.viewport_px[1],
            self.fovy_rad,
        )
    }

    fn from_fb(v: &fb::View) -> Self {
        Self {
            position: [v.px(), v.py(), v.pz()],
            direction: [v.dx(), v.dy(), v.dz()],
            up: [v.ux(), v.uy(), v.uz()],
            viewport_px: [v.vw(), v.vh()],
            fovy_rad: v.fovy(),
        }
    }

    /// How far this camera is from another, in metres.
    ///
    /// Position only. Orientation is checked separately because the two have
    /// different units and mixing them into one number makes the tolerance
    /// meaningless — and because a camera in the right place looking the wrong
    /// way is a different kind of mistake from a camera in the wrong place.
    fn metres_from(&self, other: &Self) -> f64 {
        let d = [
            self.position[0] - other.position[0],
            self.position[1] - other.position[1],
            self.position[2] - other.position[2],
        ];
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }

    /// The angle between where the two cameras look, in radians.
    fn radians_from(&self, other: &Self) -> f64 {
        let norm = |v: [f64; 3]| {
            let m = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            if m > 0.0 {
                [v[0] / m, v[1] / m, v[2] / m]
            } else {
                v
            }
        };
        let a = norm(self.direction);
        let b = norm(other.direction);
        (a[0] * b[0] + a[1] * b[1] + a[2] * b[2])
            .clamp(-1.0, 1.0)
            .acos()
    }
}

/// How far a render's camera may sit from a baked one and still be it.
///
/// Not zero, and the reason is worth stating: the camera does not travel from
/// the bake to the render as twelve doubles. It goes through a `.usda`
/// manifest as a transform matrix and comes back out of Hydra decomposed, so
/// the last bits do not survive. A metre is a thousand times looser than that
/// round trip needs and a thousand times tighter than the gap between two
/// frames of any real shot, which is what makes the match unambiguous.
pub const VIEW_TOLERANCE_M: f64 = 1.0;
/// Likewise for where it looks: a milliradian is far below what a decomposition
/// loses and far above nothing.
pub const VIEW_TOLERANCE_RAD: f64 = 1.0e-3;

/// Builds a pack. Payloads are compressed here, on the machine that has time.
pub struct PackWriter {
    scene_digest: String,
    culling: String,
    render_origin: [f64; 3],
    /// The deduplicated pool, in insertion order; `index` maps identity to
    /// position so a tile seen on forty frames is stored once.
    ///
    /// **Metadata only.** The pixels and the vertices are compressed on the
    /// way in and pushed straight into `blob`; what survives here is five
    /// [`fb::Block`]s and a handful of scalars — sixty-odd bytes a tile,
    /// whatever the tile weighs.
    tiles: Vec<StoredTile>,
    index: std::collections::HashMap<(u64, u64), u32>,
    frames: Vec<(u32, BakedView, Vec<u32>)>,
    blob: BlobSink,
    /// La première erreur d'écriture du blob, gardée pour être rendue par
    /// `finish_to`. Voir [`PackWriter::put`].
    spill_error: Option<std::io::Error>,
}

/// One tile, once its payloads are already in the blob.
struct StoredTile {
    id: u64,
    drape: u64,
    origin_ecef: [f64; 3],
    vertex_count: u32,
    index_count: u32,
    base_color_factor: [f32; 4],
    positions: fb::Block,
    normals: fb::Block,
    uvs: fb::Block,
    indices: fb::Block,
    texture: Option<fb::Block>,
    texture_format: TextureFormat,
}

/// The compressed payloads, written to a file as they are produced.
///
/// # Why there is only one of these
///
/// The blob used to be built in one go at the end, and the cost of that was
/// not one copy but three, all live at the same instant: every tile held
/// **decompressed** until `finish`, then the whole blob compressed beside it,
/// then a third buffer concatenating table and blob before a single
/// `fs::write`. The peak was roughly three times the finished pack, on a
/// machine that has to hold the tile cache as well; a minute of film did not
/// fit in sixteen gigabytes.
///
/// The offsets a [`fb::Block`] carries are relative to the start of the blob,
/// so they never depended on the table's size — nothing ever required the blob
/// to be in memory. Writing it out as it is produced makes the peak
/// independent of how long the bake is: 1440 frames cost the resident bytes
/// that 48 cost.
///
/// An in-memory variant existed beside this one for a few minutes and was
/// removed on purpose. Two paths mean the short one is the one the tests
/// exercise and the long one is the one that runs in anger — which is the
/// arrangement that let the peak go unnoticed in the first place.
struct BlobSink {
    file: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
    len: u64,
    /// FNV-1a folded in as the bytes go past, because `blob_digest` is over
    /// the whole blob and there is no longer a whole blob to hash.
    digest: u64,
}

impl BlobSink {
    fn create(path: std::path::PathBuf) -> std::io::Result<Self> {
        let file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path)?);
        Ok(Self {
            file,
            path,
            len: 0,
            digest: FNV_OFFSET,
        })
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        self.file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        self.digest = fnv1a_fold(self.digest, bytes);
        Ok(())
    }

    fn digest(&self) -> u64 {
        self.digest
    }

    /// Hands the blob to `out` and gives up its storage.
    ///
    /// Read back a megabyte at a time and removed. Never mapped, never
    /// slurped: the point of spilling was to stop holding the blob, and
    /// slurping it here would give every byte back at the worst moment.
    fn drain_into(mut self, out: &mut dyn std::io::Write) -> std::io::Result<u64> {
        use std::io::Write as _;
        self.file.flush()?;
        drop(self.file);
        let mut source =
            std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(&self.path)?);
        let copied = std::io::copy(&mut source, out)?;
        drop(source);
        // Best effort: a leftover temporary is untidy; a failed bake that also
        // fails to clean up is not worse than the failure.
        let _ = std::fs::remove_file(&self.path);
        if copied != self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("blob spill is {copied} bytes, expected {}", self.len),
            ));
        }
        Ok(copied)
    }
}

/// Une texture de ce format porte-t-elle déjà sa propre compression ?
///
/// La seule fonction qui décide, lue par l'écriture ET par la lecture. Il n'y
/// a pas de champ pour redire ce que `Tile.texture_format` dit déjà : un
/// second champ serait une valeur en double, donc deux choses à garder
/// d'accord.
///
/// Mesuré sur une cuisson de 24 frames en septembre 2026 : le LZ4 sur des PNG
/// rend le pack **plus gros** de 0,2 %, et les textures y font 94 % des
/// octets. On payait donc une compression à la cuisson et une décompression à
/// chaque lecture pour perdre de la place. La géométrie, elle, garde de la
/// structure à exploiter : 26 % sur les indices, 48 % sur les normales.
fn carries_its_own_compression(format: TextureFormat) -> bool {
    match format {
        // PNG, c'est-à-dire du DEFLATE.
        TextureFormat::Png => true,
        _ => false,
    }
}

impl PackWriter {
    /// A writer that keeps its blob in `blob_path` while it fills.
    ///
    /// The path is required, not optional: see `BlobSink`. It is removed by
    /// [`PackWriter::finish_to`]; a bake that dies before then leaves it
    /// behind, which is the right trade — a stray temporary is cheap, and a
    /// crashed bake is worth inspecting.
    pub fn new(
        scene_digest: impl Into<String>,
        render_origin: [f64; 3],
        blob_path: impl Into<std::path::PathBuf>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            scene_digest: scene_digest.into(),
            // The honest default. A writer that says nothing about how it
            // culled has to be assumed to have culled the usual way, and
            // `culling` is what a reader complains about.
            culling: "full".into(),
            render_origin,
            tiles: Vec::new(),
            index: std::collections::HashMap::new(),
            frames: Vec::new(),
            blob: BlobSink::create(blob_path.into())?,
            spill_error: None,
        })
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

    /// Whether this identity is already stored, and so needs no bytes.
    ///
    /// A camera orbit re-selects nearly the same tiles every frame, and every
    /// one of them arrives here already held. Asking first lets a caller skip
    /// building the tile at all — the PNG, the four geometry buffers — instead
    /// of building it so that [`PackWriter::frame`] can drop it. See
    /// [`FrameWriter::push_known`].
    pub fn holds(&self, id: u64, drape: u64) -> bool {
        self.index.contains_key(&(id, drape))
    }

    /// Opens a frame that takes its tiles **one at a time**.
    ///
    /// [`PackWriter::frame`] wants every tile of the frame at once, so a caller
    /// builds them all into a vector first: for one frame that is every PNG and
    /// every geometry buffer of the selection, alive together, on top of the
    /// frame they were built from. The writer does not need them together — it
    /// compresses and spills each one as it arrives — so this hands them over
    /// as they are made and the peak becomes one tile.
    ///
    /// It also lets the caller propagate an error from the middle of a frame,
    /// which an `IntoIterator<Item = BakedTile>` cannot.
    pub fn begin_frame(&mut self, frame: u32, view: BakedView) -> FrameWriter<'_> {
        FrameWriter {
            writer: self,
            frame,
            view,
            refs: Vec::new(),
        }
    }

    /// Adds one frame's selection, deduplicating tiles against every frame
    /// already added.
    pub fn frame(
        &mut self,
        frame: u32,
        view: BakedView,
        tiles: impl IntoIterator<Item = BakedTile>,
    ) {
        let mut open = self.begin_frame(frame, view);
        for tile in tiles {
            open.push(tile);
        }
        open.end();
    }

    /// Stores one tile if it is new, and says where it landed.
    fn intern(&mut self, tile: BakedTile) -> u32 {
        {
            let key = (tile.id, tile.drape);
            let at = match self.index.get(&key) {
                Some(&at) => at,
                None => {
                    let at = self.tiles.len() as u32;
                    self.index.insert(key, at);
                    // Compressé et déversé ICI, pas à la fin. C'est ce qui
                    // rend le pic indépendant de la longueur du film : une
                    // tuile qui vient d'entrer ne pèse plus que ses cinq
                    // blocs dès la ligne suivante.
                    let stored = StoredTile {
                        id: tile.id,
                        drape: tile.drape,
                        origin_ecef: tile.origin_ecef,
                        vertex_count: tile.vertex_count,
                        index_count: tile.index_count,
                        base_color_factor: tile.base_color_factor,
                        // La géométrie garde de la structure à exploiter —
                        // 26 à 48 % mesurés. L'image, elle, porte déjà son
                        // propre codec.
                        positions: self.put(&tile.positions, true),
                        normals: self.put(&tile.normals, true),
                        uvs: self.put(&tile.uvs, true),
                        indices: self.put(&tile.indices, true),
                        texture: tile.texture.as_deref().map(|t| {
                            self.put(t, !carries_its_own_compression(tile.texture_format))
                        }),
                        texture_format: tile.texture_format,
                    };
                    self.tiles.push(stored);
                    at
                }
            };
            at
        }
    }

    /// Compresses one payload into the blob and describes where it landed.
    ///
    /// An I/O error is recorded rather than returned, so that `frame` keeps
    /// its infallible signature: a bake that cannot write its blob is going to
    /// fail, but it should fail once, at [`PackWriter::finish_to`], with the
    /// error that caused it — not by poisoning every call that follows.
    fn put(&mut self, bytes: &[u8], compress: bool) -> fb::Block {
        let offset = self.blob.len();
        let compressed;
        let stored: &[u8] = if compress {
            compressed = lz4_flex::block::compress(bytes);
            &compressed
        } else {
            bytes
        };
        let block = fb::Block::new(offset, stored.len() as u32, bytes.len() as u32);
        if let Err(e) = self.blob.push(stored) {
            if self.spill_error.is_none() {
                self.spill_error = Some(e);
            }
        }
        block
    }

    /// Writes the pack to `path`, streaming the blob rather than copying it.
    ///
    /// Two passes by construction: the blob region is built first, because a
    /// [`fb::Block`] cannot be written until its offset and its compressed
    /// length are known.
    ///
    /// The table is small — identities, offsets, and one `u32` per tile per
    /// frame — so it is built in memory as before. The blob is not: it is
    /// handed straight from wherever it was accumulated to the output file, a
    /// megabyte at a time. Nothing ever holds two copies.
    pub fn finish_to(self, path: impl AsRef<std::path::Path>) -> std::io::Result<u64> {
        let path = path.as_ref();
        let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        let written = self.write(&mut file)?;
        use std::io::Write as _;
        file.flush()?;
        Ok(written)
    }
}

/// One frame being filled, tile by tile.
///
/// Dropping it without calling [`FrameWriter::end`] discards the frame rather
/// than recording a half one: a pack whose frame is missing tiles renders bare
/// ground and reports success, which is the single thing this format must never
/// do.
pub struct FrameWriter<'a> {
    writer: &'a mut PackWriter,
    frame: u32,
    view: BakedView,
    refs: Vec<u32>,
}

impl FrameWriter<'_> {
    /// Whether this identity is already stored. See [`PackWriter::holds`].
    pub fn holds(&self, id: u64, drape: u64) -> bool {
        self.writer.holds(id, drape)
    }

    /// Records a tile the pack already holds, carrying no bytes for it.
    ///
    /// Returns `false` — and records nothing — when the identity is not there,
    /// which can only mean the caller asked [`FrameWriter::holds`] about one
    /// tile and pushed another. Silently dropping it would leave a frame short
    /// of ground, so the caller is told.
    #[must_use]
    pub fn push_known(&mut self, id: u64, drape: u64) -> bool {
        match self.writer.index.get(&(id, drape)) {
            Some(&at) => {
                self.refs.push(at);
                true
            }
            None => false,
        }
    }

    /// Adds one tile, compressing and spilling it now.
    pub fn push(&mut self, tile: BakedTile) {
        let at = self.writer.intern(tile);
        self.refs.push(at);
    }

    /// Closes the frame and records it.
    pub fn end(self) {
        self.writer.frames.push((self.frame, self.view, self.refs));
    }
}

impl PackWriter {
    /// The one serialiser. `finish` and `finish_to` are the two ways to point
    /// it somewhere.
    fn write(mut self, out: &mut dyn std::io::Write) -> std::io::Result<u64> {
        // A blob that could not be written is a pack with holes in it, and it
        // would pass every check a reader makes — the table would describe
        // payloads that are not there. It fails here, once, with the error
        // that actually happened.
        if let Some(e) = self.spill_error.take() {
            return Err(e);
        }

        let mut fbb = flatbuffers::FlatBufferBuilder::with_capacity(1 << 20);
        let tiles: Vec<_> = self
            .tiles
            .iter()
            .map(|tile| {
                let origin = fbb.create_vector(&tile.origin_ecef);
                let factor = fbb.create_vector(&tile.base_color_factor);
                let mut b = fb::TileBuilder::new(&mut fbb);
                b.add_id(tile.id);
                b.add_drape(tile.drape);
                b.add_origin_ecef(origin);
                b.add_vertex_count(tile.vertex_count);
                b.add_index_count(tile.index_count);
                b.add_base_color_factor(factor);
                b.add_positions(&tile.positions);
                b.add_normals(&tile.normals);
                b.add_uvs(&tile.uvs);
                b.add_indices(&tile.indices);
                if let Some(tex) = tile.texture.as_ref() {
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
            .map(|(n, view, refs)| {
                let refs = fbb.create_vector(refs);
                let view = view.to_fb();
                let mut b = fb::FrameBuilder::new(&mut fbb);
                b.add_frame(*n);
                b.add_tiles(refs);
                b.add_view(&view);
                b.finish()
            })
            .collect();
        let frames = fbb.create_vector(&frames);

        let blob_digest = self.blob.digest();
        let digest = fbb.create_string(&self.scene_digest);
        let culling = fbb.create_string(&self.culling);
        let origin = fbb.create_vector(&self.render_origin);
        let first = self.frames.first().map_or(0, |(n, _, _)| *n);
        let last = self.frames.last().map_or(0, |(n, _, _)| *n);
        let mut b = fb::PackBuilder::new(&mut fbb);
        b.add_version(VERSION);
        b.add_scene_digest(digest);
        b.add_blob_digest(blob_digest);
        b.add_first_frame(first);
        b.add_last_frame(last);
        b.add_render_origin(origin);
        b.add_culling(culling);
        b.add_tiles(tiles);
        b.add_frames(frames);
        let root = b.finish();
        fbb.finish(root, Some("TUIL"));
        let table = fbb.finished_data();

        out.write_all(MAGIC)?;
        out.write_all(&(table.len() as u64).to_le_bytes())?;
        out.write_all(table)?;
        let blob_len = self.blob.drain_into(out)?;
        Ok(MAGIC.len() as u64 + 8 + table.len() as u64 + blob_len)
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
        let blobs = &bytes[end..];
        // Once, at open. A payload that arrives bent does not reliably fail to
        // decompress — it can hand back plausible bytes, and a renderer then
        // draws slightly wrong ground and reports success. Byte-for-byte
        // comparison downstream cannot see it either, because both sides of
        // such a comparison read the same damaged file.
        //
        // **Once really does mean once.** FNV-1a is a byte-at-a-time fold with
        // a carried dependency: it does not vectorise, and it costs seconds per
        // gigabyte. Charging it again on every frame is a toll that grows with
        // the pack — see [`Pack::reopen`], which exists because that toll was
        // being paid 24 times a second.
        let expected = root.blob_digest();
        let found = fnv1a(blobs);
        if found != expected {
            return Err(PackError::Corrupt { found, expected });
        }
        Ok(Self { root, blobs })
    }

    /// Reopens bytes this process has already verified.
    ///
    /// Same parsing and the same version check — the table is read in place,
    /// so this is a handful of bounds checks — but **no digest**. It is for
    /// the caller who verified the very same buffer with [`Pack::open`] and
    /// has held it, unmodified, ever since.
    ///
    /// # Why it exists
    ///
    /// A packed render opened the pack once per frame, and every one of those
    /// opens re-folded FNV-1a over the whole blob. On a 2.4 GB pack that is
    /// seconds of pure CPU per frame, spent proving again what was proved at
    /// startup about bytes nobody could have touched. Integrity is a property
    /// of the bytes when they arrive, not of how often they are looked at.
    ///
    /// Never point this at a file that could have changed, at a mapping shared
    /// with a writer, or at bytes that have crossed a wire since they were
    /// checked: use [`Pack::open`] for those, which is why it is the one with
    /// the plain name.
    pub fn reopen(bytes: &'a [u8]) -> Result<Self, PackError> {
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

    /// The frame baked for this camera, and what it selected.
    ///
    /// **This is how a render addresses a pack**, because a render has no
    /// frame number to ask with: a Hydra host cooks at a timecode and hands
    /// the session a camera. So the camera is the key.
    ///
    /// Nearest match within a tolerance rather than an exact one, and neither
    /// half of that is arbitrary. Not exact, because the camera does not
    /// travel from the bake to the render as twelve doubles — it goes through
    /// a `.usda` as a transform matrix and comes back decomposed, and the last
    /// bits do not survive. Not unbounded, because a camera that matches
    /// nothing is a stage and a pack from different shots, and the one thing
    /// that must not happen then is rendering the closest thing to hand and
    /// reporting success.
    pub fn frame_for_view(&self, view: &BakedView) -> Result<(u32, Vec<fb::Tile<'a>>), PackError> {
        let frames = self.root.frames().ok_or(PackError::Empty)?;
        let mut best: Option<(u32, f64, f64)> = None;
        for entry in frames.iter() {
            let Some(baked) = entry.view() else { continue };
            let baked = BakedView::from_fb(baked);
            let metres = view.metres_from(&baked);
            let radians = view.radians_from(&baked);
            if best.is_none_or(|(_, m, _)| metres < m) {
                best = Some((entry.frame(), metres, radians));
            }
        }
        let (frame, metres, radians) = best.ok_or(PackError::Empty)?;
        if metres > VIEW_TOLERANCE_M || radians > VIEW_TOLERANCE_RAD {
            return Err(PackError::NoSuchView {
                frame,
                metres,
                radians,
            });
        }
        Ok((frame, self.frame(frame)?))
    }

    /// The camera one frame was baked for.
    pub fn view_of(&self, frame: u32) -> Result<BakedView, PackError> {
        let frames = self.root.frames().ok_or(PackError::Empty)?;
        frames
            .iter()
            .find(|f| f.frame() == frame)
            .and_then(|f| f.view().map(BakedView::from_fb))
            .ok_or(PackError::NoSuchFrame(frame))
    }

    /// Decompresses one payload. The only copy a pack ever makes.
    pub fn payload(&self, block: &fb::Block, what: &'static str) -> Result<Vec<u8>, PackError> {
        let stored = self.stored(block, what)?;
        lz4_flex::block::decompress(stored, block.raw() as usize)
            .map_err(|source| PackError::Decompress { what, source })
    }

    /// The bytes of a block, still as they lie in the blob region.
    fn stored(&self, block: &fb::Block, what: &'static str) -> Result<&[u8], PackError> {
        let at = block.offset() as usize;
        let end = at
            .checked_add(block.stored() as usize)
            .filter(|e| *e <= self.blobs.len())
            .ok_or(PackError::Truncated { what })?;
        Ok(&self.blobs[at..end])
    }

    /// A tile's texture, decoded according to what its format already carries.
    ///
    /// The one place the rule is applied on the read side, mirroring
    /// `carries_its_own_compression` on the write side. Callers must not
    /// reach for [`Self::payload`] on a texture block: it would decompress
    /// something that was never compressed, and the PNG that came out of the
    /// bake would come back as a decoder error nobody would connect to a pack
    /// that is perfectly intact.
    pub fn texture(&self, tile: &fb::Tile<'a>) -> Result<Vec<u8>, PackError> {
        let Some(block) = tile.texture() else {
            return Ok(Vec::new());
        };
        if carries_its_own_compression(tile.texture_format()) {
            return Ok(self.stored(block, "texture")?.to_vec());
        }
        self.payload(block, "texture")
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
                // Par `texture`, pas par `payload` : c'est elle qui sait si
                // ce format porte déjà sa compression.
                Some(_) => Some(self.texture(tile)?),
                None => None,
            },
            texture_format: tile.texture_format(),
        })
    }
}

#[cfg(test)]
mod tests {

    /// Rouvrir ne doit pas reprendre le prix de l'intégrité.
    ///
    /// Un rendu depuis un pack rouvre le conteneur à chaque image. Tant que
    /// `open` était le seul chemin, chacune de ces ouvertures repliait FNV-1a
    /// sur tout le blob : sur 2,4 Go, des secondes de CPU par image, pour
    /// reprouver ce qui avait été prouvé au démarrage sur des octets que
    /// personne n'avait pu toucher.
    ///
    /// Le test compare les deux sur le même tampon. Le rapport exact dépend de
    /// la machine ; ce qui est vrai partout, c'est que `reopen` ne parcourt pas
    /// le blob et que `open` le parcourt entièrement.
    #[test]
    fn reopening_does_not_re_fold_the_whole_blob() {
        let mut w = Bake::new("s", [0.0; 3]);
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut heavy = a_tile(7, 100);
        heavy.positions = (0..8_000_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xff) as u8
            })
            .collect();
        w.frame(1, a_view(0.0), [heavy]);
        let bytes = w.finish();
        assert!(
            bytes.len() > 4 << 20,
            "le pack de test est trop petit pour mesurer"
        );

        let checked = std::time::Instant::now();
        Pack::open(&bytes).expect("opens");
        let checked = checked.elapsed();

        let trusted = std::time::Instant::now();
        Pack::reopen(&bytes).expect("reopens");
        let trusted = trusted.elapsed();

        assert!(
            trusted * 4 < checked,
            "reopen a coûté {trusted:?} contre {checked:?} pour open : \
             la vérification n'a pas été retirée du chemin par image"
        );
    }

    /// Ce que `reopen` refuse quand même.
    ///
    /// Sauter le digest n'est pas sauter la lecture. Un fichier qui n'est pas
    /// un pack, une table tronquée ou une version étrangère restent des
    /// erreurs — ce sont des pannes de forme, et elles ne coûtent rien à voir.
    #[test]
    fn reopening_still_refuses_what_is_not_a_pack() {
        let bytes = Bake::new("s", [0.0; 3]).finish();
        assert!(matches!(
            Pack::reopen(b"pas un pack du tout"),
            Err(PackError::NotAPack)
        ));
        assert!(matches!(
            Pack::reopen(&bytes[..MAGIC.len() + 4]),
            Err(PackError::NotAPack)
        ));
        let mut short = bytes.clone();
        short.truncate(MAGIC.len() + 16);
        assert!(matches!(
            Pack::reopen(&short),
            Err(PackError::Truncated { .. }) | Err(PackError::Malformed(_))
        ));
    }

    /// Le blob est sur disque AVANT la fin, pas retenu jusque-là.
    ///
    /// C'est toute la propriété : le pic cesse de dépendre de la longueur du
    /// film. Une cuisson d'une minute tenait trois fois le pack fini —
    /// tuiles décompressées, blob compressé, puis une copie concaténant table
    /// et blob — et ne rentrait pas dans seize gigaoctets.
    ///
    /// Le tampon d'écriture fait un mégaoctet, donc la charge utile ici est
    /// plus grosse que lui : une tuile minuscule resterait légitimement dans
    /// le tampon et le test ne prouverait rien.
    #[test]
    fn the_payloads_reach_the_disk_before_the_pack_is_finished() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spill = dir.path().join("blob.part");
        let mut w = PackWriter::new("s", [0.0; 3], &spill).expect("spill");

        let mut fat = a_tile(7, 100);
        // Incompressible, et vraiment.
        //
        // Première tentative : `(i * K).to_le_bytes()[0]`, qui a l'air
        // désordonné et ne l'est pas — l'octet de poids faible d'une
        // multiplication par un impair boucle toutes les 256 valeurs. LZ4 a
        // ramené trois mégaoctets sous le mégaoctet du tampon d'écriture, le
        // fichier est resté à zéro, et le test a accusé le writer. Un
        // xorshift n'a pas cette période.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        fat.positions = (0..3_000_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xff) as u8
            })
            .collect();
        w.frame(1, a_view(0.0), [fat]);

        let on_disk = std::fs::metadata(&spill).expect("stat").len();
        assert!(
            on_disk > 1 << 20,
            "le blob n'a que {on_disk} octets sur disque : le writer le retient \
             encore, et le pic est reparti pour croître avec le film"
        );
    }

    /// Une tuile en attente ne coûte que ses métadonnées.
    ///
    /// Si quelqu'un remet un `Vec<u8>` dans `StoredTile`, la structure grossit
    /// et le pic redevient proportionnel au nombre de tuiles uniques —
    /// silencieusement, parce que tout continue de marcher sur un pack court.
    #[test]
    fn a_pending_tile_is_metadata_and_nothing_else() {
        let size = std::mem::size_of::<StoredTile>();
        assert!(
            size <= 200,
            "StoredTile fait {size} octets : quelque chose y porte à nouveau \
             des données, et non cinq Blocks et des scalaires"
        );
    }

    /// Le digest incrémental est le digest tout court.
    ///
    /// `blob_digest` couvre le blob entier, et il n'y a plus de blob entier à
    /// hacher : il est replié morceau par morceau. Un repli qui diverge du
    /// calcul en une fois ferait échouer l'ouverture de chaque pack, avec un
    /// message parlant de corruption.
    #[test]
    fn folding_the_digest_in_pieces_matches_hashing_it_whole() {
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let whole = fnv1a(&bytes);
        let mut folded = FNV_OFFSET;
        for chunk in bytes.chunks(7) {
            folded = fnv1a_fold(folded, chunk);
        }
        assert_eq!(folded, whole);
    }

    /// Un writer jetable, et les octets qu'il finit par produire.
    ///
    /// Chaque test écrit deux vrais fichiers — le déversoir et le pack — parce
    /// qu'il n'y a plus qu'un chemin. Un raccourci en mémoire pour les tests
    /// remettrait exactement l'asymétrie qu'on vient de retirer : le court
    /// éprouvé, le long exécuté pour de bon. C'est ainsi qu'un pic de mémoire
    /// trois fois trop gros a tenu jusqu'à ce qu'une cuisson d'une minute le
    /// rencontre.
    struct Bake {
        dir: tempfile::TempDir,
        writer: Option<PackWriter>,
    }

    impl Bake {
        fn new(scene: &str, origin: [f64; 3]) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let writer = PackWriter::new(scene, origin, dir.path().join("blob.part"))
                .expect("opening the spill");
            Self {
                dir,
                writer: Some(writer),
            }
        }

        fn with(mut self, f: impl FnOnce(PackWriter) -> PackWriter) -> Self {
            self.writer = Some(f(self.writer.take().expect("writer")));
            self
        }

        fn frame(
            &mut self,
            frame: u32,
            view: BakedView,
            tiles: impl IntoIterator<Item = BakedTile>,
        ) {
            self.writer
                .as_mut()
                .expect("writer")
                .frame(frame, view, tiles);
        }

        fn writer(&mut self) -> &mut PackWriter {
            self.writer.as_mut().expect("writer")
        }

        fn finish(mut self) -> Vec<u8> {
            let out = self.dir.path().join("pack.tuilepack");
            self.writer
                .take()
                .expect("writer")
                .finish_to(&out)
                .expect("finish_to");
            std::fs::read(&out).expect("reading the pack back")
        }
    }

    use super::*;

    /// A tile the pack already holds costs nothing to reference.
    ///
    /// The point is not the bytes on disk — `frame` already deduplicated those
    /// — it is that the caller never has to BUILD the duplicate. An orbit
    /// re-selects almost the same ground every frame, and every one of those
    /// tiles was being encoded to PNG, converted to four little-endian buffers
    /// and then dropped by the writer as a duplicate.
    ///
    /// So the pack produced by referencing must be byte-identical to the pack
    /// produced by building the tile again and letting the writer throw it
    /// away. Anything less and this is a shortcut, not a saving.
    #[test]
    fn referencing_a_held_tile_builds_the_same_pack_as_rebuilding_it() {
        let mut rebuilt = Bake::new("s", [0.0; 3]);
        rebuilt.frame(1, a_view(0.0), [a_tile(7, 100)]);
        rebuilt.frame(2, a_view(1.0), [a_tile(7, 100), a_tile(9, 100)]);
        let rebuilt = rebuilt.finish();

        let mut referenced = Bake::new("s", [0.0; 3]);
        referenced.frame(1, a_view(0.0), [a_tile(7, 100)]);
        {
            let mut open = referenced.writer().begin_frame(2, a_view(1.0));
            assert!(open.holds(7, 100), "frame 1 stored it");
            assert!(open.push_known(7, 100), "and it can be referenced");
            assert!(!open.holds(9, 100), "this one is new");
            open.push(a_tile(9, 100));
            open.end();
        }
        let referenced = referenced.finish();

        assert_eq!(rebuilt, referenced, "the two packs differ");
    }

    /// And an identity the pack does not hold is refused rather than dropped.
    ///
    /// Recording nothing would leave that frame one tile short of its ground,
    /// and a pack with a hole renders bare terrain and reports success — the
    /// one failure this format must not have.
    #[test]
    fn an_unheld_identity_cannot_be_referenced() {
        let mut bake = Bake::new("s", [0.0; 3]);
        let mut open = bake.writer().begin_frame(1, a_view(0.0));
        assert!(!open.push_known(7, 100), "nothing was ever stored");
        // The same tile under a different drape is a different picture, and
        // holding one must not answer for the other.
        open.push(a_tile(7, 100));
        assert!(!open.push_known(7, 101), "a redrape is not the same tile");
        assert!(open.push_known(7, 100), "the one that was stored");
        open.end();
    }

    fn a_view(km_east: f64) -> BakedView {
        BakedView {
            position: [4_700_959.9 + km_east * 1000.0, 186_133.4, 4_314_027.3],
            direction: [0.0, 0.0, -1.0],
            up: [0.0, 1.0, 0.0],
            viewport_px: [1280.0, 960.0],
            fovy_rad: std::f64::consts::FRAC_PI_4,
        }
    }

    fn a_tile(id: u64, drape: u64) -> BakedTile {
        BakedTile {
            id,
            drape,
            origin_ecef: [
                4_700_959.945_556_212,
                186_133.405_820_756_92,
                4_314_027.341_856_181,
            ],
            // Deliberately not round numbers and not compressible: a payload
            // that happens to compress to nothing proves nothing about the
            // block plumbing.
            positions: (0..1024u32)
                .flat_map(|i| (i.wrapping_mul(2_654_435_761)).to_le_bytes())
                .collect(),
            normals: (0..512u32)
                .flat_map(|i| (i ^ 0xdead_beef).to_le_bytes())
                .collect(),
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
        let mut w = Bake::new("scene-abc", [1.0, 2.0, 3.0]);
        let a = a_tile(7, 100);
        let b = a_tile(9, 200);
        w.frame(1, a_view(0.0), [a.clone(), b.clone()]);
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
        let mut w = Bake::new("s", [0.0; 3]);
        for frame in 1..=40u32 {
            w.frame(frame, a_view(f64::from(frame)), [a_tile(7, 100)]);
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
        let mut w = Bake::new("s", [0.0; 3]);
        w.frame(1, a_view(0.0), [a_tile(7, 100)]);
        w.frame(2, a_view(1.0), [a_tile(7, 200)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert_eq!(pack.tile_count(), 2);
        assert_eq!(pack.frame(1).expect("f1")[0].drape(), 100);
        assert_eq!(pack.frame(2).expect("f2")[0].drape(), 200);
    }

    #[test]
    fn the_wrong_scene_is_refused_rather_than_rendered() {
        let bytes = Bake::new("scene-abc", [0.0; 3]).finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert!(pack.expect_scene("scene-abc").is_ok());
        assert!(matches!(
            pack.expect_scene("scene-xyz"),
            Err(PackError::WrongScene { .. })
        ));
    }

    /// A pack baked under a known-imperfect cull must never pass for a good
    /// one: the admission travels with the data.
    /// Une image n'est pas recompressée, la géométrie l'est.
    ///
    /// Mesuré sur une cuisson de 24 frames en septembre 2026 : le LZ4 rend les
    /// PNG **plus gros** de 0,2 %, et ils font 94 % des octets d'un pack. On
    /// payait donc une compression à la cuisson et une décompression à chaque
    /// lecture pour perdre de la place. La géométrie, elle, gagne 26 à 48 %.
    ///
    /// Le test porte sur les deux moitiés de la règle : retirer la première
    /// referait grossir le pack, retirer la seconde le ferait doubler.
    #[test]
    fn an_already_compressed_payload_is_not_compressed_again() {
        let mut w = Bake::new("s", [0.0; 3]);
        w.frame(1, a_view(0.0), [a_tile(1, 1)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");
        let tiles = pack.frame(1).expect("the frame");
        let tile = tiles.first().expect("one tile");

        let texture = tile.texture().expect("a textured tile");
        assert_eq!(
            texture.stored(),
            texture.raw(),
            "un PNG est déjà compressé : stocké tel quel, les deux longueurs \
             sont la même"
        );

        // La géométrie, elle, passe bien par LZ4 — vérifié en la décodant,
        // pas en mesurant un ratio : les octets de cette fixture sont
        // synthétiques et ne se compressent pas, si bien qu'un test sur la
        // taille ne dirait rien du code. Le gain réel (26 à 48 %) se mesure
        // sur un vrai pack, et c'est la mesure citée en tête de
        // `carries_its_own_compression`.
        let source = a_tile(1, 1);
        for (block, what, want) in [
            (tile.positions(), "positions", &source.positions),
            (tile.normals(), "normals", &source.normals),
            (tile.uvs(), "uvs", &source.uvs),
            (tile.indices(), "indices", &source.indices),
        ] {
            let block = block.expect(what);
            assert_eq!(
                &pack.payload(block, "geometry").expect("décode"),
                want,
                "{what} doit se décoder par LZ4 et rendre ses octets"
            );
        }

        // Et le tour complet reste exact : une règle d'écriture ne vaut que si
        // la lecture applique la même.
        let baked = pack.baked(tile).expect("decodes");
        assert_eq!(baked.texture.as_deref(), source.texture.as_deref());
        assert_eq!(baked.positions, source.positions);
    }

    #[test]
    fn the_pack_carries_how_it_was_culled() {
        let bytes = Bake::new("s", [0.0; 3])
            .with(|w| w.culling("disabled"))
            .finish();
        assert_eq!(Pack::open(&bytes).expect("opens").culling(), "disabled");
        let bytes = Bake::new("s", [0.0; 3]).finish();
        assert_eq!(Pack::open(&bytes).expect("opens").culling(), "full");
    }

    /// A payload bent in transit is caught at open, not drawn.
    ///
    /// Written because the first round-trip check could not see it: it
    /// compared what the render session decoded against what the pack reader
    /// decoded, and both read the same damaged file. Two readings of one
    /// corrupt byte agree perfectly.
    #[test]
    fn a_payload_damaged_in_transit_is_caught_at_open() {
        let mut w = Bake::new("s", [0.0; 3]);
        w.frame(1, a_view(0.0), [a_tile(1, 1)]);
        let mut bytes = w.finish();
        assert!(Pack::open(&bytes).is_ok());
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(
            matches!(Pack::open(&bytes), Err(PackError::Corrupt { .. })),
            "one flipped byte in a payload must not open"
        );
    }

    #[test]
    fn something_that_is_not_a_pack_says_so() {
        assert!(matches!(
            Pack::open(b"not a pack at all"),
            Err(PackError::NotAPack)
        ));
        let mut bytes = Bake::new("s", [0.0; 3]).finish();
        bytes.truncate(MAGIC.len() + 4);
        assert!(matches!(Pack::open(&bytes), Err(PackError::NotAPack)));
    }

    /// A render addresses a pack by camera, and the match must survive the
    /// trip a camera actually makes — out through a `.usda` as a matrix, back
    /// through Hydra decomposed.
    #[test]
    fn a_camera_that_drifted_in_its_last_bits_still_finds_its_frame() {
        let mut w = Bake::new("s", [0.0; 3]);
        w.frame(1, a_view(0.0), [a_tile(1, 1)]);
        w.frame(2, a_view(1.0), [a_tile(2, 1)]);
        w.frame(3, a_view(2.0), [a_tile(3, 1)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");

        let mut drifted = a_view(1.0);
        drifted.position[0] += 0.2;
        drifted.position[2] -= 0.3;
        let (frame, tiles) = pack.frame_for_view(&drifted).expect("finds frame 2");
        assert_eq!(frame, 2);
        assert_eq!(tiles[0].id(), 2);
    }

    /// …and a camera from another shot is refused rather than served the
    /// nearest thing to hand. This is the failure a split pipeline adds:
    /// rendering the wrong ground, successfully.
    #[test]
    fn a_camera_from_another_shot_is_refused_not_approximated() {
        let mut w = Bake::new("s", [0.0; 3]);
        w.frame(1, a_view(0.0), [a_tile(1, 1)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");

        // A kilometre away: closer than any two frames of a slow shot, and
        // still nothing this pack was baked for.
        let err = pack.frame_for_view(&a_view(1.0)).expect_err("refused");
        assert!(
            matches!(err, PackError::NoSuchView { frame: 1, .. }),
            "{err}"
        );

        // Right place, wrong way round.
        let mut turned = a_view(0.0);
        turned.direction = [0.0, 0.0, 1.0];
        let err = pack.frame_for_view(&turned).expect_err("refused");
        assert!(matches!(err, PackError::NoSuchView { .. }), "{err}");
    }

    #[test]
    fn a_frame_the_pack_does_not_hold_is_an_error_not_an_empty_selection() {
        let mut w = Bake::new("s", [0.0; 3]);
        w.frame(1, a_view(0.0), [a_tile(1, 1)]);
        let bytes = w.finish();
        let pack = Pack::open(&bytes).expect("opens");
        assert!(matches!(pack.frame(2), Err(PackError::NoSuchFrame(2))));
    }
}

/// FNV-1a over a byte run. Stable across processes and architectures, which is
/// the only property that matters for something written on one machine and
/// checked on another.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_fold(FNV_OFFSET, bytes)
}

/// FNV-1a continued from a running state.
///
/// The blob no longer exists as one slice — it is written as it is produced —
/// so `blob_digest` is folded in chunk by chunk. FNV-1a is a pure fold over
/// bytes, so this is the same number the one-shot version gives for the same
/// stream; [`fnv1a`] is now that fold started from the offset basis, and the
/// tests hold the two against each other.
fn fnv1a_fold(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
