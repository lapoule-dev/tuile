// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **One pump for the scene: the surfaces that cover the ground apart.**
//!
//! A scene's one stream carries the globe and the 3D Tiles over it. The pump
//! resolves them into the passes the renderer must not mix: the ground's
//! (exact, fallback, the backdrop) and the surfaces drawn in place of the
//! ground (`Ancestry::covers`), which `TileRenderer::paint_replacing` draws
//! before it. The backdrop is the ground's alone; a 3D Tiles mesh keeps no
//! CPU copy (it is never stitched) and is acknowledged without one, as the
//! server's estimate charges it.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

mod support;

use std::sync::Arc;

use glam::{DVec3, Mat4};

use support::{ancestry, exact, fallback, gpu, mesh, Scripted};
use tuile_core::budget::{
    Bytes, Class, DrawItem, DrawSet, FrameDeadline, Generation, PerClass, ResourceId, UploadPlan,
};
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::protocol::{Ancestry, ClientMessage, ResourceContent, ServerMessage};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_wgpu::ContentPump;

fn surface() -> DecodedTileContent {
    DecodedTileContent {
        withheld_drape: None,
        copyright: None,
        meshes: vec![DecodedMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            uvs: Some(vec![[0.0, 0.0]; 3]),
            indices: vec![0, 1, 2],
            material: MaterialDesc {
                base_color_factor: [1.0, 1.0, 1.0, 1.0],
                base_color_texture: None,
                unlit: false,
            },
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::ZERO,
        transform_local: Mat4::IDENTITY,
    }
}

/// A 3D Tiles mesh of a covering source (tag 1), at `level` of its tree.
fn covering(tile: TileId, level: u32, parent: Option<TileId>, gen: u64) -> ServerMessage {
    ServerMessage::Content {
        resource: ResourceId::Mesh(tile),
        gen: Generation(gen),
        class: Class::Tiles3dMesh,
        ancestry: Ancestry {
            level,
            parent,
            covers: true,
        },
        content: ResourceContent::Mesh(Arc::new(surface())),
    }
}

/// The surface's tile `index` and its ancestry (`root` its parent, if any).
fn surface_ancestry(index: u64, level: u32) -> (TileId, Ancestry) {
    let tile = TileId(index).with_tag(1);
    let parent = (index > 0).then(|| TileId(0).with_tag(1));
    (
        tile,
        Ancestry {
            level,
            parent,
            covers: true,
        },
    )
}

/// The draw set over the ground tile `ground` and the surface's tiles, with
/// the shape of each tree as the server states it.
fn select(
    mut items: Vec<DrawItem>,
    ground: TileId,
    surface: &[(TileId, Ancestry)],
) -> ServerMessage {
    items.sort_by_key(|i| i.mesh);
    let mut shape = vec![(ground, ancestry(ground))];
    let mut up = ancestry(ground).parent;
    while let Some(p) = up {
        shape.push((p, ancestry(p)));
        up = ancestry(p).parent;
    }
    shape.extend_from_slice(surface);
    shape.sort_by_key(|(t, _)| *t);
    ServerMessage::Select {
        draw: DrawSet { items },
        plan: UploadPlan::default(),
        ancestry: shape,
        stats: TraversalStats::default(),
        hard_limit: PerClass::splat(Bytes::MAX),
        frame_ready: false,
        converged: false,
        lost: 0,
        bare: 0,
        generation: 1,
    }
}

/// The bytes each resource was acknowledged with.
fn acked(stream: &Scripted, r: ResourceId) -> Option<u64> {
    stream
        .sent
        .lock()
        .expect("sent")
        .iter()
        .find_map(|m| match m {
            ClientMessage::Ack {
                resource, bytes, ..
            } if *resource == r => Some(*bytes),
            _ => None,
        })
}

/// The ground and a covering surface in one draw set resolve into their own
/// passes; the backdrop takes the ground at its level and not the surface
/// at the same depth of its own tree. Red with every mesh in the ground's
/// passes (the surface drawn as ground, fighting it for depth), and with the
/// backdrop taking every mesh of its level.
#[test]
fn the_ground_and_the_surfaces_over_it_resolve_apart() {
    let Some(gpu) = gpu() else { return };
    let ground = TileId::from_terrain(2, 1, 1);
    let (root, child) = (TileId(0).with_tag(1), TileId(1).with_tag(1));
    let mut stream = Scripted::new([
        mesh(ground, 1, surface()),
        covering(root, 2, None, 2),
        covering(child, 3, Some(root), 3),
        select(
            vec![
                exact(ground),
                exact(child),
                fallback(root, &[TileId(2).with_tag(1)]),
            ],
            ground,
            &[
                surface_ancestry(0, 2),
                surface_ancestry(1, 3),
                surface_ancestry(2, 3),
            ],
        ),
    ]);
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.set_backdrop_level(Some(2));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let (drawn, counts) = pump.resolve(&gpu);
    assert_eq!(drawn.exact.len(), 1, "the ground drawn as ground");
    assert!(drawn.fallback.is_empty());
    assert_eq!(drawn.covering.len(), 1, "the surface drawn apart");
    assert_eq!(drawn.covering_fallback.len(), 1, "and its stand-in");
    assert_eq!(
        drawn.backdrop.len(),
        1,
        "the backdrop is the ground's: the surface's root at level 2 is not in it"
    );
    assert_eq!(counts.lost, 0);
}

/// A 3D Tiles mesh keeps no copy and is acknowledged without one — the
/// bytes the server's estimate charges it (`cost_held(false)`); the ground's
/// keeps it, acknowledged with it. Red with every mesh keeping its copy: the
/// surface acknowledged above its estimate (drift on every 3D Tiles tile).
#[test]
fn a_surface_is_acknowledged_without_a_copy_and_the_ground_with_one() {
    let Some(gpu) = gpu() else { return };
    let ground = TileId::from_terrain(2, 1, 1);
    let tile = TileId(0).with_tag(1);
    let mut stream = Scripted::new([
        mesh(ground, 1, surface()),
        covering(tile, 0, None, 2),
        select(
            vec![exact(ground), exact(tile)],
            ground,
            &[surface_ancestry(0, 0)],
        ),
    ]);
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let ground_ack = acked(&stream, ResourceId::Mesh(ground)).expect("the ground acknowledged");
    let tile_ack = acked(&stream, ResourceId::Mesh(tile)).expect("the surface acknowledged");
    let copy = 3 * tuile_core::content::VERTEX_STRIDE + 3 * 4;
    assert_eq!(
        ground_ack - tile_ack,
        copy,
        "the same mesh, with and without the copy a stitch reads"
    );
    // Each acknowledgement is the server's estimate, within its drift band.
    let estimate = |cpu_copy: bool| surface().cost_held(cpu_copy).gpu.0 as f64;
    for (ack, est) in [(ground_ack, estimate(true)), (tile_ack, estimate(false))] {
        assert!(
            (ack as f64 - est).abs() <= 0.05 * est,
            "acknowledged {ack} against an estimate of {est}"
        );
    }
}

/// A source hidden — a tileset switched off, before the server's next
/// selection has arrived — is not drawn from the next frame; the ground is.
/// Shown again, it is. Red with the hidden source drawn: the surface still
/// in the covering pass the frame it was switched off.
#[test]
fn a_hidden_source_is_not_drawn_from_the_next_frame() {
    let Some(gpu) = gpu() else { return };
    let ground = TileId::from_terrain(2, 1, 1);
    let tile = TileId(0).with_tag(1);
    let mut stream = Scripted::new([
        mesh(ground, 1, surface()),
        covering(tile, 0, None, 2),
        select(
            vec![exact(ground), exact(tile)],
            ground,
            &[surface_ancestry(0, 0)],
        ),
    ]);
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    assert_eq!(pump.resolve(&gpu).0.covering.len(), 1);
    pump.hide_source(1, true);
    let (drawn, counts) = pump.resolve(&gpu);
    assert!(drawn.covering.is_empty(), "the hidden source drawn");
    assert_eq!(drawn.exact.len(), 1, "the ground drawn");
    assert_eq!(counts.lost, 0);
    pump.hide_source(1, false);
    assert_eq!(pump.resolve(&gpu).0.covering.len(), 1);
}
