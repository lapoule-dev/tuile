// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The async→frame bridge: drains a [`GeometryStream`] each frame, spreads
//! GPU uploads over frames (no hitches), acknowledges materialized tiles
//! and frees evicted ones.
//!
//! The pump does not know which binding feeds it — in-process, WebSocket
//! or HTTP pull — that's the point of the trait (`docs/11-crate-wgpu.md`).

use crate::context::GpuContext;
use crate::prepare::{prepare, PreparedTile};
use glam::DVec3;
use std::collections::{HashMap, VecDeque};
use std::task::{Context, Poll};
use tuile_core::content::{DecodedTileContent, TileContent};
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage};
use tuile_core::tileset::TileId;
use tuile_core::traversal::TraversalStats;

pub struct ContentPump {
    /// The f64 world point all tiles and the view matrix are relative to.
    pub render_origin: DVec3,
    prepared: HashMap<TileId, PreparedTile>,
    pending: VecDeque<(TileId, DecodedTileContent)>,
    /// Current selection (tile + SSE), as sent by the geometry server.
    pub selection: Vec<(TileId, f64)>,
    pub stats: TraversalStats,
    /// Sum of GPU bytes of prepared tiles.
    pub gpu_bytes: usize,
    /// True once the server side hung up.
    pub closed: bool,
    /// Non-fatal errors reported by the server (drain freely).
    pub errors: Vec<String>,
}

impl ContentPump {
    pub fn new(render_origin: DVec3) -> Self {
        Self {
            render_origin,
            prepared: HashMap::new(),
            pending: VecDeque::new(),
            selection: Vec::new(),
            stats: TraversalStats::default(),
            gpu_bytes: 0,
            closed: false,
            errors: Vec::new(),
        }
    }

    /// Call once per frame: drains every queued server message, then
    /// performs at most `max_uploads` GPU uploads. Returns the number of
    /// tiles uploaded this call.
    pub fn pump<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &GpuContext,
        max_uploads: usize,
    ) -> usize {
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match stream.poll_message(&mut cx) {
                Poll::Ready(Some(msg)) => self.on_message(msg),
                Poll::Ready(None) => {
                    self.closed = true;
                    break;
                }
                Poll::Pending => break,
            }
        }

        let mut uploaded = 0;
        while uploaded < max_uploads {
            let Some((tile, content)) = self.pending.pop_front() else {
                break;
            };
            let prepared = prepare(gpu, &content, self.render_origin);
            self.gpu_bytes += prepared.gpu_bytes;
            self.prepared.insert(tile, prepared);
            // Best effort: a closed stream just means the session is over.
            let _ = stream.send(ClientMessage::Ack { tile });
            uploaded += 1;
        }
        uploaded
    }

    fn on_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Select { tiles, stats } => {
                self.selection = tiles;
                self.stats = stats;
            }
            ServerMessage::Content { tile, content } => match content {
                TileContent::Decoded(decoded) => self.pending.push_back((tile, decoded)),
                TileContent::Raw { .. } => self
                    .errors
                    .push(format!("tile {tile:?}: raw content reached the renderer")),
            },
            ServerMessage::Evict { tiles } => {
                for tile in tiles {
                    if let Some(p) = self.prepared.remove(&tile) {
                        self.gpu_bytes -= p.gpu_bytes;
                    }
                }
            }
            ServerMessage::Error { tile, message } => {
                self.errors.push(match tile {
                    Some(t) => format!("tile {t:?}: {message}"),
                    None => message,
                });
            }
        }
    }

    /// Selected tiles whose GPU resources are ready to draw.
    pub fn visible(&self) -> impl Iterator<Item = &PreparedTile> {
        self.selection
            .iter()
            .filter_map(|(t, _)| self.prepared.get(t))
    }

    /// Number of selected tiles still waiting for content or upload.
    pub fn missing(&self) -> usize {
        self.selection
            .iter()
            .filter(|(t, _)| !self.prepared.contains_key(t))
            .count()
    }

    pub fn prepared_count(&self) -> usize {
        self.prepared.len()
    }

    pub fn pending_uploads(&self) -> usize {
        self.pending.len()
    }
}
