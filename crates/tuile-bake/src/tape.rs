// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A tape's frames baked into a pack: the bake a program asks for, as a
//! function.
//!
//! Everything a pack depends on is in [`TapeBake`], and [`TapeBake::scene`]
//! names it before anything is fetched: two bakes that would not give the
//! same pack do not have the same name, and a pack found under a name was
//! baked with what the name says.

use std::path::{Path, PathBuf};

use crate::bake::{
    bake_frames, bake_settings, digest_of_scene, BakeFrames, BakeOutcome, PackContent,
    PackedMirror, Poses, SceneName,
};
use crate::{exact_traversal, source_namespace, tiles::Tiles, Finish, GlobeConfig, Session};

/// The bake of a tape: its cameras, and every setting a pack is the bake of.
#[derive(Debug, Clone)]
pub struct TapeBake {
    /// The source's token. Not read for the open sources
    /// ([`crate::open`]).
    pub token: String,
    /// Where source tiles are kept between two bakes.
    pub cache_dir: Option<PathBuf>,
    /// The tape's cameras: frame `n` is `poses[n - 1]`.
    pub poses: Vec<tuile_tape::Frame>,
    /// The picture's size, in pixels.
    pub viewport: (f64, f64),
    /// The selection's largest screen-space error. `None`: the default.
    pub sse: Option<f64>,
    /// The terrain's asset. `None`: the default.
    pub terrain: Option<i64>,
    /// The imagery's asset: `None` the default, negative none at all.
    pub imagery: Option<i64>,
    /// What the pack carries of its tiles.
    pub content: tuile_pack::Content,
    /// Where the ground comes from ([`crate::open::Ground`]): ion's assets
    /// above, or an open source that needs no token.
    pub ground: crate::open::Ground,
}

