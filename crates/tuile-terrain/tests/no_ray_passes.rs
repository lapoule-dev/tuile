// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **No ray passes between two tiles of terrain**, whatever their levels and
//! wherever their surfaces are from.
//!
//! The seams a film showed — dark hairs along straight lines of the terrain
//! grid, the far side of the planet seen through the ground — were tiles cut
//! from an ancestor's terrain, drawn with no skirt. A cut tile agrees with
//! every other tile cut from the same terrain tile, and with nothing else:
//! across a line of the source's own grid the two sides are two surfaces,
//! each interpolating the relief between its own vertices.
//!
//! The scenes here are that, made up: one relief, two terrain tiles that
//! measure it on different grids, and the tiles a camera would draw cut from
//! them. The judge is `tuile_core::seam`, which casts rays through every
//! step along the shared line.

mod support;

use support::{along, measured, meeting, tile_at, LAT, LON};
use tuile_core::seam::{seam, Seam, Side};
use tuile_core::DecodedTileContent;
use tuile_terrain::{
    skirt_height, to_decoded, to_ground, upsample, GeographicTilingScheme, QuantizedMesh, TileCoord,
};

/// The mesh of `target`, cut from the terrain tile `from` a level at a time,
/// as a loader cuts it.
fn cut(source: &QuantizedMesh, from: TileCoord, target: TileCoord) -> QuantizedMesh {
    let mut mesh = source.clone();
    let mut at = from;
    while at.level < target.level {
        let shift = target.level - at.level - 1;
        let child = TileCoord::new(at.level + 1, target.x >> shift, target.y >> shift);
        mesh = upsample(&mesh, at, child).expect("the ancestor covers its descendant");
        at = child;
    }
    mesh
}

fn rect_of(tile: TileCoord) -> tuile_terrain::GeoRect {
    GeographicTilingScheme::default().tile_rect(tile)
}

/// Every pair of tiles that meet across the meridian, judged; the worst of
/// them is what is returned.
fn worst(
    west: &[(TileCoord, DecodedTileContent)],
    east: &[(TileCoord, DecodedTileContent)],
) -> Seam {
    let mut worst = Seam {
        length: 0.0,
        step: 0.0,
        open: 0.0,
        rays: 0,
        through: 0,
    };
    for w in west {
        for e in east {
            let Some((a, b)) = meeting((w.0, &w.1), (e.0, &e.1)) else {
                continue;
            };
            let found = seam(a, b, 48);
            worst.length += found.length;
            worst.step = worst.step.max(found.step);
            worst.open = worst.open.max(found.open);
            worst.rays += found.rays;
            worst.through += found.through;
        }
    }
    worst
}

/// Two terrain tiles of level 13 side by side, the western one measured on
/// 32 spacings and the eastern on 20.
fn two_sources() -> ((TileCoord, QuantizedMesh), (TileCoord, QuantizedMesh)) {
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    ((a, measured(a, 32)), (b, measured(b, 20)))
}

fn drawn(
    source: &(TileCoord, QuantizedMesh),
    tiles: &[TileCoord],
    ground: impl Fn(&QuantizedMesh, &tuile_terrain::GeoRect) -> DecodedTileContent,
) -> Vec<(TileCoord, DecodedTileContent)> {
    tiles
        .iter()
        .map(|&tile| {
            let mesh = cut(&source.1, source.0, tile);
            (tile, ground(&mesh, &rect_of(tile)))
        })
        .collect()
}

#[test]
fn tiles_cut_from_two_terrain_tiles_are_closed_where_they_meet() {
    let (a, b) = two_sources();
    // What a camera near the line draws: level 16 on one side, level 14 on
    // the other, neither from terrain of its own level.
    let (west, east) = (along(a.0, 16, Side::East), along(b.0, 14, Side::West));

    // The scene has steps to close, and with no wall they are open: or it
    // proves nothing.
    let bare = |mesh: &QuantizedMesh, rect: &tuile_terrain::GeoRect| to_decoded(mesh, rect, 0.0);
    let open = worst(&drawn(&a, &west, bare), &drawn(&b, &east, bare));
    assert!(open.step > 3.0, "the two surfaces barely part: {open:?}");
    assert!(
        open.through * 2 > open.rays,
        "with no wall, most rays must pass: {open:?}"
    );

    let closed = worst(&drawn(&a, &west, to_ground), &drawn(&b, &east, to_ground));
    assert_eq!(closed.rays, open.rays, "the same steps, looked into again");
    assert_eq!(
        closed.through, 0,
        "{} of {} rays pass between tiles cut from two terrain tiles, through steps of up to \
         {:.2} m",
        closed.through, closed.rays, closed.open
    );
}

#[test]
fn a_cut_tile_and_a_tile_drawn_as_itself_are_closed_where_they_meet() {
    let (a, b) = two_sources();
    let west = drawn(&a, &along(a.0, 15, Side::East), to_ground);
    let east = drawn(&b, &[b.0], to_ground);
    let found = worst(&west, &east);
    assert!(found.rays > 0);
    assert_eq!(found.through, 0, "{found:?}");
}

#[test]
fn tiles_cut_from_one_terrain_tile_state_one_line() {
    // Levels 14, 15 and 16 against one another, all of one surface: no step
    // at all, which is why a wall between them is never seen.
    let (a, _) = two_sources();
    let [sw, se, ..] = a.0.children();
    let west = drawn(&a, &along(sw, 16, Side::East), to_ground);
    let east = drawn(&a, &along(se, 14, Side::West), to_ground);
    let found = worst(&west, &east);
    assert!(found.length > 1000.0, "{found:?}");
    assert!(found.step < 1.0e-3, "{found:?}");
    assert!(found.is_closed());
}

/// A coast, an island, the edge of a source's coverage: fine tiles against a
/// tile many levels coarser, which states the line from vertices kilometres
/// apart.
fn across_levels(coarse_level: u32) -> ((TileCoord, QuantizedMesh), (TileCoord, QuantizedMesh)) {
    let coarse = tile_at(coarse_level, LON, LAT);
    let down = 13 - coarse_level;
    // The level-13 terrain tile just west of the coarse tile's west side,
    // half way up it.
    let fine = TileCoord::new(
        13,
        (coarse.x << down) - 1,
        (coarse.y << down) + (1 << (down - 1)),
    );
    ((fine, measured(fine, 32)), (coarse, measured(coarse, 16)))
}

#[test]
fn a_tile_several_levels_finer_than_its_neighbour_is_closed_against_it() {
    for coarse_level in [11, 9, 7] {
        let (fine, coarse) = across_levels(coarse_level);
        let tiles = along(fine.0, 17, Side::East);
        let east = drawn(&coarse, &[coarse.0], to_ground);

        // By the rule a tile is given beside neighbours a level away — five
        // of its own geometric errors — these are open: the fixture bites.
        let by_the_rule = |mesh: &QuantizedMesh, rect: &tuile_terrain::GeoRect| {
            to_decoded(mesh, rect, skirt_height(rect))
        };
        let open = worst(&drawn(&fine, &tiles, by_the_rule), &east);
        assert!(
            open.through > 0,
            "level 17 against level {coarse_level}: steps of {:.1} m are closed by a skirt of \
             {:.1} m, so this proves nothing",
            open.step,
            skirt_height(&rect_of(tiles[0]))
        );

        let found = worst(&drawn(&fine, &tiles, to_ground), &east);
        assert_eq!(
            found.through, 0,
            "level 17 cut from level 13, against level {coarse_level}: {} of {} rays pass, \
             through steps of up to {:.1} m",
            found.through, found.rays, found.open
        );
    }
}
