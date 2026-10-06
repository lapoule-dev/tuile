// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Checks a pack against itself: its tiles as it carries them, against the
//! same tiles built again from the tile store through its references.
//!
//! A pack baked with `--content both` holds each tile twice over — the
//! finished mesh and composed texture, and where to find what they were made
//! from. This builds every tile the second way and compares it with the
//! first: meshes byte for byte, textures texel for texel.
//!
//! ```bash
//! TUILE_STORE_BUCKET=<tile store bucket> cargo run --release -p tuile-pack-api \
//!     --example store_parity -- <both.tuilepack> [how many tiles]
//! ```
//!
//! The bucket's endpoint and key come from the environment, as the API takes
//! them (`TUILE_STORE_*`, a `.env` in the working directory read first).

use std::collections::HashMap;
use std::sync::Arc;

use tuile_core::content::DecodedTexture;
use tuile_core::raster::TilingScheme;
use tuile_farm::{BucketConfig, ObjectRunStore, Tuning};
use tuile_film::from_store::{compose, imagery_texture, is_baked, terrain_mesh};
use tuile_film::{refs_of, Mesh, Pack};
use tuile_repository::{ArchivedTiles, Objects, TileRepository};

#[derive(Default)]
struct Tally {
    tiles: usize,
    meshes_same: usize,
    meshes_differ: usize,
    textures_same: usize,
    textures_differ: usize,
    texels_apart: u64,
    worst: u8,
    renewed: usize,
    absent: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .first()
        .ok_or("usage: store_parity <both.tuilepack> [tiles]")?;
    let limit: usize = args
        .get(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(usize::MAX);

    let bytes = std::fs::read(path)?;
    let pack = Pack::open(&bytes)?;
    let (terrain_layer, imagery_layer) = pack
        .store_layers()
        .ok_or("this pack holds no reference to the tile store")?;

    let bucket: Arc<dyn Objects> = Arc::new(ObjectRunStore::bucket(
        &BucketConfig::from_env()?,
        Tuning::from_env(),
    )?);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let store = ArchivedTiles::open(bucket.clone(), bucket, Arc::new(move || now)).await?;
    let scheme = match store.layers().iter().find(|l| l.name == imagery_layer) {
        Some(layer) if layer.grid == "geographic" => TilingScheme::geographic(),
        _ => TilingScheme::web_mercator(),
    };

    let mut tally = Tally::default();
    let mut textures: HashMap<(u8, u32, u32), Arc<DecodedTexture>> = HashMap::new();
    let mut seen = std::collections::BTreeSet::new();
    let (first, last) = pack.frame_range();
    'frames: for frame in first..=last {
        for tile in pack.frame(frame)? {
            if !seen.insert((tile.id(), tile.drape())) {
                continue;
            }
            if tally.tiles >= limit {
                break 'frames;
            }
            let Some(refs) = refs_of(&tile) else { continue };
            tally.tiles += 1;
            let carried = Mesh::of(&pack, &tile)?;

            // The mesh, from the terrain tile it was built from.
            let source = refs.terrain;
            let Some(terrain) = store
                .tile(terrain_layer, source.level, source.x, source.y)
                .await?
            else {
                tally.absent += 1;
                continue;
            };
            if !is_baked(&source, &terrain.bytes) {
                tally.renewed += 1;
            }
            let built = terrain_mesh(tile.id(), &refs, &terrain.bytes, carried.base_color_factor)?;
            if built == carried {
                tally.meshes_same += 1;
            } else {
                tally.meshes_differ += 1;
                if tally.meshes_differ <= 3 {
                    println!(
                        "  tile {}: mesh differs — positions {} vs {} bytes, indices {} vs {}, origin {}",
                        tile.id(),
                        built.positions.len(),
                        carried.positions.len(),
                        built.indices.len(),
                        carried.indices.len(),
                        if built.origin_ecef == carried.origin_ecef { "same" } else { "differs" },
                    );
                }
            }

            // The drape, from its imagery tiles.
            let mut missing = false;
            for placed in &refs.imagery {
                let key = (placed.tile.level, placed.tile.x, placed.tile.y);
                if textures.contains_key(&key) {
                    continue;
                }
                let Some(imagery) = store
                    .tile(
                        imagery_layer,
                        placed.tile.level,
                        placed.tile.x,
                        placed.tile.y,
                    )
                    .await?
                else {
                    missing = true;
                    break;
                };
                if !is_baked(&placed.tile, &imagery.bytes) {
                    tally.renewed += 1;
                }
                textures.insert(
                    key,
                    Arc::new(imagery_texture(&placed.tile, &scheme, &imagery.bytes)?),
                );
            }
            if missing {
                tally.absent += 1;
                continue;
            }
            let composed = compose(&refs, carried.base_color_factor, |t| {
                textures[&(t.level, t.x, t.y)].clone()
            });
            let carried_png = tuile_film::texture(&pack, &tile)?;
            match (composed, carried_png) {
                (None, None) => tally.textures_same += 1,
                (Some(composed), Some(png)) => {
                    let carried = image::load_from_memory(&png)?.to_rgba8();
                    let same_size =
                        (carried.width(), carried.height()) == (composed.width, composed.height);
                    let (apart, worst) = if same_size {
                        carried.as_raw().iter().zip(&composed.rgba8).fold(
                            (0u64, 0u8),
                            |(apart, worst), (a, b)| {
                                let d = a.abs_diff(*b);
                                (apart + u64::from(d > 0), worst.max(d))
                            },
                        )
                    } else {
                        (u64::MAX, u8::MAX)
                    };
                    if apart == 0 {
                        tally.textures_same += 1;
                    } else {
                        tally.textures_differ += 1;
                        tally.worst = tally.worst.max(worst);
                        if same_size {
                            tally.texels_apart += apart;
                        }
                        if tally.textures_differ <= 3 {
                            println!(
                                "  tile {}: texture differs — {}x{} against {}x{}, {apart} values apart, worst {worst}",
                                tile.id(),
                                composed.width,
                                composed.height,
                                carried.width(),
                                carried.height()
                            );
                        }
                    }
                }
                _ => {
                    tally.textures_differ += 1;
                    println!(
                        "  tile {}: one side has a texture and the other none",
                        tile.id()
                    );
                }
            }
        }
    }
    println!(
        "{} tiles: meshes {} identical, {} different; textures {} identical, {} different \
         ({} values apart, worst {}); {} source tiles renewed since the bake, {} absent from the store",
        tally.tiles,
        tally.meshes_same,
        tally.meshes_differ,
        tally.textures_same,
        tally.textures_differ,
        tally.texels_apart,
        tally.worst,
        tally.renewed,
        tally.absent
    );
    Ok(())
}
