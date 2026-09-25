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

use std::collections::HashSet;
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

    fn insert(&self, id: u64, drape: u64) {
        if let Ok(mut held) = self.0.lock() {
            held.insert((id, drape));
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
    pub packed: &'a PackedMirror,
}

/// What a bake produced.
#[derive(Debug)]
pub struct BakeOutcome {
    pub scene: String,
    /// The poses baked, frame `first` first.
    pub poses: Vec<tuile_tape::Frame>,
    pub bytes: u64,
}

/// Bakes `job.first..=job.last` into `job.out`, asking `source` for each pose.
pub fn bake_frames(
    session: &mut Session,
    job: &BakeFrames<'_>,
    source: &mut dyn PoseSource,
) -> Result<BakeOutcome, String> {
    if job.first == 0 || job.first > job.last {
        return Err(format!("frames {}:{} is not a range of frames", job.first, job.last));
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
    let mut writer: Option<PackWriter> = None;
    let mut poses = Vec::with_capacity((job.last - job.first + 1) as usize);

    for number in job.first..=job.last {
        let pose = source.pose(number, session).map_err(|e| format!("frame {number}: {e}"))?;
        let at = Instant::now();
        let frame = session
            .frame_for_pose(&pose, job.viewport)
            .map_err(|e| format!("frame {number}: {e}"))?;

        // The writer is opened on the first pose, which is the render origin
        // the pack's positions are relative to. The pack stores each tile's own
        // ECEF origin, so this is carried for the consumer that rebases, not
        // used to move anything here.
        let writer = match &mut writer {
            Some(w) => w,
            None => writer.insert(
                PackWriter::new(
                    match &job.scene {
                        SceneName::Given(scene) => scene.clone(),
                        SceneName::OfPoses { .. } => String::new(),
                    },
                    pose.position,
                    &spill,
                )
                .map_err(|e| format!("opening {}: {e}", spill.display()))?
                .culling(job.culling),
            ),
        };

        // The camera goes in beside the tiles, because that is what a render
        // will address this frame by: a renderer hands the session a camera,
        // never a frame number.
        let mut open = writer.begin_frame(
            number,
            tuile_pack::BakedView {
                position: pose.position,
                direction: pose.direction,
                up: pose.up,
                viewport_px: [job.viewport.0, job.viewport.1],
                fovy_rad: pose.fovy,
            },
        );
        // One tile at a time, and only the ones the pack does not already
        // hold: an orbit re-selects almost the same ground every frame, and
        // after frame 1 this touches nothing but the index for those.
        let mut selected = 0usize;
        let mut reused = 0usize;
        for (index, tile) in frame.tiles.iter().enumerate() {
            selected += 1;
            if open.push_known(tile.tile.0, tile.drape()) {
                reused += 1;
                continue;
            }
            open.push(baked_tile(&frame, index, tile)?);
            job.packed.insert(tile.tile.0, tile.drape());
        }
        open.end();
        poses.push(pose);
        tracing::info!(frame = number, selected, reused, seconds = at.elapsed().as_secs_f64(), "BAKE-FRAME");
    }

    let scene = match &job.scene {
        SceneName::Given(scene) => scene.clone(),
        SceneName::OfPoses { settings } => digest_of_scene(&poses, job.viewport, settings),
    };
    let mut writer = writer.ok_or("no frame was baked")?;
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
    Ok(BakeOutcome { scene, poses, bytes })
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
pub fn digest_of_scene(poses: &[tuile_tape::Frame], viewport: (f64, f64), settings: &str) -> String {
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
    tuile_pack::scene_digest(&[&path, &viewport.0.to_le_bytes(), &viewport.1.to_le_bytes(), settings.as_bytes()])
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
pub fn baked_tile(frame: &Frame, index: usize, tile: &Arc<TileGeometry>) -> Result<BakedTile, String> {
    // One prim per tile, as the consumer expects; terrain produces one mesh.
    let mesh = tile
        .content
        .meshes
        .first()
        .ok_or_else(|| format!("tile {} carries no mesh", tile.tile.0))?;

    // Index 0 only: a terrain tile carries one draped mosaic. A tile with
    // several would need the pack to hold several, and nothing produces one —
    // so this fails loudly rather than silently baking the first of many.
    let texture = frame.texture_png(index, 0).map_err(|e| format!("tile {}: {e}", tile.tile.0))?;
    if frame
        .texture_png(index, 1)
        .map_err(|e| format!("tile {}: {e}", tile.tile.0))?
        .is_some()
    {
        return Err(format!("tile {} carries more than one texture; the pack holds one", tile.tile.0));
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
        normals: mesh.normals.as_ref().map(|n| f32x3_le(n)).unwrap_or_default(),
        uvs: mesh.uvs.as_ref().map(|u| f32x2_le(u)).unwrap_or_default(),
        indices: u32_le(&mesh.indices),
        vertex_count: mesh.positions.len() as u32,
        index_count: mesh.indices.len() as u32,
        base_color_factor: mesh.material.base_color_factor,
        texture_format: if texture.is_some() { TextureFormat::Png } else { TextureFormat::None },
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
        assert!(!drape_has_its_pixels(0xdead, false), "a drape whose pixels are nowhere");
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
        let mut writer = PackWriter::new(String::new(), [0.0; 3], dir.path().join("blob.part")).expect("writer");
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
        assert_eq!(tuile_pack::Pack::open(&bytes).expect("open").scene_digest(), "named-after-the-poses");
        let session = Session::from_pack(&path, Some("named-after-the-poses")).expect("the late name is the one a render checks");
        assert!(session.heights().is_none());
    }

    #[test]
    fn the_mirror_answers_for_what_was_inserted_only() {
        let mirror = PackedMirror::new();
        let held = mirror.held_drape();
        assert!(!(held.0)(7, 9));
        mirror.insert(7, 9);
        assert!((held.0)(7, 9));
        assert!(!(held.0)(7, 10));
    }
}
