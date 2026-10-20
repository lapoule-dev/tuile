// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A frame range baked into a pack, from poses given one at a time.
//!
//! The binary bakes a tape: every pose known before the first frame. A caller
//! that decides each pose against the ground — lifting a camera out of a hill
//! it would otherwise sit in — cannot write that tape first without converging
//! every frame twice, once to decide and once to bake. [`bake_frames`] takes a
//! [`PoseSource`] instead: the source is handed the session, may converge on
//! views of its own, and returns the pose; the frame is baked on the ground it
//! just loaded. A tape is one such source ([`Poses`]), and the binary's bake is
//! exactly that.
//!
//! # The scene's name
//!
//! A pack is named by the poses it was baked from ([`digest_of_scene`]). When
//! they are known up front the caller names the scene itself
//! ([`SceneName::Given`]); when they are decided during the bake the name is
//! taken over the poses actually baked, once the last one is in
//! ([`SceneName::OfPoses`]).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tuile_pack::{BakedTile, PackWriter, TextureFormat};

use crate::{Frame, GlobeConfig, HeldDrape, Session, TileGeometry};

/// Where the poses of a bake come from, one frame at a time.
pub trait PoseSource {
    /// The pose to bake as frame `number`. `session` is the bake's own: a
    /// source may converge on views of its own before it decides, and the
    /// frame is then baked on the ground those left resident.
    fn pose(&mut self, number: u32, session: &mut Session) -> Result<tuile_tape::Frame, String>;
}

/// Poses known in advance: frame `n` is `self.0[n - 1]`.
pub struct Poses<'a>(pub &'a [tuile_tape::Frame]);

impl PoseSource for Poses<'_> {
    fn pose(&mut self, number: u32, _session: &mut Session) -> Result<tuile_tape::Frame, String> {
        number
            .checked_sub(1)
            .and_then(|i| self.0.get(i as usize))
            .copied()
            .ok_or_else(|| format!("frame {number} is past the {} poses given", self.0.len()))
    }
}

/// How a pack is named.
pub enum SceneName {
    /// The caller knows the scene: every pose was known before the bake.
    Given(String),
    /// Named over the poses actually baked, with these settings (see
    /// [`bake_settings`]).
    OfPoses { settings: String },
}

/// What the pack already holds, mirrored for the loader so it never fetches a
/// drape twice.
///
/// The pack is the authority — `FrameWriter::push_known` reads its real index
/// — and this mirrors it for the one caller that has to know the answer
/// *before* the request goes out: the loader asks from a worker thread, deep
/// inside a fetch, while the writer is borrowed mutably by the frame loop.
///
/// A divergence cannot pass silently: a tile the loader withheld arrives with
/// no imagery, so if `push_known` then fails to find it, [`baked_tile`] refuses
/// a drape with no texture rather than storing bare ground.
#[derive(Clone, Default)]
pub struct PackedMirror(Arc<Mutex<HashSet<(u64, u64)>>>);

impl PackedMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// The loader's side, for [`GlobeConfig::held_drape`].
    pub fn held_drape(&self) -> HeldDrape {
        let packed = Arc::clone(&self.0);
        HeldDrape::new(move |id, drape| packed.lock().is_ok_and(|held| held.contains(&(id, drape))))
    }

    /// Says the pack holds tile `id` with drape `drape`: what a bake says
    /// as soon as the frame that adds it is selected.
    pub fn hold(&self, id: u64, drape: u64) {
        if let Ok(mut held) = self.0.lock() {
            held.insert((id, drape));
        }
    }
}

/// What a pack carries for its tiles: the tiles, references to them in the
/// tile store, or both — and which layers of the store the references name.
#[derive(Debug, Clone, PartialEq)]
pub struct PackContent {
    pub content: tuile_pack::Content,
    pub terrain_layer: String,
    pub imagery_layer: String,
    /// The store's layer of the night lights, empty for none.
    pub night_layer: String,
}

