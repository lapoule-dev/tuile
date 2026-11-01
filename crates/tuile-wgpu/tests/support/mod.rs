// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the GPU scenarios share: an adapter or a stated reason there is none,
//! a scripted stream, the messages a server sends, and the loop that drives a
//! real server and the pump until the view has converged.
//!
//! Each scenario uses a part of it; the rest is dead code there.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use glam::DVec3;

use tuile_core::budget::{
    Bytes, Class, DrapeChoice, DrawItem, DrawSet, FrameDeadline, Generation, LayerId, PerClass,
    ResourceId, Role, UploadPlan, UvTransform,
};
use tuile_core::content::{DecodedTexture, DecodedTileContent, DrapeContent};
use tuile_core::protocol::{
    Ancestry, ClientMessage, GeometryStream, ResourceContent, ServerMessage, StreamError,
};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_core::source::TileId;
use tuile_core::texels::{BlockFormat, ImageryFormat};
use tuile_core::traversal::{TraversalStats, ViewState};
use tuile_wgpu::{ContentPump, GpuContext, ImageryCompression};

/// A GPU, or a stated reason there is none.
///
/// `TUILE_REQUIRE_GPU=1` turns the skip into a failure. Without that escape a
/// headless CI would report success for a test it never ran, which is worse
/// than not having the test.
pub fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU test: no usable adapter ({e})");
            None
        }
    }
}

/// The imagery plans every scenario that drapes runs under: what a consumer
/// gets without asking, and each block format the GPU encodes.
pub const PLANS: [(&str, ImageryCompression); 3] = [
    ("off", ImageryCompression::Off),
    ("bc7", ImageryCompression::Format(BlockFormat::Bc7)),
    ("astc", ImageryCompression::Format(BlockFormat::Astc4x4)),
];

/// A GPU holding imagery as `compression` asks, or a stated reason there is
/// none: no adapter (a failure under `TUILE_REQUIRE_GPU=1`), or an adapter
/// that cannot sample the format asked for — a fact about the device, said
/// and skipped.
pub fn gpu_with(compression: ImageryCompression) -> Option<GpuContext> {
    let gpu = match pollster::block_on(GpuContext::headless_with(compression)) {
        Ok(gpu) => gpu,
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU test: no usable adapter ({e})");
            return None;
        }
    };
    if let ImageryCompression::Format(format) = compression {
        if gpu.imagery_plan().target != ImageryFormat::from(format) {
            eprintln!("skipping {format:?}: this adapter does not sample it");
            return None;
        }
    }
    Some(gpu)
}

/// Holds a scenario to the plan it ran under: under a block plan, imagery
/// really was encoded — `at_least` tiles, none fallen back to RGBA8 — so a
/// scenario cannot pass "under BC7" on RGBA8 textures.
pub fn encoded_as_planned(plan: &str, gpu: &GpuContext, at_least: u64) {
    match gpu.imagery_encode_stats() {
        None => assert_eq!(
            gpu.imagery_plan().target,
            ImageryFormat::Rgba8,
            "{plan}: a block plan with no encoder"
        ),
        Some(stats) => {
            assert!(
                stats.encoded >= at_least,
                "{plan}: {} tiles encoded, {at_least} at least expected",
                stats.encoded
            );
            assert_eq!(stats.fell_back, 0, "{plan}: a 256² tile fell back to RGBA8");
        }
    }
}

/// A `width × height` imagery texture painted by `paint(x, y)`, held as
/// `gpu`'s plan says — the mark a loader under that plan puts on it.
pub fn painted(
    gpu: &GpuContext,
    width: u32,
    height: u32,
    paint: impl Fn(u32, u32) -> [u8; 4],
) -> DecodedTexture {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&paint(x, y));
        }
    }
    DecodedTexture::from_rgba8(width, height, pixels).planned(gpu.imagery_plan())
}

/// A drape of `tile` at `level`: one imagery tile, `texture`, over the whole
/// of it, keyed by the tile's own coord so drapes never share it.
pub fn whole_drape(tile: TileId, level: u32, texture: DecodedTexture) -> DrapeContent {
    let (z, x, y) = tile.terrain_coord();
    DrapeContent {
        layer: LayerId(0),
        level,
        tiles: vec![ImageryLayer {
            coord: ImageryCoord { level: z, x, y },
            source: 0,
            texture: Arc::new(texture),
            coverage: [0.0, 0.0, 1.0, 1.0],
            translation: [0.0, 0.0],
            scale: [1.0, 1.0],
        }],
        floor_tiles: 0,
        identity: tile.0,
        withheld: false,
    }
}

/// A stream that hands the pump a scripted list of messages, and keeps what
/// the pump sent.
///
/// `Pending` once drained, which is exactly what a live server looks like
/// between arrivals, so the pump takes the same path it does in a session.
#[derive(Default)]
pub struct Scripted {
    pub inbox: VecDeque<ServerMessage>,
    pub sent: Mutex<Vec<ClientMessage>>,
}

impl Scripted {
    pub fn new(messages: impl IntoIterator<Item = ServerMessage>) -> Self {
        Self {
            inbox: messages.into_iter().collect(),
            sent: Mutex::default(),
        }
    }
}

