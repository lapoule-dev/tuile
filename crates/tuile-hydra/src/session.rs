// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One streaming session, driven a frame at a time.
//!
//! Everything the C ABI exposes is a thin projection of what is here, so this
//! module is testable in plain Rust without going through the boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glam::DVec3;
use tuile_core::content::DecodedTileContent;
use tuile_core::drive::drive_until_complete;
use tuile_core::runtime::in_process_with;
use tuile_core::source::{TileId, TileLoader, TileTree};
use tuile_core::traversal::{Config, ViewState};

/// What a renderer must decide before the first frame.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub traversal: Config,
    /// Longest a single frame may take to converge.
    ///
    /// A bulk frame blocks until every selected tile is resident, and there are
    /// two ways that never happens: a fetch that hangs without failing, and a
    /// resident budget too small for the frame's working set, where eviction
    /// during convergence keeps the request count above zero. Neither is
    /// hypothetical, and on a farm both look identical — a node that stopped.
    /// This turns them into a reported failure.
    pub frame_timeout: Duration,
    /// Whether a frame that converged but lost tiles along the way is an error.
    ///
    /// Default `true`, and it should stay true anywhere the output is kept. A
    /// failed tile does not leave a hole: its ancestor stands in, so the frame
    /// renders plausibly at the wrong level of detail. That is the worst kind of
    /// defect — invisible until someone compares two frames rendered on
    /// different machines.
    pub fail_on_tile_errors: bool,
    /// Names the data this session serves, and scopes every asset URI it hands
    /// out. See [`Frame::texture_uri`] for why it exists and why it is a name
    /// rather than a counter.
    ///
    /// It must be stable for the same sources and distinct for different ones.
    /// [`Session::globe`] derives it from the ion asset ids; a host wiring its
    /// own sources chooses its own, and choosing badly means one session's
    /// textures answering another's requests.
    pub dataset: String,
    /// Largest side of the per-tile imagery mosaic baked before the boundary.
    ///
    /// Draped imagery crosses the ABI as one owned texture per tile
    /// ([`tuile_core::raster::bake_imagery`]) — a Hydra host authors one
    /// `UsdUVTexture` per tile and knows nothing about layers. This caps the
    /// bake; the bake itself picks the finest layer's scale below it.
    pub bake_max_size: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            traversal: Config::default(),
            frame_timeout: Duration::from_secs(120),
            fail_on_tile_errors: true,
            // Deliberately not "" — an empty dataset would collapse the URI to
            // `tuile:///tile/...`, which resolves but scopes nothing.
            dataset: "default".into(),
            bake_max_size: 2048,
        }
    }
}

/// Why a frame did not produce geometry.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame did not converge within {0:?}")]
    TimedOut(Duration),
    #[error("the geometry server hung up")]
    ServerGone,
    #[error("{count} tile(s) failed to load; first: {first}")]
    TilesFailed { count: usize, first: String },
    #[error("encoding a texture: {0}")]
    TextureEncode(String),
    /// A thread panicked while holding the frame's encode map. Only reachable
    /// if something above already went wrong, but the boundary must not turn it
    /// into a second panic.
    #[error("the frame's texture cache was left poisoned")]
    Poisoned,
}

/// One tile's geometry, ready to hand to a renderer.
///
/// Positions are f32 **relative to `origin_ecef`** — the anti-jitter protocol.
/// A consumer places the tile with a double-precision transform built from
/// `origin_ecef` and its own render origin; going through f32 for that
/// subtraction is exactly what the protocol exists to avoid.
#[derive(Debug)]
pub struct TileGeometry {
    pub tile: TileId,
    pub origin_ecef: DVec3,
    pub content: DecodedTileContent,
}

/// A texture, as the renderer will ask for it.
#[derive(Debug)]
pub struct EncodedTexture {
    /// The `tuile://` URI a material points at, and the key the host's asset
    /// resolver will be handed back.
    pub uri: String,
    /// PNG bytes.
    pub png: Vec<u8>,
}