impl PackContent {
    /// The tiles themselves and nothing of the store: what a pack always was.
    pub fn embedded() -> Self {
        Self {
            content: tuile_pack::Content::Embedded,
            terrain_layer: String::new(),
            imagery_layer: String::new(),
            night_layer: String::new(),
        }
    }
}

/// One bake of a frame range.
pub struct BakeFrames<'a> {
    pub first: u32,
    pub last: u32,
    pub viewport: (f64, f64),
    pub out: &'a Path,
    pub scene: SceneName,
    /// How the selection was culled, written into the pack.
    pub culling: &'a str,
    /// What the pack carries for its tiles, and the tile store's layers its
    /// references are into.
    pub content: &'a PackContent,
    pub packed: &'a PackedMirror,
}

/// What a bake produced.
#[derive(Debug)]
pub struct BakeOutcome {
    pub scene: String,
    /// The poses baked, frame `first` first.
    pub poses: Vec<tuile_tape::Frame>,
    /// What each frame converged on ([`Session::selection_digest`]), frame
    /// `first` first: two bakes that agree on these selected the same tiles
    /// every frame.
    pub selections: Vec<u64>,
    pub bytes: u64,
}

/// How many frames may be selected and not yet written: the most frames'
/// tiles a bake holds at once beyond the one being written.
const WRITE_LEAD: usize = 8;

/// One tile of a frame as the writer takes it: one the pack will already
/// hold by then, or one to add.
enum Entry {
    Known { id: u64, drape: u64 },
    New(Box<BakedTile>),
}

/// A frame, made ready for the writer.
struct Ready {
    number: u32,
    view: tuile_pack::BakedView,
    /// The first pose: the render origin a pack is opened with.
    origin: [f64; 3],
    entries: Result<Vec<Entry>, String>,
    selected: usize,
    reused: usize,
    selected_in: std::time::Duration,
    pose_in: std::time::Duration,
}