impl GeometryStream for Scripted {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
        self.sent.lock().expect("sent").push(msg);
        Ok(())
    }

    fn poll_message(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        match self.inbox.pop_front() {
            Some(msg) => Poll::Ready(Some(msg)),
            None => Poll::Pending,
        }
    }
}

/// The terrain ancestry of a tile, as the server states it on the wire.
pub fn ancestry(tile: TileId) -> Ancestry {
    let (z, x, y) = tile.terrain_coord();
    Ancestry {
        level: z,
        parent: (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2)),
    }
}

/// The shape of the tree around some tiles: every tile named, **and every
/// ancestor of one**, up to the root.
pub fn tree_shape(tiles: &[TileId]) -> Vec<(TileId, Ancestry)> {
    let mut out = Vec::new();
    for tile in tiles {
        let mut cur = Some(*tile);
        while let Some(id) = cur {
            let a = ancestry(id);
            out.push((id, a));
            cur = a.parent;
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out.dedup_by_key(|(t, _)| *t);
    out
}

/// A terrain mesh delivered, at generation `gen`.
pub fn mesh(tile: TileId, gen: u64, content: DecodedTileContent) -> ServerMessage {
    ServerMessage::Content {
        resource: ResourceId::Mesh(tile),
        gen: Generation(gen),
        class: Class::TerrainMesh,
        ancestry: ancestry(tile),
        content: ResourceContent::Mesh(Arc::new(content)),
    }
}

/// A drape delivered, at generation `gen`.
pub fn drape(tile: TileId, gen: u64, content: DrapeContent) -> ServerMessage {
    ServerMessage::Content {
        resource: ResourceId::drape(tile, content.layer, content.level),
        gen: Generation(gen),
        class: Class::Drape,
        ancestry: ancestry(tile),
        content: ResourceContent::Drape(Arc::new(content)),
    }
}

/// A mesh drawn as itself.
pub fn exact(tile: TileId) -> DrawItem {
    let mut item = DrawItem::new(ResourceId::Mesh(tile), Role::Exact);
    item.serves.push(tile);
    item
}

/// Each of `tiles` drawn as itself.
pub fn exacts(tiles: &[TileId]) -> Vec<DrawItem> {
    tiles.iter().map(|t| exact(*t)).collect()
}

/// An ancestor standing in for `serves`, under the fallback pass.
pub fn fallback(tile: TileId, serves: &[TileId]) -> DrawItem {
    let mut item = DrawItem::new(ResourceId::Mesh(tile), Role::Fallback);
    item.serves.extend_from_slice(serves);
    item.serves.sort();
    item
}

/// `item` wearing `drape` (a resource of `owner`'s) through `uv`.
pub fn wearing(mut item: DrawItem, drape: ResourceId, level: u32, uv: UvTransform) -> DrawItem {
    let ResourceId::Drape { layer, .. } = drape else {
        unreachable!("a drape resource");
    };
    item.drapes.push(DrapeChoice {
        resource: drape,
        layer,
        level,
        uv,
    });
    item
}

/// The draw set the server sends, at generation `generation`: the items
/// sorted by mesh, and the shape of the tree around everything they name.
pub fn select(mut items: Vec<DrawItem>, generation: u64) -> ServerMessage {
    items.sort_by_key(|i| i.mesh);
    let mut named: Vec<TileId> = Vec::new();
    for item in &items {
        named.push(item.mesh.tile());
        named.extend(item.serves.iter().copied());
        named.extend(item.drapes.iter().map(|d| d.resource.tile()));
    }
    ServerMessage::Select {
        draw: DrawSet { items },
        plan: UploadPlan::default(),
        ancestry: tree_shape(&named),
        stats: TraversalStats::default(),
        hard_limit: PerClass::splat(Bytes::MAX),
        frame_ready: false,
        converged: false,
        lost: 0,
        bare: 0,
        generation,
    }
}

/// A server to drive by hand: polled on the test's thread, so a scenario is
/// a sequence, not a race.
pub type Server = Pin<Box<dyn Future<Output = ()>>>;

/// Polls the server once.
pub fn poll(server: &mut Server) {
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let _ = server.as_mut().poll(&mut cx);
}

/// Sends `views` and drives the server and the pump until the view has
/// converged: the server says so for the camera just sent, and nothing is
/// left to upload. Returns whether it did within the bound.
pub fn settle<S: GeometryStream>(
    server: &mut Server,
    stream: &mut S,
    pump: &mut ContentPump,
    gpu: &GpuContext,
    views: &[ViewState],
    origin: DVec3,
) -> bool {
    poll(server);
    pump.advance(stream, gpu, views, origin, FrameDeadline::UNBOUNDED);
    for _ in 0..4_000 {
        poll(server);
        pump.pump(stream, gpu, FrameDeadline::UNBOUNDED);
        if pump.generation_selected() >= pump.generation_sent()
            && (pump.converged || pump.frame_ready)
            && pump.pending_uploads() == 0
        {
            return true;
        }
    }
    false
}
