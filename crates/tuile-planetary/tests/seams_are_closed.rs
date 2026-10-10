// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **What the loader hands out is closed ground.**
//!
//! Every path that draws the globe — a live viewer, a bake, a host's own
//! renderer on the other side of the stream — draws the content this loader
//! returns, and none of them knows what a seam is. So the claim is made
//! here, on that content: between two tiles the loader built, whatever
//! their levels and wherever their terrain is from, no ray passes.
//!
//! The source is made up and serves bytes, as a real one does: a relief
//! measured by each terrain tile on its own grid, encoded as quantized
//! mesh. It has nothing finer than the tiles it is given, so everything
//! finer is cut by the loader from what it has.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::sync::Arc;

use support::{along, encoded, measured, meeting, tile_at, LAT, LON};
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_core::seam::{seam, Side};
use tuile_core::source::{Loaded, TileId, TileLoader};
use tuile_core::DecodedTileContent;
use tuile_planetary::{globe, GlobeOptions};
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};

struct Served(HashMap<TileCoord, Vec<u8>>);

#[async_trait::async_trait]
impl TerrainSource for Served {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        match self.0.get(&coord) {
            Some(bytes) => Ok(Fetched::undated(bytes.clone())),
            None => Err(TerrainSourceError::no_such_tile(format!("{coord:?}"))),
        }
    }
}

/// One small picture for every imagery tile: a stand-in is refused where
/// no picture is in memory, so there has to be one.
struct OnePicture;

/// A 4 × 4 PNG of one colour.
const PICTURE: [u8; 73] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 4, 8, 2, 0,
    0, 0, 38, 147, 9, 41, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 168, 104, 202, 131, 35, 6,
    226, 56, 0, 73, 242, 22, 129, 231, 235, 56, 46, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

#[async_trait::async_trait]
impl ImageryProvider for OnePicture {
    fn tiling_scheme(&self) -> TilingScheme {
        TilingScheme::web_mercator()
    }

    async fn fetch_tile_bytes(
        &self,
        _coord: ImageryCoord,
    ) -> Result<Fetched<bytes::Bytes>, RasterError> {
        Ok(Fetched::undated(bytes::Bytes::from_static(&PICTURE)))
    }
}

fn loader_over(tiles: &[(TileCoord, usize)]) -> Arc<dyn TileLoader> {
    let served = tiles
        .iter()
        .map(|&(tile, steps)| (tile, encoded(&measured(tile, steps))))
        .collect();
    // The source says what it has, and it has these tiles and nothing else:
    // everything under them is the loader's to cut.
    let deepest = tiles.iter().map(|t| t.0.level).max().unwrap_or(0);
    let available: Vec<String> = (0..=deepest)
        .map(|level| {
            let ranges: Vec<String> = tiles
                .iter()
                .filter(|t| t.0.level == level)
                .map(|(t, _)| {
                    format!(
                        r#"{{"startX":{0},"startY":{1},"endX":{0},"endY":{1}}}"#,
                        t.x, t.y
                    )
                })
                .collect();
            format!("[{}]", ranges.join(","))
        })
        .collect();
    let layer = LayerJson::from_slice(
        format!(
            r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
                "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
                "bounds":[-180,-90,180,90],"available":[{}]}}"#,
            available.join(",")
        )
        .as_bytes(),
    )
    .expect("layer.json");
    let (_tree, loader, _detail, _heights) =
        globe(Served(served), OnePicture, layer, GlobeOptions::default());
    loader
}

fn id(tile: TileCoord) -> TileId {
    TileId::from_terrain(tile.level, tile.x, tile.y)
}

fn loaded(loader: &Arc<dyn TileLoader>, tile: TileCoord) -> DecodedTileContent {
    match futures_executor::block_on(loader.load(id(tile))).expect("loads") {
        Loaded::Content(content) => content,
        Loaded::Expanded => panic!("terrain loads as content"),
    }
}

/// Rays cast between every pair that meets, and how many passed.
fn rays(
    west: &[(TileCoord, DecodedTileContent)],
    east: &[(TileCoord, DecodedTileContent)],
) -> (usize, usize, f64) {
    let (mut cast, mut through, mut step) = (0, 0, 0.0f64);
    for w in west {
        for e in east {
            if let Some((a, b)) = meeting((w.0, &w.1), (e.0, &e.1)) {
                let found = seam(a, b, 48);
                cast += found.rays;
                through += found.through;
                step = step.max(found.open);
            }
        }
    }
    (cast, through, step)
}

#[test]
fn loaded_tiles_cut_from_two_terrain_tiles_are_closed() {
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    let loader = loader_over(&[(a, 32), (b, 20)]);
    let west: Vec<_> = along(a, 16, Side::East)
        .into_iter()
        .map(|t| (t, loaded(&loader, t)))
        .collect();
    let east: Vec<_> = along(b, 14, Side::West)
        .into_iter()
        .map(|t| (t, loaded(&loader, t)))
        .collect();
    let (cast, through, step) = rays(&west, &east);
    assert!(cast > 500, "the scene has few steps to look into: {cast}");
    assert_eq!(
        through, 0,
        "{through} of {cast} rays pass between loaded tiles, through steps of up to {step:.2} m"
    );
}

#[test]
fn a_loaded_tile_is_closed_against_a_neighbour_several_levels_coarser() {
    for coarse_level in [11, 9, 7] {
        let coarse = tile_at(coarse_level, LON, LAT);
        let down = 13 - coarse_level;
        let fine = TileCoord::new(
            13,
            (coarse.x << down) - 1,
            (coarse.y << down) + (1 << (down - 1)),
        );
        let loader = loader_over(&[(fine, 32), (coarse, 16)]);
        let west: Vec<_> = along(fine, 17, Side::East)
            .into_iter()
            .map(|t| (t, loaded(&loader, t)))
            .collect();
        let east = vec![(coarse, loaded(&loader, coarse))];
        let (cast, through, step) = rays(&west, &east);
        assert!(
            cast > 500,
            "level {coarse_level}: few steps to look into: {cast}"
        );
        assert_eq!(
            through, 0,
            "level 17 against level {coarse_level}: {through} of {cast} rays pass, through \
             steps of up to {step:.1} m"
        );
    }
}

#[test]
fn a_stand_in_is_closed_as_the_tile_it_stands_for() {
    // Stand-ins of level 17 over level-13 terrain, against a loaded tile of
    // level 9: the steps of a coast, and a surface four levels above its
    // rectangle.
    let coarse = tile_at(9, LON, LAT);
    let fine = TileCoord::new(13, (coarse.x << 4) - 1, (coarse.y << 4) + 8);
    let loader = loader_over(&[(fine, 32), (coarse, 16)]);
    // What is in memory when a stand-in is asked for: the terrain tile
    // itself, and the pictures over it.
    let _ = loaded(&loader, fine);
    let west: Vec<_> = along(fine, 17, Side::East)
        .into_iter()
        .map(|t| {
            let stand_in = loader
                .fill(id(t))
                .unwrap_or_else(|| panic!("no stand-in for {t:?}"));
            (t, stand_in)
        })
        .collect();
    let east = vec![(coarse, loaded(&loader, coarse))];
    let (cast, through, step) = rays(&west, &east);
    assert!(cast > 500, "the scene has few steps to look into: {cast}");
    assert_eq!(
        through, 0,
        "{through} of {cast} rays pass between stand-ins and their neighbour, through steps of \
         up to {step:.1} m"
    );
}