/// Bakes `job.first..=job.last` into `job.out`, asking `source` for each pose.
///
/// # Three stages
///
/// Each frame's pose is decided and its selection converged here, frame
/// after frame — the one thing that has to happen in order, since a frame
/// selects against what the frames before it chose. Its tiles are then made
/// ready for the pack (a drape composed and encoded, for a pack that
/// carries its tiles) on the rayon pool, while the next frame converges;
/// and a writer takes the frames in order, whichever was made ready first.
///
/// What the pack holds is decided where the order is: a tile is new to the
/// pack if no frame before it, nor any tile before it in its frame, has
/// the same tile and drape — exactly what the writer will find
/// (`FrameWriter::push_known`) — so only new tiles are made ready, and the
/// loader is told the pack holds them ([`PackedMirror`]) as soon as their
/// frame is selected, as it was told when frames were written one by one.
/// The pack is the one writing each frame in turn writes, byte for byte.
pub fn bake_frames(
    session: &mut Session,
    job: &BakeFrames<'_>,
    source: &mut dyn PoseSource,
) -> Result<BakeOutcome, String> {
    if job.first == 0 || job.first > job.last {
        return Err(format!(
            "frames {}:{} is not a range of frames",
            job.first, job.last
        ));
    }
    let began = Instant::now();
    // Le blob part sur disque au fil de la cuisson, à côté du pack.
    //
    // Sans ça, une cuisson tient les tuiles décompressées, puis le blob
    // compressé, puis une troisième copie concaténant table et blob — trois
    // fois le pack fini, vivants au même instant, sur une machine qui porte
    // aussi le cache de tuiles. Le déversoir rend le pic indépendant de la
    // longueur : 1440 frames coûtent ce que coûtent 48.
    let spill = job.out.with_extension("blob.part");
    let frames = (job.last - job.first + 1) as usize;
    let mut poses = Vec::with_capacity(frames);
    let mut selections = Vec::with_capacity(frames);
    let metrics = tuile_core::metrics::metrics();
    let content = job.content.content;

    let (writer, failed) = std::thread::scope(|scope| {
        let (ready, readied) = std::sync::mpsc::channel::<Ready>();
        // Frames selected and not yet written: one slot each.
        let (slot, freed) = std::sync::mpsc::sync_channel::<()>(WRITE_LEAD);
        let writer = scope.spawn(move || {
            write_in_order(job, &spill, readied, || {
                // Its slot, for a frame still to be selected.
                let _ = freed.recv();
            })
        });

        let mut failed: Option<String> = None;
        // What the pack will hold once every frame selected so far is
        // written: the writer's index, ahead of it.
        let mut will_hold: HashSet<(u64, u64)> = HashSet::new();
        for number in job.first..=job.last {
            let posing = Instant::now();
            let pose = source.pose(number, session);
            let pose_in = posing.elapsed();
            metrics.pose_seconds.record(pose_in);
            let pose = match pose {
                Ok(pose) => pose,
                Err(e) => {
                    failed = Some(format!("frame {number}: {e}"));
                    break;
                }
            };
            let at = Instant::now();
            let frame = match session.frame_for_pose(&pose, job.viewport) {
                Ok(frame) => frame,
                Err(e) => {
                    failed = Some(format!("frame {number}: {e}"));
                    break;
                }
            };
            let selected_in = at.elapsed();
            metrics.bake_frame_seconds.record(selected_in);
            selections.push(tuile_core::determinism::digest(
                frame.tiles.iter().map(|t| t.tile.0),
            ));
            let new: Vec<bool> = frame
                .tiles
                .iter()
                .map(|tile| {
                    let key = (tile.tile.0, tile.drape());
                    let new = will_hold.insert(key);
                    if new {
                        job.packed.hold(key.0, key.1);
                    }
                    new
                })
                .collect();
            let reused = new.iter().filter(|new| !**new).count();
            let origin = poses
                .first()
                .map_or(pose.position, |p: &tuile_tape::Frame| p.position);
            let view = tuile_pack::BakedView {
                position: pose.position,
                direction: pose.direction,
                up: pose.up,
                viewport_px: [job.viewport.0, job.viewport.1],
                fovy_rad: pose.fovy,
            };
            poses.push(pose);
            // A slot, or wait for the writer to free one.
            if slot.send(()).is_err() {
                break;
            }
            let ready = ready.clone();
            rayon::spawn(move || {
                let entries = frame
                    .tiles
                    .iter()
                    .zip(new)
                    .enumerate()
                    .map(|(index, (tile, new))| {
                        if new {
                            packed_tile(&frame, index, tile, content)
                                .map(|t| Entry::New(Box::new(t)))
                        } else {
                            Ok(Entry::Known {
                                id: tile.tile.0,
                                drape: tile.drape(),
                            })
                        }
                    })
                    .collect::<Result<Vec<_>, _>>();
                let _ = ready.send(Ready {
                    number,
                    view,
                    origin,
                    entries,
                    selected: frame.tiles.len(),
                    reused,
                    selected_in,
                    pose_in,
                });
            });
        }
        drop(ready);
        drop(slot);
        let writer = writer
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (writer, failed)
    });
    if let Some(failed) = failed {
        return Err(failed);
    }
    let mut writer = writer?.ok_or("no frame was baked")?;

    let scene = match &job.scene {
        SceneName::Given(scene) => scene.clone(),
        SceneName::OfPoses { settings } => digest_of_scene(&poses, job.viewport, settings),
    };
    writer.set_scene_digest(scene.clone());
    let bytes = writer
        .finish_to(job.out)
        .map_err(|e| format!("writing {}: {e}", job.out.display()))?;
    tracing::info!(
        scene,
        culling = job.culling,
        path = %job.out.display(),
        bytes,
        seconds = began.elapsed().as_secs_f64(),
        "BAKE-DONE"
    );
    Ok(BakeOutcome {
        scene,
        poses,
        selections,
        bytes,
    })
}

