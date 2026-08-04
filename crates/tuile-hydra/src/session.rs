// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One streaming session, driven a frame at a time.
//!
//! Everything the C ABI exposes is a thin projection of what is here, so this
//! module is testable in plain Rust without going through the boundary.

use std::sync::Arc;
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

/// A converged answer for one camera.
#[derive(Debug)]
pub struct Frame {
    /// Tiles in traversal order.
    ///
    /// Ordered on purpose: the driver also returns a `HashMap`, whose iteration
    /// order is seeded per process, so a consumer that walked it would emit
    /// prims in a different order on every run — and a farm comparing two
    /// renders of the same frame would see a difference that is not there.
    pub tiles: Vec<TileGeometry>,
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

        Ok(Frame { tiles })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