/// A converged answer for one camera.
pub struct Frame {
    /// Tiles in traversal order.
    ///
    /// Ordered on purpose: the driver also returns a `HashMap`, whose iteration
    /// order is seeded per process, so a consumer that walked it would emit
    /// prims in a different order on every run — and a farm comparing two
    /// renders of the same frame would see a difference that is not there.
    pub tiles: Vec<TileGeometry>,
    /// Textures encoded so far, keyed by (tile index, texture index).
    ///
    /// Filled on demand and never evicted, because the C boundary hands out
    /// borrows that stay valid until the frame is freed. Encoding is deferred
    /// rather than done up front for the ordinary reason: a renderer asks for
    /// the textures it will actually sample, which after frustum and material
    /// culling is rarely all of them.
    encoded: Mutex<HashMap<(usize, usize), Arc<EncodedTexture>>>,
    /// Scopes this frame's asset URIs. See [`Frame::texture_uri`].
    dataset: Arc<str>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("tiles", &self.tiles.len())
            .finish_non_exhaustive()
    }
}

impl Frame {
    /// Builds a frame from its tiles, with nothing encoded yet.
    pub fn new(dataset: impl Into<Arc<str>>, tiles: Vec<TileGeometry>) -> Self {
        Self {
            tiles,
            encoded: Mutex::new(HashMap::new()),
            dataset: dataset.into(),
        }
    }

    /// What scopes this frame's asset URIs.
    pub fn dataset(&self) -> &str {
        &self.dataset
    }

    /// The URI under which a tile's texture is published.
    ///
    /// Defined here, in one place, because both sides must agree: the material
    /// a consumer authors names this string and the asset resolver is handed it
    /// back verbatim. Deriving it independently on each side is how they drift
    /// and every texture silently resolves to nothing.
    ///
    /// # Why the dataset is in the path
    ///
    /// A [`TileId`] is unique **within one tree**, not globally. Two sessions —
    /// two ion assets, or a terrain-only globe beside a textured one — hand out
    /// the same ids for different tiles, and a resolver keyed on the id alone
    /// would serve one session's texture to the other. Silently: the bytes are
    /// a valid PNG, so nothing errors, and the wrong imagery simply appears.
    ///
    /// # Why a name and not a counter
    ///
    /// The dataset is derived from what the session reads — for an ion globe,
    /// its asset ids — rather than assigned from a counter at open time. A
    /// counter depends on the order sessions happen to be opened, so two farm
    /// nodes rendering the same frame would emit different asset paths for
    /// identical data, and a comparison would report a difference that is not
    /// there. Same reason `BulkFrame.selected` is iterated rather than its
    /// `HashMap`.
    pub fn texture_uri(dataset: &str, tile: TileId, texture: usize) -> String {
        format!("tuile://{dataset}/tile/{}/texture/{texture}.png", tile.0)
    }

    /// The PNG for one tile's texture, encoding it on first request.
    ///
    /// Returns `None` for an out-of-range index. The `Arc` is what makes the
    /// borrow handed across the C boundary safe: the bytes live as long as the
    /// frame, whatever the caller does with the map afterwards.
    pub fn texture_png(
        &self,
        tile_index: usize,
        texture_index: usize,
    ) -> Result<Option<Arc<EncodedTexture>>, FrameError> {
        let Some(tile) = self.tiles.get(tile_index) else {
            return Ok(None);
        };
        let Some(texture) = tile.content.textures.get(texture_index) else {
            return Ok(None);
        };

        let key = (tile_index, texture_index);
        if let Some(hit) = self
            .encoded
            .lock()
            .map_err(|_| FrameError::Poisoned)?
            .get(&key)
        {
            return Ok(Some(Arc::clone(hit)));
        }

        // Encoded outside the lock: this is milliseconds of CPU per texture,
        // and holding the map while it runs would serialise every other tile's
        // encode behind it for no reason.
        let mut png = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut png),
            &texture.rgba8,
            texture.width,
            texture.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(|e| FrameError::TextureEncode(e.to_string()))?;

