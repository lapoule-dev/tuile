// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A fallback is drawn under the surfaces that own their ground, and gives
//! way to the real tile without a frame in between.
//!
//! When a wanted tile has not arrived, the budget draws the nearest ancestor
//! the GPU holds in its place (`Role::Fallback`). That ancestor spans **all**
//! of its descendants, including the siblings that did arrive — two surfaces
//! over the same ground. The pump draws the ancestor in its own pass, after
//! the surfaces that own their ground, testing depth and never writing it:
//! it fills only what nothing else covers. Nothing is refused — which is the
//! difference between this and two earlier attempts that withheld the
//! fallback and left large black rectangles.
//!
//! What is here is the pump's side: that a draw set's roles become the
//! passes `resolve` names, and that a swap from the fallback to the real tile
//! leaves no frame where the ground has no surface.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

mod support;

use glam::{DVec3, Mat4};

use support::{exact, fallback, gpu, mesh, select, Scripted};
use tuile_core::budget::FrameDeadline;
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::source::TileId;
use tuile_wgpu::ContentPump;

/// A single triangle, enough to be a surface and nothing more.
///
/// What is under test is which tiles `resolve` names, not what they look like:
/// the pump's bookkeeping is the same whether the mesh is three vertices or
/// thirty thousand.
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

fn here() -> TileId {
    TileId::from_terrain(3, 4, 4)
}
fn neighbour() -> TileId {
    TileId::from_terrain(3, 5, 4)
}
fn grandparent() -> TileId {
    TileId::from_terrain(1, 1, 1)
}

/// **A missing tile is drawn by its ancestor, in the fallback pass.** The
/// sibling that arrived is drawn as itself; the ancestor stands in for the
/// one that did not, and is never drawn as a surface owning its ground.
#[test]
fn a_missing_tile_is_drawn_by_its_ancestor_under_the_fallback_pass() {
    let Some(gpu) = gpu() else { return };
    let mut stream = Scripted::new([
        mesh(here(), 1, surface()),
        mesh(grandparent(), 2, surface()),
        select(
            vec![exact(here()), fallback(grandparent(), &[neighbour()])],
            1,
        ),
    ]);
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let (drawn, counts) = pump.resolve(&gpu);

    assert_eq!(counts.lost, 0, "nothing may be left uncovered");
    assert_eq!(counts.exact, 1, "the tile that arrived, as itself");
    assert_eq!(counts.coarser, 1, "the one that did not, by its ancestor");
    assert_eq!(drawn.exact.len(), 1);
    assert_eq!(
        drawn.fallback.len(),
        1,
        "the ancestor is drawn — under the surfaces that own their ground, \
         never refused"
    );
}

/// **The real tile replaces its fallback without a frame in between.**
///
/// The rule `CLAUDE.md` states for every surface swap: create the new one,
/// activate it, and only then let the old one go. The ancestor stays drawn
/// until the draw set that names the real tile arrives, and that draw set
/// arrives only once the real tile is on the GPU; the ancestor's eviction
/// comes after it.
///
/// Asserted after every frame rather than only at the end: there must be no
/// point in the sequence where the neighbour's ground has no surface.
#[test]
fn the_real_tile_replaces_its_fallback_without_a_gap() {
    let Some(gpu) = gpu() else { return };
    let mut pump = ContentPump::new(DVec3::ZERO);
    let covered = |pump: &mut ContentPump| {
        let (drawn, counts) = pump.resolve(&gpu);
        counts.lost == 0 && drawn.exact.len() + drawn.fallback.len() == 2
    };

    let mut first = Scripted::new([
        mesh(here(), 1, surface()),
        mesh(grandparent(), 2, surface()),
        select(
            vec![exact(here()), fallback(grandparent(), &[neighbour()])],
            1,
        ),
    ]);
    pump.pump(&mut first, &gpu, FrameDeadline::UNBOUNDED);
    assert!(covered(&mut pump), "the fallback covers the neighbour");

    // The real tile arrives; the draw set has not changed yet.
    let mut arrival = Scripted::new([mesh(neighbour(), 3, surface())]);
    pump.pump(&mut arrival, &gpu, FrameDeadline::UNBOUNDED);
    assert!(
        covered(&mut pump),
        "the ground lost its surface when the real tile arrived"
    );

    // The draw set names it, and the ancestor goes after it.
    let mut swap = Scripted::new([
        select(vec![exact(here()), exact(neighbour())], 1),
        tuile_core::protocol::ServerMessage::Evict {
            resources: vec![tuile_core::budget::ResourceId::Mesh(grandparent())],
        },
    ]);
    pump.pump(&mut swap, &gpu, FrameDeadline::UNBOUNDED);
    let (drawn, counts) = pump.resolve(&gpu);
    assert_eq!(counts.lost, 0);
    assert_eq!(counts.exact, 2, "both as themselves");
    assert!(drawn.fallback.is_empty(), "no ancestor left over them");
    assert!(pump.has(neighbour()));
}
