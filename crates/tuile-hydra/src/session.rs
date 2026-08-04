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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            traversal: Config::default(),
            frame_timeout: Duration::from_secs(120),
            fail_on_tile_errors: true,
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
    pub fn new(tiles: Vec<TileGeometry>) -> Self {
        Self {
            tiles,
            encoded: Mutex::new(HashMap::new()),
        }
    }

    /// The URI under which a tile's texture is published.
    ///
    /// Defined here, in one place, because both sides need to agree: the
    /// material a consumer authors names this string, and the asset resolver is
    /// handed it back verbatim. Deriving it independently on each side is how
    /// the two drift and every texture silently resolves to nothing.
    pub fn texture_uri(tile: TileId, texture: usize) -> String {
        format!("tuile://tile/{}/texture/{}.png", tile.0, texture)
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
            uri: Self::texture_uri(tile.tile, texture_index),
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (stream, server) = in_process_with(tree, loader, config.traversal.clone());

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
                Some(TileGeometry {
                    tile: *tile,
                    origin_ecef: decoded.local_origin_ecef,
                    content: decoded,
                })
            })
            .collect();

        Ok(Frame::new(tiles))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::content::DecodedTexture;

    /// A frame holding one tile with one small opaque texture.
    fn frame_with_a_texture() -> Frame {
        Frame::new(vec![TileGeometry {
            tile: TileId(7),
            origin_ecef: DVec3::ZERO,
            content: DecodedTileContent {
                meshes: Vec::new(),
                textures: vec![DecodedTexture {
                    width: 2,
                    height: 2,
                    rgba8: vec![255u8; 2 * 2 * 4],
                }],
                local_origin_ecef: DVec3::ZERO,
                transform_local: glam::Mat4::IDENTITY,
            },
        }])
    }

    /// Both sides derive the texture's name from this one function, so it is
    /// worth pinning: a change here silently unresolves every material.
    #[test]
    fn the_texture_uri_is_stable_and_scheme_qualified() {
        assert_eq!(
            Frame::texture_uri(TileId(7), 0),
            "tuile://tile/7/texture/0.png"
        );
    }

    #[test]
    fn a_texture_encodes_to_png() {
        let frame = frame_with_a_texture();
        let texture = frame
            .texture_png(0, 0)
            .expect("encoding")
            .expect("the texture exists");
        assert_eq!(texture.uri, "tuile://tile/7/texture/0.png");
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
}
