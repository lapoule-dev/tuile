// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The backdrop is drawn whatever its tiles wear.**
//!
//! The complete coarse level behind everything is what a camera that has
//! just turned sees where the draw set has not caught up yet — on a page,
//! the draw set arrives a message later than the camera. It used to be drawn
//! only for the floor tiles holding a drape of exactly the backdrop's level;
//! the budget asks a floor tile's drape at the level the view needs, so a
//! floor tile seen closer wore a finer one, was left out, and the turn
//! showed the sky through the planet. A backdrop tile wears the finest drape
//! of its own it holds, or none: soft or bare ground, never none.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

mod support;

use glam::{DVec3, Mat4};

use support::{drape, gpu, mesh, select, Scripted};
use tuile_core::budget::{FrameDeadline, LayerId};
use tuile_core::content::{DecodedMesh, DecodedTileContent, DrapeContent, MaterialDesc};
use tuile_core::source::TileId;
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
            },
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::ZERO,
        transform_local: Mat4::IDENTITY,
    }
}

fn drape_at(level: u32) -> DrapeContent {
    DrapeContent {
        layer: LayerId(0),
        level,
        tiles: Vec::new(),
        floor_tiles: 0,
        identity: u64::from(level),
        withheld: false,
    }
}

#[test]
fn a_backdrop_tile_is_drawn_whatever_drape_it_holds() {
    let Some(gpu) = gpu() else { return };
    // Three floor tiles of level 2: one dressed at the backdrop's level, one
    // at a finer level, one not dressed at all. Nothing is in the draw set.
    let (at_level, finer, bare) = (
        TileId::from_terrain(2, 1, 1),
        TileId::from_terrain(2, 2, 1),
        TileId::from_terrain(2, 3, 1),
    );
    let mut stream = Scripted::new([
        mesh(at_level, 1, surface()),
        mesh(finer, 2, surface()),
        mesh(bare, 3, surface()),
        drape(at_level, 4, drape_at(2)),
        drape(finer, 5, drape_at(4)),
        select(Vec::new(), 1),
    ]);
    let mut pump = ContentPump::new(DVec3::ZERO);
    pump.set_backdrop_level(Some(2));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let (drawn, _) = pump.resolve(&gpu);
    assert_eq!(
        drawn.backdrop.len(),
        3,
        "every floor tile on the GPU is in the backdrop, whatever it wears"
    );
}