/// The writer's stage: frames as they are made ready, written in the order
/// of their numbers — each one as soon as every frame before it is.
fn write_in_order(
    job: &BakeFrames<'_>,
    spill: &Path,
    readied: impl IntoIterator<Item = Ready>,
    mut written: impl FnMut(),
) -> Result<Option<PackWriter>, String> {
    let mut writer: Option<PackWriter> = None;
    let mut waiting: BTreeMap<u32, Ready> = BTreeMap::new();
    let mut next = job.first;
    for ready in readied {
        waiting.insert(ready.number, ready);
        while let Some(ready) = waiting.remove(&next) {
            let entries = ready
                .entries
                .map_err(|e| format!("frame {}: {e}", ready.number))?;
            // The writer is opened on the first pose, which is the render
            // origin the pack's positions are relative to. The pack stores
            // each tile's own ECEF origin, so this is carried for the
            // consumer that rebases, not used to move anything here.
            let writer = match &mut writer {
                Some(w) => w,
                None => writer.insert(
                    PackWriter::new(
                        match &job.scene {
                            SceneName::Given(scene) => scene.clone(),
                            SceneName::OfPoses { .. } => String::new(),
                        },
                        ready.origin,
                        spill,
                    )
                    .map_err(|e| format!("opening {}: {e}", spill.display()))?
                    .culling(job.culling)
                    .with_content(
                        job.content.content,
                        &job.content.terrain_layer,
                        &job.content.imagery_layer,
                    )
                    .with_night_layer(&job.content.night_layer),
                ),
            };
            // The camera goes in beside the tiles, because that is what a
            // render will address this frame by: a renderer hands the
            // session a camera, never a frame number.
            let mut open = writer.begin_frame(ready.number, ready.view);
            for entry in entries {
                match entry {
                    Entry::Known { id, drape } => {
                        if !open.push_known(id, drape) {
                            return Err(format!(
                                "frame {}: tile {id} with drape {drape:016x} was to be in \
                                 the pack by now, and is not",
                                ready.number
                            ));
                        }
                    }
                    Entry::New(tile) => open.push(*tile),
                }
            }
            open.end();
            tracing::info!(
                frame = ready.number,
                selected = ready.selected,
                reused = ready.reused,
                seconds = ready.selected_in.as_secs_f64(),
                pose_seconds = ready.pose_in.as_secs_f64(),
                "BAKE-FRAME"
            );
            written();
            next += 1;
        }
    }
    // Every frame selected was made ready, or the bake stopped short of
    // one and says why elsewhere: one left waiting is a frame lost.
    if let Some(number) = waiting.keys().next() {
        return Err(format!(
            "frame {next} never came to be written, frame {number} waits for it"
        ));
    }
    Ok(writer)
}

/// Tout ce qui décide du contenu d'un pack hors trajectoire, en une chaîne.
///
/// La traversée résolue, **et les sources**. Ce paramètre s'appelait
/// `traversal` et ne portait que la première, ce qui laissait passer deux
/// collisions mesurées le 17 septembre 2026 sur la même orbite :
///
/// * boost d'imagerie 1 et 2 → même digest `9fb2b0f3559debc6`. Le plafond de
///   boost est une option du chargeur, pas de la traversée, donc
///   `exact_traversal` ne le voit pas.
/// * asset d'imagerie 2 (Bing) et 3954 (Sentinel) → même digest, pour la même
///   raison.
///
/// Composée en UN endroit pour la cuisson et `--verify`, parce que deux
/// compositions séparées finissent par diverger.
pub fn bake_settings(config: &GlobeConfig, resolved: &tuile_core::traversal::Config) -> String {
    format!(
        "{resolved:?}\nterrain={}\nimagery={:?}\nimagery_boost={}",
        config.terrain_asset_id,
        config.imagery_asset_id,
        crate::imagery_boost_cap(),
    )
}

