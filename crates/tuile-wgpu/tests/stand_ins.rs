// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A stand-in is a surface, and a surface stops the climb.
//!
//! When a selected tile has not arrived, the consumer draws the nearest ancestor
//! it holds. That ancestor spans **all** of its descendants, including the
//! siblings that did arrive — so two surfaces end up over the same ground, both
//! writing depth at nearly the same place, and the depth test picks a winner per
//! pixel. On the globe that reads as coarse imagery and sharp imagery
//! interleaved in blobs following the relief rather than the tile grid, worst
//! over islands where refinement is deep and uneven.
//!
//! The server's answer is to send each missing tile a stand-in over **its own
//! rectangle** ([`tuile_core::protocol::ServerMessage::Fill`]). The consumer
//! needs no rule for it: a stand-in is that tile's surface, the walk stops
//! there, and no ancestor is drawn. Nothing is refused — which is the difference
//! between this and two earlier attempts that withheld the fallback and left
//! large black rectangles.
//!
//! [`crate::pump`]'s own unit tests assert that invariant on the pure walk. What
//! is left, and what is here, is that a `Fill` message actually becomes a
//! drawn surface: the queueing, the upload and the bookkeeping between the
//! message arriving and `resolve` naming it.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

use std::collections::VecDeque;
use std::task::{Context, Poll};

use glam::{DVec3, Mat4};

use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc, TileContent};
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage, StreamError};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_wgpu::{ContentPump, GpuContext};