impl TapeBake {
    /// The cameras of the tape at `path`, in order.
    pub fn poses_of(path: &Path) -> Result<Vec<tuile_tape::Frame>, String> {
        let mut tape = tuile_tape::Tape::replaying(path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        let mut poses = Vec::new();
        while let Some(frame) = tape.next_frame() {
            poses.push(frame);
        }
        if poses.is_empty() {
            return Err(format!("{} holds no camera frames", path.display()));
        }
        Ok(poses)
    }

    /// The tile store's layers its sources need declared.
    pub fn store_layers(&self) -> Vec<tuile_tile_server::LayerDef> {
        let config = self.config();
        crate::open::store_layers(
            self.ground,
            (config.terrain_asset_id, config.imagery_asset_id),
        )
    }

    /// Whether a bake of it needs an ion token.
    pub fn needs_token(&self) -> bool {
        self.ground.needs_token()
    }

    /// The globe as this bake sets it, before a session is opened on it.
    fn config(&self) -> GlobeConfig {
        let mut config = GlobeConfig::new(&self.token);
        config.cache_dir = self.cache_dir.clone();
        // Set before the traversal is resolved, hence before the digest.
        if let Some(sse) = self.sse {
            config.session.traversal.maximum_screen_space_error = sse;
        }
        if let Some(id) = self.terrain {
            config.terrain_asset_id = id;
        }
        if let Some(id) = self.imagery {
            config.imagery_asset_id = if id < 0 { None } else { Some(id) };
        }
        // The open sources answer for themselves: no token is read.
        let open = match self.ground {
            crate::open::Ground::Ion => None,
            crate::open::Ground::OpenEarth => {
                Some((crate::open::ELLIPSOID, Some(crate::open::BLUE_MARBLE)))
            }
            crate::open::Ground::Empty => Some((crate::open::NOTHING, None)),
        };
        // An open imagery over ion's relief: ion's terrain, the imagery
        // answered by the open sources.
        if open.is_none() && config.imagery_asset_id.is_some_and(crate::open::is_open_imagery) {
            config.imagery_sources = Some(crate::globe::SourcesHandle(std::sync::Arc::new(
                crate::open::OpenSources::new(self.cache_dir.clone()),
            )));
        }
        if let Some((terrain, imagery)) = open {
            config.terrain_asset_id = terrain;
            config.imagery_asset_id = imagery;
            config.sources = Some(crate::globe::SourcesHandle(std::sync::Arc::new(
                crate::open::OpenSources::new(self.cache_dir.clone()),
            )));
        }
        // A pack that only refers to the tile store holds no mesh and no
        // picture: its tiles are named, not drawn.
        if self.content == tuile_pack::Content::References {
            config.session.finish = Finish::Identity;
        }
        config
    }

    /// The tile store's layers this bake's tiles live in: the terrain's, and
    /// the imagery's if there is one.
    pub fn layers(&self) -> (String, Option<String>) {
        let config = self.config();
        (
            source_namespace(config.terrain_asset_id),
            config.imagery_asset_id.map(source_namespace),
        )
    }

    /// The scene's name: over the whole tape, the picture's size and the
    /// resolved settings — not over the frames baked, so that every pack of
    /// one film agrees on it.
    pub fn scene(&self) -> String {
        let config = self.config();
        let resolved = exact_traversal(config.session.traversal.clone());
        digest_of_scene(
            &self.poses,
            self.viewport,
            &bake_settings(&config, &resolved),
        )
    }

    /// The prefix of this bake's packs in a run store:
    /// `packs/<scene>[.references|.both]`.
    pub fn packs(&self) -> String {
        let kind = match self.content {
            tuile_pack::Content::References => ".references",
            tuile_pack::Content::Both => ".both",
            _ => "",
        };
        format!("packs/{}{kind}", self.scene())
    }

    /// Bakes frames `first..=last` into the pack at `out`. `tiles` is the
    /// shared tile store, when there is one: read before the source is
    /// asked, and offered what the source served.
    pub fn bake(
        &self,
        first: u32,
        last: u32,
        out: &Path,
        tiles: Option<&Tiles>,
    ) -> Result<BakeOutcome, String> {
        let wanted = (last as usize).min(self.poses.len());
        if first == 0 || first as usize > wanted {
            return Err(format!(
                "frames {first}:{last} of a tape that holds {}",
                self.poses.len()
            ));
        }
        let mut config = self.config();
        let layers = self.layers();
        if let Some(tiles) = tiles {
            let mut namespaces = vec![layers.0.clone()];
            namespaces.extend(layers.1.clone());
            tiles.check(&namespaces);
            // The scene's slice of the store, local before the first frame,
            // as far as this traversal reads at this picture's height.
            let poses = &self.poses[first as usize - 1..wanted];
            let traversal = exact_traversal(config.session.traversal.clone());
            let sight = crate::tiles::Sight {
                terrain_layer: &layers.0,
                imagery_layer: layers.1.as_deref(),
                traversal: &traversal,
                viewport_height_px: self.viewport.1,
            };
            // A pack of references draws no imagery: its imagery is projected
            // by the archives' indexes, not their bytes.
            if config.session.finish == Finish::Identity {
                tiles.project_by_reference(poses, &sight)?;
            } else {
                tiles.project(poses, &sight)?;
            }
            config.tile_cache = Some(tiles.cache());
        }
        // What the pack already holds, mirrored for the loader so it never
        // fetches a drape twice.
        let packed = PackedMirror::new();
        config.held_drape = Some(packed.held_drape());
        let resolved = exact_traversal(config.session.traversal.clone());
        let scene = digest_of_scene(
            &self.poses,
            self.viewport,
            &bake_settings(&config, &resolved),
        );
        let culling = if resolved.cull { "full" } else { "disabled" };
        let content = PackContent {
            content: self.content,
            terrain_layer: layers.0,
            imagery_layer: layers.1.unwrap_or_default(),
        };
        tracing::info!(
            scene,
            culling,
            sse = resolved.maximum_screen_space_error,
            frames = format!("{first}:{wanted}"),
            viewport = format!("{}x{}", self.viewport.0, self.viewport.1),
            "BAKE-BEGIN"
        );
        let mut session = Session::globe(config).map_err(|e| format!("opening the globe: {e}"))?;
        bake_frames(
            &mut session,
            &BakeFrames {
                first,
                last: wanted as u32,
                viewport: self.viewport,
                out,
                scene: SceneName::Given(scene),
                culling,
                content: &content,
                packed: &packed,
            },
            &mut Poses(&self.poses),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(x: f64) -> tuile_tape::Frame {
        tuile_tape::Frame {
            position: [6_400_000.0 + x, 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            fovy: 0.8,
        }
    }

    fn bake() -> TapeBake {
        TapeBake {
            token: "a token".into(),
            cache_dir: None,
            poses: vec![pose(0.0), pose(10.0), pose(20.0)],
            viewport: (1920.0, 1080.0),
            sse: Some(3.0),
            terrain: None,
            imagery: None,
            content: tuile_pack::Content::References,
            ground: crate::open::Ground::Ion,
        }
    }

    #[test]
    fn no_ground_is_baked_through_the_same_stages_with_no_token() {
        let sky = TapeBake {
            token: String::new(),
            ground: crate::open::Ground::Empty,
            ..bake()
        };
        assert!(!sky.needs_token());
        assert_ne!(sky.scene(), bake().scene());
        assert_eq!(sky.layers().1, None);
        let dir = tempfile::tempdir().expect("dir");
        let out = dir.path().join("sky.tuilepack");
        let done = sky.bake(2, 3, &out, None).expect("baked with no token");
        assert_eq!(done.poses.len(), 2);
        let bytes = std::fs::read(&out).expect("read");
        let pack = tuile_pack::Pack::open(&bytes).expect("open");
        assert_eq!(pack.frame_range(), (2, 3));
        // The two roots, and no triangle in either.
        assert!(pack.tile_count() <= 2, "{}", pack.tile_count());
    }

    #[test]
    fn an_open_imagery_drapes_ions_relief_and_names_its_own_scene() {
        let nasa = TapeBake {
            imagery: Some(crate::open::BLUE_MARBLE),
            ..bake()
        };
        let config = nasa.config();
        // Ion's relief, the open imagery: only the imagery is asked elsewhere.
        assert!(config.sources.is_none());
        assert!(config.imagery_sources.is_some());
        assert_eq!(config.terrain_asset_id, crate::CESIUM_WORLD_TERRAIN);
        assert!(nasa.needs_token());
        assert_eq!(nasa.layers().1.as_deref(), Some("asset-1000000101"));
        assert_ne!(nasa.scene(), bake().scene());
        assert!(bake().config().imagery_sources.is_none());
    }

    #[test]
    fn a_scene_is_named_by_what_a_pack_depends_on_and_by_nothing_else() {
        let scene = bake().scene();
        // The same asked twice, by whoever, wherever its tiles are kept.
        assert_eq!(bake().scene(), scene);
        assert_eq!(
            TapeBake {
                token: "another".into(),
                ..bake()
            }
            .scene(),
            scene
        );
        assert_eq!(
            TapeBake {
                cache_dir: Some("/elsewhere".into()),
                ..bake()
            }
            .scene(),
            scene
        );
        // What the pack carries is not the scene: it is beside it in the key.
        let embedded = TapeBake {
            content: tuile_pack::Content::Embedded,
            ..bake()
        };
        assert_eq!(embedded.scene(), scene);
        assert_eq!(bake().packs(), format!("packs/{scene}.references"));
        assert_eq!(embedded.packs(), format!("packs/{scene}"));

        // Anything that changes a frame changes the name.
        let others = [
            TapeBake {
                sse: Some(4.0),
                ..bake()
            },
            TapeBake {
                viewport: (3840.0, 2160.0),
                ..bake()
            },
            TapeBake {
                imagery: Some(3954),
                ..bake()
            },
            TapeBake {
                imagery: Some(-1),
                ..bake()
            },
            TapeBake {
                terrain: Some(7),
                ..bake()
            },
            TapeBake {
                poses: vec![pose(0.0), pose(10.0), pose(21.0)],
                ..bake()
            },
        ];
        let mut names: Vec<String> = others.iter().map(TapeBake::scene).collect();
        names.push(scene);
        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "{names:?}");
    }

    #[test]
    fn frames_the_tape_does_not_hold_are_refused_before_anything_is_opened() {
        let out = std::path::Path::new("/nowhere/pack");
        assert!(bake().bake(0, 2, out, None).is_err());
        assert!(bake().bake(4, 9, out, None).is_err());
    }
}