/// The name of the scene a pack is the bake of.
///
/// Everything that changes what a frame contains goes in, and nothing else. In
/// particular the frame range does **not**: two shards of one shot bake
/// different ranges of the same scene, and they must agree on its name.
///
/// # The camera path, not the file that carried it
///
/// Taken over the **poses**, canonically: ten little-endian `f64` per frame, in
/// order. Hashing the tape's bytes renamed every scene whenever the container
/// changed — writer version, repeated schemas, `HashMap` order — measured on 16
/// September 2026, when two bakes of one trajectory landed under
/// `fb45fe68fb26e559` and `05274f175b83df6a`, and both were right.
pub fn digest_of_scene(
    poses: &[tuile_tape::Frame],
    viewport: (f64, f64),
    settings: &str,
) -> String {
    let mut path = Vec::with_capacity(poses.len() * 10 * 8);
    for pose in poses {
        for value in pose
            .position
            .iter()
            .chain(&pose.direction)
            .chain(&pose.up)
            .chain(std::slice::from_ref(&pose.fovy))
        {
            path.extend_from_slice(&value.to_le_bytes());
        }
    }
    tuile_pack::scene_digest(&[
        &path,
        &viewport.0.to_le_bytes(),
        &viewport.1.to_le_bytes(),
        settings.as_bytes(),
    ])
}