/// A GPU, or a stated reason there is none.
fn gpu() -> Option<GpuContext> {
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

/// A stream that hands the pump a scripted list of messages.
struct Scripted(VecDeque<ServerMessage>);

impl GeometryStream for Scripted {
    fn send(&self, _msg: ClientMessage) -> Result<(), StreamError> {
        Ok(())
    }
    fn poll_message(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        match self.0.pop_front() {
            Some(msg) => Poll::Ready(Some(msg)),
            None => Poll::Pending,
        }
    }
}

/// A single triangle, enough to be a surface and nothing more.
///
/// What is under test is which tiles `resolve` names, not what they look like:
/// the pump's bookkeeping is the same whether the mesh is three vertices or
/// thirty thousand.
fn surface() -> DecodedTileContent {
    DecodedTileContent {
        meshes: vec![DecodedMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            uvs: Some(vec![[0.0, 0.0]; 3]),
            indices: vec![0, 1, 2],
            material: MaterialDesc {
                base_color_factor: [1.0, 1.0, 1.0, 1.0],
                base_color_texture: None,
            },
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::ZERO,
        transform_local: Mat4::IDENTITY,
    }
}

fn parent_of(id: TileId) -> Option<TileId> {
    let (z, x, y) = id.terrain_coord();
    (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2))
}

fn here() -> TileId {
    TileId::from_terrain(3, 4, 4)
}
fn neighbour() -> TileId {
    TileId::from_terrain(3, 5, 4)
}
fn grandparent() -> TileId {
    TileId::from_terrain(1, 1, 1)
}

/// Runs the script and reports how the selection resolved.
///
/// `Resolution::coarser` is the number that matters and it is the renderer's
/// own: "drawn by a coarser ancestor while the chosen tile streams in". Each
/// one of those is an ancestor spanning ground its descendants already cover —
/// two surfaces over one patch, which is the z-fighting. `unresolved` names
/// them.
fn resolve(gpu: &GpuContext, script: Vec<ServerMessage>) -> (usize, tuile_wgpu::Resolution, Vec<TileId>) {
    let mut stream = Scripted(VecDeque::from(script));
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.pump(&mut stream, gpu, 16);
    let (drawn, counts, unresolved) = pump.resolve_reporting(&gpu.queue, parent_of);
    (drawn.exact.len() + drawn.fallback.len(), counts, unresolved)
}

/// The script up to the point where the neighbour is missing: both tiles
/// selected, this one and the grandparent resident.
fn missing_neighbour() -> Vec<ServerMessage> {
    vec![
        ServerMessage::Select {
            tiles: vec![(here(), 0.0), (neighbour(), 0.0)],
            stats: TraversalStats::default(),
        },
        ServerMessage::Content {
            tile: here(),
            content: TileContent::Decoded(surface()),
        },
        ServerMessage::Content {
            tile: grandparent(),
            content: TileContent::Decoded(surface()),
        },
    ]
}

/// **Without a stand-in, the neighbour's ancestor is drawn over this tile too.**
///
/// The state being fixed, asserted so the fix has something to be measured
/// against. The grandparent covers both selected tiles; only one of them has a
/// surface of its own; so that one's ground carries two surfaces.
#[test]
fn a_missing_tile_pulls_its_ancestor_over_its_neighbour() {
    let Some(gpu) = gpu() else { return };

    let (drawn, counts, unresolved) = resolve(&gpu, missing_neighbour());

    assert_eq!(counts.lost, 0, "nothing may be left uncovered");
    assert_eq!(
        counts.coarser, 1,
        "the tile that has not arrived must be covered by its ancestor"
    );
    assert_eq!(
        unresolved,
        vec![neighbour()],
        "and the ancestor drawn for it also spans its neighbour, which is the \
         two-surfaces-over-one-patch this exists to remove"
    );
    assert_eq!(drawn, 2, "this tile and the grandparent");
}

/// **A `Fill` for the missing tile leaves one surface per patch of ground.**
///
/// Same script, one message more. The stand-in is content like any other from
/// the pump's side, which is the design: `resolve` asks whether it holds a
/// surface for a tile, never whether that surface is the real one.
///
/// The grandparent is still resident and still eligible — nothing here refuses
/// to draw it. It is simply never reached, because the walk stops at the
/// neighbour's own surface. That distinction is the whole fix: two earlier
/// attempts withheld the fallback instead, and each left large black
/// rectangles.
#[test]
fn a_fill_for_the_missing_tile_keeps_the_ancestor_off_the_screen() {
    let Some(gpu) = gpu() else { return };

    let mut script = missing_neighbour();
    script.push(ServerMessage::Fill {
        tile: neighbour(),
        content: surface(),
    });
    let (drawn, counts, unresolved) = resolve(&gpu, script);

    assert_eq!(counts.lost, 0, "a stand-in may never become a hole");
    assert_eq!(
        counts.coarser, 0,
        "an ancestor was drawn even though every selected tile had a surface \
         of its own — that is two surfaces over one patch of ground, and it is \
         what z-fights. Still wanting their own surface: {unresolved:?}"
    );
    assert_eq!(counts.exact, 2, "both selected tiles drawn at their own level");
    assert_eq!(drawn, 2, "one surface per selected tile, no more");
}

/// **Real content replaces a stand-in without a frame in between.**
///
/// The rule `CLAUDE.md` states for every surface swap: create the new one,
/// activate it, and only then let the old one go. A stand-in that were
/// *removed* on the real tile's arrival — rather than replaced — would leave
/// that ground drawn by nothing for as long as the upload takes, which is
/// exactly the black the stand-in exists to prevent.
///
/// Asserted by asking after every message rather than only at the end: there
/// must be no point in the sequence where the tile has no surface.
#[test]
fn the_real_tile_replaces_its_stand_in_without_a_gap() {
    let Some(gpu) = gpu() else { return };

    let mut stream = Scripted(VecDeque::from(vec![
        ServerMessage::Select {
            tiles: vec![(neighbour(), 0.0)],
            stats: TraversalStats::default(),
        },
        ServerMessage::Fill {
            tile: neighbour(),
            content: surface(),
        },
    ]));
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.pump(&mut stream, &gpu, 16);
    assert!(
        pump.has(neighbour()),
        "the stand-in did not become a surface"
    );

    let mut arrival = Scripted(VecDeque::from(vec![ServerMessage::Content {
        tile: neighbour(),
        content: TileContent::Decoded(surface()),
    }]));
    pump.pump(&mut arrival, &gpu, 16);
    assert!(
        pump.has(neighbour()),
        "the ground lost its surface when the real tile arrived — a replacement \
         may overlap for a frame, it may never gap for one"
    );
}