        let entry = Arc::new(EncodedTexture {
            uri: Self::texture_uri(&self.dataset, tile.tile, texture_index),
            png,
        });

        // Another thread may have won the race; keep whichever landed first so
        // every caller sees one buffer at one address.
        let mut map = self.encoded.lock().map_err(|_| FrameError::Poisoned)?;
        Ok(Some(Arc::clone(
            map.entry(key).or_insert_with(|| Arc::clone(&entry)),
        )))
    }
}

impl std::fmt::Debug for Session {
    /// Deliberately says almost nothing. A session owns a tokio runtime, a
    /// server thread and a live stream, none of which have a useful textual
    /// form — and its configuration can carry an ion token, which must not end
    /// up in a log line because someone printed a `Result`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("frame_timeout", &self.config.frame_timeout)
            .finish_non_exhaustive()
    }
}

/// A live session over one tile source.
pub struct Session {
    stream: tuile_core::protocol::InProcessStream,
    runtime: tokio::runtime::Runtime,
    config: SessionConfig,
    /// Kept alive for as long as the session: dropping it ends the server.
    _server: std::thread::JoinHandle<()>,
}

impl Session {
    /// Starts a session over an already-resolved tree and loader.
    ///
    /// The host resolves its own sources — this crate knows nothing about ion,
    /// Bing or HTTP, exactly like every other consumer of the core.
    pub fn new(
        tree: Box<dyn TileTree>,
        loader: Arc<dyn TileLoader>,
        config: SessionConfig,
    ) -> std::io::Result<Self> {
        Self::from_parts(Self::runtime()?, tree, loader, config)
    }

    /// The runtime a session drives everything on.
    ///
    /// Exposed to the crate because resolving sources is itself async — an ion
    /// endpoint and a Bing metadata document have to be fetched before there is
    /// a tree to hand over — and doing that on a second runtime would mean two
    /// thread pools and two sets of connections for one session.
    pub(crate) fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
    }

    /// Starts a session on an already-built runtime.
    pub(crate) fn from_parts(
        runtime: tokio::runtime::Runtime,
        tree: Box<dyn TileTree>,
        loader: Arc<dyn TileLoader>,
        config: SessionConfig,
    ) -> std::io::Result<Self> {
        let (stream, server) =
            in_process_with(tree, loader, exact_traversal(config.traversal.clone()));

        // The server runs on its own runtime handle so a frame call can block
        // the calling thread — which is what Hydra's cook wants — without
        // deadlocking against the loads it is waiting for.
        let handle = runtime.handle().clone();
        let server_thread = std::thread::Builder::new()
            .name("tuile-hydra-server".into())
            .spawn(move || handle.block_on(server.run()))?;

        Ok(Self {
            stream,
            runtime,
            config,
            _server: server_thread,
        })
    }

    /// Resolves one frame for the given views, blocking until it converges.
    ///
    /// Multiple views are a union, not a choice: stereo pairs must share one
    /// selection or the eyes drift apart at LOD boundaries.
    pub fn frame(&mut self, views: Vec<ViewState>) -> Result<Frame, FrameError> {
        let timeout = self.config.frame_timeout;
        let stream = &mut self.stream;

        let bulk = self.runtime.block_on(async {
            tokio::time::timeout(timeout, drive_until_complete(stream, views)).await
        });

        let bulk = match bulk {
            Ok(Ok(frame)) => frame,
            Ok(Err(_closed)) => return Err(FrameError::ServerGone),
            Err(_elapsed) => return Err(FrameError::TimedOut(timeout)),
        };

        if self.config.fail_on_tile_errors && !bulk.errors.is_empty() {
            let first = bulk
                .errors
                .first()
                .map(|(_, message): &(Option<TileId>, String)| message.clone())
                .unwrap_or_default();
            return Err(FrameError::TilesFailed {
                count: bulk.errors.len(),
                first,
            });
        }

        let mut contents = bulk.contents;
        let bake_max_size = self.config.bake_max_size;
        let tiles = bulk
            .selected
            .iter()
            .filter_map(|(tile, _sse)| {
                let content = contents.remove(tile)?;
                let tuile_core::content::TileContent::Decoded(decoded) = content else {
                    // The in-process binding always decodes; raw bytes here
                    // would mean the server was misconfigured.
                    return None;
                };
                Some(finish_tile(*tile, decoded, bake_max_size))
            })
            .collect();

        Ok(Frame::new(self.config.dataset.as_str(), tiles))
    }
}