/// Vertex data as the pack stores it: little-endian, whatever the host is.
///
/// Spelled out rather than cast, because a cast would write the host's order
/// and makes the format a thing the file says rather than a thing the writer
/// remembers.
fn f32x3_le(values: &[[f32; 3]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 12);
    for v in values {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    out
}

fn f32x2_le(values: &[[f32; 2]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 8);
    for v in values {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    out
}

fn u32_le(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Whether a tile can be stored, given the drape it names and what it carries.
///
/// - drape 0, no texture — terrain with no imagery. Legitimate: the geometry
///   debug view, and any tile the provider covers with nothing.
/// - drape 0, a texture — a texture the tile owns rather than one we composed.
/// - a drape, a texture — the ordinary draped tile.
/// - **a drape, no texture** — the pixels are nowhere. Refused.
fn drape_has_its_pixels(drape: u64, has_texture: bool) -> bool {
    drape == 0 || has_texture
}

/// One tile, in the shape the pack stores and the ABI hands out.
///
/// The mapping is deliberately the same one `tuile_frame_tile` makes, field for
/// field: a pack is a substitute for a live session, and a substitute that
/// reshapes the data is a second implementation.
/// One tile as a pack of `content` takes it.
///
/// A tile that still has its mesh and its picture goes as [`baked_tile`]
/// makes it, whatever the pack keeps of it. A tile finished by identity
/// ([`crate::Finish::Identity`]) has neither: it can only go into a pack that
/// refers to the tile store, and only with its references — then the record
/// is the one the same tile would have left with its pictures, since such a
/// pack stores a tile's counts and never its bytes.
pub fn packed_tile(
    frame: &Frame,
    index: usize,
    tile: &Arc<TileGeometry>,
    content: tuile_pack::Content,
) -> Result<BakedTile, String> {
    let Some(shape) = tile.shape else {
        return baked_tile(frame, index, tile);
    };
    if content != tuile_pack::Content::References {
        return Err(format!(
            "tile {} was finished by identity and has no mesh or picture to embed: \
             only a pack by reference can take it",
            tile.tile.0
        ));
    }
    let Some(refs) = tile.refs.clone() else {
        return Err(format!(
            "tile {} was finished by identity and its source noted nothing of where it \
             comes from: there is nothing to refer to",
            tile.tile.0
        ));
    };
    Ok(BakedTile {
        id: tile.tile.0,
        drape: tile.drape(),
        origin_ecef: tile.origin_ecef.to_array(),
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
        vertex_count: shape.vertex_count,
        index_count: shape.index_count,
        base_color_factor: shape.base_color_factor,
        // What the picture would have been encoded as, had it been made.
        texture_format: if tile.drape() != 0 {
            TextureFormat::Png
        } else {
            TextureFormat::None
        },
        refs: Some(refs),
        texture: None,
    })
}

pub fn baked_tile(
    frame: &Frame,
    index: usize,
    tile: &Arc<TileGeometry>,
) -> Result<BakedTile, String> {
    // One prim per tile, as the consumer expects; terrain produces one mesh.
    let mesh = tile
        .content
        .meshes
        .first()
        .ok_or_else(|| format!("tile {} carries no mesh", tile.tile.0))?;

    // Index 0 only: a terrain tile carries one draped mosaic. A tile with
    // several would need the pack to hold several, and nothing produces one —
    // so this fails loudly rather than silently baking the first of many.
    let texture = frame
        .texture_png(index, 0)
        .map_err(|e| format!("tile {}: {e}", tile.tile.0))?;
    if frame
        .texture_png(index, 1)
        .map_err(|e| format!("tile {}: {e}", tile.tile.0))?
        .is_some()
    {
        return Err(format!(
            "tile {} carries more than one texture; the pack holds one",
            tile.tile.0
        ));
    }

    // A tile that names a drape and carries no texture is bare ground: the
    // loader withheld the imagery because the pack was said to hold this drape,
    // and then the pack did not hold it. Storing it would put real terrain in
    // the film under no picture at all, with every counter reading green.
    if !drape_has_its_pixels(tile.drape(), texture.is_some()) {
        return Err(format!(
            "tile {} names drape {:016x} and carries no texture: its imagery was \
             withheld for a pack that does not hold it",
            tile.tile.0,
            tile.drape()
        ));
    }

    Ok(BakedTile {
        id: tile.tile.0,
        drape: tile.drape(),
        origin_ecef: tile.origin_ecef.to_array(),
        positions: f32x3_le(&mesh.positions),
        normals: mesh
            .normals
            .as_ref()
            .map(|n| f32x3_le(n))
            .unwrap_or_default(),
        uvs: mesh.uvs.as_ref().map(|u| f32x2_le(u)).unwrap_or_default(),
        indices: u32_le(&mesh.indices),
        vertex_count: mesh.positions.len() as u32,
        index_count: mesh.indices.len() as u32,
        base_color_factor: mesh.material.base_color_factor,
        texture_format: if texture.is_some() {
            TextureFormat::Png
        } else {
            TextureFormat::None
        },
        refs: tile.refs.clone(),
        texture: texture.map(|t| t.png.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drape_with_no_pixels_is_refused_and_nothing_else_is() {
        assert!(drape_has_its_pixels(0, false), "terrain with no imagery");
        assert!(drape_has_its_pixels(0, true), "a texture the tile owns");
        assert!(drape_has_its_pixels(0xdead, true), "a draped tile");
        assert!(
            !drape_has_its_pixels(0xdead, false),
            "a drape whose pixels are nowhere"
        );
    }

    #[test]
    fn vertices_are_written_little_endian_whatever_the_host_is() {
        assert_eq!(f32x3_le(&[[1.0, -2.0, 0.5]]), {
            let mut v = Vec::new();
            for c in [1.0f32, -2.0, 0.5] {
                v.extend_from_slice(&c.to_le_bytes());
            }
            v
        });
        assert_eq!(u32_le(&[1, 0x0102_0304]), vec![1, 0, 0, 0, 4, 3, 2, 1]);
        assert_eq!(f32x2_le(&[[0.0, 1.0]]).len(), 8);
    }

    /// A scene named after the fact is the name the pack carries, and a packed
    /// session has no heights to offer — only a live globe reads ground.
    #[test]
    fn a_scene_named_late_is_the_pack_s_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer =
            PackWriter::new(String::new(), [0.0; 3], dir.path().join("blob.part")).expect("writer");
        writer.frame(
            1,
            tuile_pack::BakedView {
                position: [1.0, 2.0, 3.0],
                direction: [0.0, 0.0, -1.0],
                up: [0.0, 1.0, 0.0],
                viewport_px: [540.0, 960.0],
                fovy_rad: std::f64::consts::FRAC_PI_4,
            },
            [],
        );
        writer.set_scene_digest("named-after-the-poses");
        let path = dir.path().join("p.tuilepack");
        writer.finish_to(&path).expect("pack");

        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(
            tuile_pack::Pack::open(&bytes).expect("open").scene_digest(),
            "named-after-the-poses"
        );
        let session = Session::from_pack(&path, Some("named-after-the-poses"))
            .expect("the late name is the one a render checks");
        assert!(session.heights().is_none());
    }

    /// A pack that refers to the tile store is the same file, byte for byte,
    /// whether its tiles were finished with their pictures or by identity
    /// alone: nothing of a mesh or of a mosaic goes into it.
    #[test]
    fn a_pack_by_reference_is_the_same_bytes_without_the_pictures() {
        use crate::session::{finish_tile_as, tests::draped_content, Finish, TextureMemo};
        use tuile_core::source::TileId;

        let pack_of = |finish: Finish| -> Vec<u8> {
            let noted = tuile_planetary::Provenance::default();
            let memo = Arc::new(TextureMemo::default());
            let tiles: Vec<Arc<TileGeometry>> = [7u64, 8, 9]
                .into_iter()
                .map(|id| {
                    let tile = TileId(id);
                    let (level, x, y) = tile.terrain_coord();
                    let content = draped_content();
                    noted.fetched_terrain(
                        tuile_terrain::TileCoord::new(level, x, y),
                        content.local_origin_ecef.to_array(),
                        tuile_planetary::digest(&id.to_le_bytes()),
                    );
                    noted.fetched_imagery(content.imagery[0].coord, b"imagery bytes");
                    Arc::new(finish_tile_as(
                        &memo,
                        "t",
                        tile,
                        content,
                        256,
                        Some(&noted),
                        finish,
                    ))
                })
                .collect();
            let frame = Frame::with_memo("t", tiles, memo);
            let dir = tempfile::tempdir().expect("tempdir");
            let mut writer =
                PackWriter::new("scene".to_string(), [0.0; 3], dir.path().join("blob.part"))
                    .expect("writer")
                    .with_content(tuile_pack::Content::References, "terrain", "imagery");
            let mut open = writer.begin_frame(
                1,
                tuile_pack::BakedView {
                    position: [1.0, 2.0, 3.0],
                    direction: [0.0, 0.0, -1.0],
                    up: [0.0, 1.0, 0.0],
                    viewport_px: [540.0, 960.0],
                    fovy_rad: std::f64::consts::FRAC_PI_4,
                },
            );
            for (index, tile) in frame.tiles.iter().enumerate() {
                open.push(
                    packed_tile(&frame, index, tile, tuile_pack::Content::References)
                        .expect("a tile for the pack"),
                );
            }
            open.end();
            let path = dir.path().join("p.tuilepack");
            writer.finish_to(&path).expect("pack");
            std::fs::read(&path).expect("read")
        };
        let (with, without) = (pack_of(Finish::Pictures), pack_of(Finish::Identity));
        assert!(with.len() > 200, "a pack was written");
        assert_eq!(without, with);
    }

    /// A tile finished by identity has no mesh and no picture to embed: asked
    /// for a pack that carries its tiles, it is refused rather than stored
    /// empty.
    #[test]
    fn a_tile_finished_by_identity_cannot_go_into_a_pack_that_embeds() {
        use crate::session::{finish_tile_as, tests::draped_content, Finish, TextureMemo};
        let memo = Arc::new(TextureMemo::default());
        let tile = Arc::new(finish_tile_as(
            &memo,
            "t",
            tuile_core::source::TileId(7),
            draped_content(),
            256,
            None,
            Finish::Identity,
        ));
        let frame = Frame::with_memo("t", vec![Arc::clone(&tile)], memo);
        for content in [tuile_pack::Content::Embedded, tuile_pack::Content::Both] {
            assert!(
                packed_tile(&frame, 0, &tile, content).is_err(),
                "{content:?}"
            );
        }
        // And by reference it needs its references.
        assert!(packed_tile(&frame, 0, &tile, tuile_pack::Content::References).is_err());
    }

    /// Frame `number`'s tiles as the writer takes them: a tile of its own,
    /// and the tile of the frame before, which the pack holds by then.
    fn ready(number: u32) -> Ready {
        let tile = |id: u64| BakedTile {
            id,
            drape: 0,
            origin_ecef: [f64::from(number), 0.0, 0.0],
            positions: vec![number as u8; 36],
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: vec![0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0],
            vertex_count: 3,
            index_count: 3,
            base_color_factor: [1.0; 4],
            texture_format: TextureFormat::None,
            refs: None,
            texture: None,
        };
        let mut entries = vec![Entry::New(Box::new(tile(u64::from(number))))];
        if number > 1 {
            entries.push(Entry::Known {
                id: u64::from(number - 1),
                drape: 0,
            });
        }
        Ready {
            number,
            view: tuile_pack::BakedView {
                position: [f64::from(number), 2.0, 3.0],
                direction: [0.0, 0.0, -1.0],
                up: [0.0, 1.0, 0.0],
                viewport_px: [540.0, 960.0],
                fovy_rad: std::f64::consts::FRAC_PI_4,
            },
            origin: [1.0, 2.0, 3.0],
            entries: Ok(entries),
            selected: 2,
            reused: 1,
            selected_in: std::time::Duration::ZERO,
            pose_in: std::time::Duration::ZERO,
        }
    }

    /// The writer writes frame after frame whatever order they are made
    /// ready in: the pack of frames made ready backwards, or shuffled, is
    /// the pack of frames made ready in order — and a frame that refers to
    /// a tile of the frame before finds it there.
    #[test]
    fn frames_made_ready_in_any_order_are_written_in_theirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let content = PackContent::embedded();
        let packed = PackedMirror::new();
        let pack_of = |order: &[u32]| -> Vec<u8> {
            let out = dir.path().join("p.tuilepack");
            let job = BakeFrames {
                first: 1,
                last: 6,
                viewport: (540.0, 960.0),
                out: &out,
                scene: SceneName::Given("scene".into()),
                culling: "full",
                content: &content,
                packed: &packed,
            };
            let mut written = 0;
            let writer = write_in_order(
                &job,
                &dir.path().join("blob.part"),
                order.iter().map(|n| ready(*n)),
                || written += 1,
            )
            .expect("written")
            .expect("a writer");
            assert_eq!(written, order.len());
            writer.finish_to(&out).expect("finished");
            std::fs::read(&out).expect("read")
        };
        let in_order = pack_of(&[1, 2, 3, 4, 5, 6]);
        assert!(pack_of(&[6, 5, 4, 3, 2, 1]) == in_order, "backwards");
        assert!(pack_of(&[2, 1, 4, 6, 3, 5]) == in_order, "shuffled");
        // A frame that never came is not written past.
        let dir_out = dir.path().join("q.tuilepack");
        let job = BakeFrames {
            first: 1,
            last: 3,
            viewport: (540.0, 960.0),
            out: &dir_out,
            scene: SceneName::Given("scene".into()),
            culling: "full",
            content: &content,
            packed: &packed,
        };
        assert!(write_in_order(
            &job,
            &dir.path().join("blob.part"),
            [1, 3].map(ready),
            || ()
        )
        .is_err());
    }

    #[test]
    fn the_mirror_answers_for_what_was_inserted_only() {
        let mirror = PackedMirror::new();
        let held = mirror.held_drape();
        assert!(!(held.0)(7, 9));
        mirror.hold(7, 9);
        assert!((held.0)(7, 9));
        assert!(!(held.0)(7, 10));
    }
}