/// The traversal a bulk frame runs, whatever the caller asked for.
///
/// Stand-ins are an interactive kindness — a plausible surface while the real
/// tile is in flight. A bulk frame has no "while": it converges or it fails,
/// and a stand-in that slipped into a kept frame is the invisible defect
/// docs/15 forbids (two farm nodes disagreeing about which tiles were real).
/// Holes are forbidden for the same reason, whichever way the caller's config
/// leans.
fn exact_traversal(mut config: Config) -> Config {
    config.stand_ins = false;
    config.forbid_holes = true;
    config
}

/// One decoded tile, made boundary-ready.
///
/// Draped imagery is baked to a single owned texture here — before anything
/// crosses the ABI — so the host sees `textures` and a `base_color_texture`
/// index and never the layer stack. Without this, every ion terrain tile
/// crosses with `textures` empty and `base_color_texture == -1`.
fn finish_tile(
    tile: TileId,
    mut decoded: DecodedTileContent,
    bake_max_size: u32,
) -> TileGeometry {
    tuile_core::raster::bake_imagery(&mut decoded, bake_max_size);
    TileGeometry {
        tile,
        origin_ecef: decoded.local_origin_ecef,
        content: decoded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::content::DecodedTexture;

    /// A frame holding one tile with one small opaque texture.
    fn frame_with_a_texture() -> Frame {
        Frame::new(
            "ion-1-2",
            vec![TileGeometry {
                tile: TileId(7),
                origin_ecef: DVec3::ZERO,
                content: DecodedTileContent {
                    meshes: Vec::new(),
                    textures: vec![DecodedTexture {
                        width: 2,
                        height: 2,
                        rgba8: vec![255u8; 2 * 2 * 4],
                    }],
                    imagery: Vec::new(),
                    local_origin_ecef: DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                },
            }],
        )
    }

    /// Both sides derive the texture's name from this one function, so it is
    /// worth pinning: a change here silently unresolves every material.
    #[test]
    fn the_texture_uri_is_stable_and_scheme_qualified() {
        assert_eq!(
            Frame::texture_uri("ion-1-2", TileId(7), 0),
            "tuile://ion-1-2/tile/7/texture/0.png"
        );
    }

    /// The whole reason the dataset is in the path: a TileId is unique within
    /// one tree, so two sessions hand out the same ids for different tiles and
    /// a resolver keyed on the id alone would answer with the wrong imagery —
    /// silently, since the bytes are a valid PNG either way.
    #[test]
    fn two_datasets_never_collide_on_a_tile_id() {
        assert_ne!(
            Frame::texture_uri("ion-1-2", TileId(7), 0),
            Frame::texture_uri("ion-1-3812", TileId(7), 0)
        );
    }

    #[test]
    fn a_texture_encodes_to_png() {
        let frame = frame_with_a_texture();
        let texture = frame
            .texture_png(0, 0)
            .expect("encoding")
            .expect("the texture exists");
        assert_eq!(texture.uri, "tuile://ion-1-2/tile/7/texture/0.png");
        // The PNG signature, so this is decodable bytes rather than the raw
        // RGBA the host's image plugin cannot read.
        assert_eq!(&texture.png[..8], b"\x89PNG\r\n\x1a\n");
    }

    /// The C boundary hands out a pointer into these bytes and promises it
    /// stays valid for the frame's life. That only holds if the second call
    /// returns the same allocation rather than encoding again.
    #[test]
    fn encoding_happens_once_and_the_bytes_do_not_move() {
        let frame = frame_with_a_texture();
        let first = frame.texture_png(0, 0).expect("encoding").expect("present");
        let second = frame.texture_png(0, 0).expect("encoding").expect("present");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.png.as_ptr(), second.png.as_ptr());
    }

    /// An untextured tile and a past-the-end index are ordinary answers, not
    /// errors — a caller enumerates until one of them comes back.
    #[test]
    fn absent_textures_report_absence_rather_than_failing() {
        let frame = frame_with_a_texture();
        assert!(frame.texture_png(0, 1).expect("no error").is_none());
        assert!(frame.texture_png(9, 0).expect("no error").is_none());
    }

    #[test]
    fn the_default_frame_timeout_is_finite() {
        // A bulk frame can block forever on a hung fetch or a budget too small
        // to converge. The default must bound that, not inherit it.
        let config = SessionConfig::default();
        assert!(config.frame_timeout > Duration::ZERO);
        assert!(config.frame_timeout < Duration::from_secs(3600));
    }

    /// A failed tile does not leave a hole — an ancestor stands in — so a frame
    /// can look fine and be wrong. Anything keeping its output wants this loud.
    #[test]
    fn tile_errors_fail_the_frame_by_default() {
        assert!(SessionConfig::default().fail_on_tile_errors);
    }

    /// Whatever the caller's traversal config says, a bulk frame is exact:
    /// no stand-in surface may reach a kept frame, and holes stay forbidden.
    #[test]
    fn bulk_traversal_is_exact_whatever_the_caller_asked() {
        let mut config = Config::default();
        config.stand_ins = true;
        config.forbid_holes = false;
        let exact = exact_traversal(config);
        assert!(!exact.stand_ins);
        assert!(exact.forbid_holes);
    }

    /// A tile with draped imagery must cross the boundary as one owned
    /// texture: the bake happens in the session, before the ABI, or every ion
    /// terrain tile reports `base_color_texture == -1` and renders untextured.
    #[test]
    fn a_finished_tile_owns_its_mosaic() {
        use tuile_core::content::{DecodedMesh, MaterialDesc};
        use tuile_core::geo::{geodetic_to_ecef, Geodetic};
        use tuile_core::raster::{
            uvs_geographic, GeoRect, ImageryCoord, ImageryLayer,
        };

        let rect = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 0.01,
            north: 0.01,
        };
        let origin = geodetic_to_ecef(Geodetic {
            lon: 0.005,
            lat: 0.005,
            height: 0.0,
        });
        let positions = vec![[0.0f32; 3]; 3];
        let uvs = uvs_geographic(&positions, origin, &rect);
        let decoded = DecodedTileContent {
            meshes: vec![DecodedMesh {
                positions,
                normals: None,
                uvs: Some(uvs),
                indices: vec![0, 1, 2],
                material: MaterialDesc {
                    base_color_factor: [1.0; 4],
                    base_color_texture: None,
                },
            }],
            textures: Vec::new(),
            imagery: vec![ImageryLayer {
                coord: ImageryCoord {
                    level: 0,
                    x: 0,
                    y: 0,
                },
                texture: Arc::new(DecodedTexture {
                    width: 2,
                    height: 2,
                    rgba8: vec![128u8; 2 * 2 * 4],
                }),
                coverage: [-0.1, -0.1, 1.1, 1.1],
                translation: [0.0, 0.0],
                scale: [1.0, 1.0],
            }],
            local_origin_ecef: origin,
            transform_local: glam::Mat4::IDENTITY,
        };

        let tile = finish_tile(TileId(7), decoded, 256);
        assert_eq!(tile.content.textures.len(), 1, "the mosaic is owned");
        assert!(tile.content.imagery.is_empty(), "the layers are consumed");
        assert_eq!(
            tile.content.meshes[0].material.base_color_texture,
            Some(0),
            "the material points at the baked texture"
        );
    }
}
