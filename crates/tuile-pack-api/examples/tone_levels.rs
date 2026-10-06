// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Works out one gain per imagery level for a scene, from the tile store.
//!
//! The scene is the packs given: every imagery tile they refer to is read
//! from the store, each is seen against the nearest ancestors that are also
//! referred to, over the same ground, and the observations are solved for
//! one gain a level (`tuile_radiometry::LevelGains`). The result is printed,
//! with what it leaves between each pair of levels, and written as the JSON
//! a renderer reads from the store, beside the layer: `<layer>/tone.json`.
//!
//! ```bash
//! TUILE_STORE_BUCKET=<tile store bucket> cargo run --release -p tuile-pack-api \
//!     --example tone_levels -- <out.json> <anchor level> <pack>...
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use tuile_farm::{BucketConfig, ObjectRunStore, Tuning};
use tuile_film::{refs_of, Pack};
use tuile_radiometry::{tone_of, LevelGains, LevelParams, Observation};
use tuile_repository::{ArchivedTiles, Objects, TileRepository};

/// How many levels up a tile looks for an ancestor: past this the ancestor
/// shows the tile's ground in too few texels to say what tone it is.
const REACH: u8 = 5;

type Coord = (u8, u32, u32);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, anchor, packs @ ..] = args.as_slice() else {
        return Err("usage: tone_levels <out.json> <anchor level> <pack>...".into());
    };
    let anchor: u8 = anchor.parse()?;

    // Every imagery tile the scene refers to, and the layer they are in.
    let mut wanted: BTreeSet<Coord> = BTreeSet::new();
    let mut layer = String::new();
    for path in packs {
        let bytes = std::fs::read(path)?;
        let pack = Pack::open(&bytes)?;
        let (_, imagery) = pack
            .store_layers()
            .ok_or("a pack with no reference to the store")?;
        layer = imagery.to_string();
        let (first, last) = pack.frame_range();
        for frame in first..=last {
            for tile in pack.frame(frame)? {
                for placed in refs_of(&tile).map(|r| r.imagery).unwrap_or_default() {
                    wanted.insert((placed.tile.level, placed.tile.x, placed.tile.y));
                }
            }
        }
    }
    let by_level = wanted
        .iter()
        .fold(BTreeMap::<u8, usize>::new(), |mut m, c| {
            *m.entry(c.0).or_default() += 1;
            m
        });
    println!(
        "layer {layer}: {} imagery tiles, by level {by_level:?}",
        wanted.len()
    );

    let bucket: Arc<dyn Objects> = Arc::new(ObjectRunStore::bucket(
        &BucketConfig::from_env()?,
        Tuning::from_env(),
    )?);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let store = ArchivedTiles::open(bucket.clone(), bucket, Arc::new(move || now)).await?;

    // Decoded as stored: a tile and its ancestors share a projection, so
    // the ground a tile covers is an exact rectangle of each ancestor.
    let mut images: HashMap<Coord, (Vec<u8>, u32, u32)> = HashMap::new();
    let mut absent = 0usize;
    for (n, coord) in wanted.iter().enumerate() {
        match store.tile(&layer, coord.0, coord.1, coord.2).await? {
            Some(tile) => {
                let decoded = image::load_from_memory(&tile.bytes)?.to_rgba8();
                let (w, h) = (decoded.width(), decoded.height());
                images.insert(*coord, (decoded.into_raw(), w, h));
            }
            None => absent += 1,
        }
        if n % 500 == 499 {
            eprintln!("  {} of {} tiles read", n + 1, wanted.len());
        }
    }
    if absent > 0 {
        println!("{absent} tiles are no longer in the store");
    }

    let mut observations = Vec::new();
    for (coord, (rgba, w, h)) in &images {
        let (tone, counted) = tone_of(rgba, *w, *h, (0.0, 0.0, 1.0, 1.0));
        if counted < 0.5 {
            continue;
        }
        // The two nearest ancestors in hand.
        let mut found = 0;
        for up in 1..=REACH.min(coord.0) {
            let ancestor = (coord.0 - up, coord.1 >> up, coord.2 >> up);
            let Some((a_rgba, aw, ah)) = images.get(&ancestor) else {
                continue;
            };
            let span = (1u32 << up) as f32;
            let (x, y) = (
                (coord.1 & ((1 << up) - 1)) as f32 / span,
                (coord.2 & ((1 << up) - 1)) as f32 / span,
            );
            let (under, a_counted) =
                tone_of(a_rgba, *aw, *ah, (x, y, x + 1.0 / span, y + 1.0 / span));
            if a_counted < 0.5 || under.iter().any(|c| *c <= 0.0) || tone.iter().any(|c| *c <= 0.0)
            {
                continue;
            }
            let mut gain = [0.0f32; 3];
            for c in 0..3 {
                gain[c] = (under[c] / tone[c]).log2();
            }
            // An ancestor far up shows this ground in a handful of texels.
            let texels = *aw as f32 / span;
            observations.push(Observation {
                level: coord.0,
                ancestor: ancestor.0,
                gain,
                weight: counted.min(a_counted) * (texels / 32.0).min(1.0),
            });
            found += 1;
            if found == 2 {
                break;
            }
        }
    }
    let solved = LevelGains::solve(
        &observations,
        &LevelParams {
            anchor,
            ..Default::default()
        },
    );

    println!(
        "{} observations; anchor level {}",
        observations.len(),
        solved.anchor
    );
    println!("gain per level, in stops (R G B):");
    for (level, g) in &solved.gains {
        println!("  level {level:>2}: {:+.3} {:+.3} {:+.3}", g[0], g[1], g[2]);
    }
    // A gain that brightens can push texels past white, where they are
    // all one colour: how much of each level that would be.
    let mut clipped: BTreeMap<u8, (u64, u64)> = BTreeMap::new();
    for (coord, (rgba, _, _)) in &images {
        let gain = solved.of(coord.0).map(|g| 2f32.powf(g));
        if gain.iter().all(|g| *g <= 1.0) {
            continue;
        }
        let entry = clipped.entry(coord.0).or_default();
        for texel in rgba.chunks_exact(4).step_by(7) {
            entry.1 += 1;
            let over = (0..3).any(|c| {
                let v = f32::from(texel[c]) / 255.0;
                let lit = if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                };
                lit * gain[c] > 1.0
            });
            entry.0 += u64::from(over);
        }
    }
    for (level, (over, of)) in &clipped {
        println!(
            "  level {level:>2}: {:.2} % of texels pushed past white",
            *over as f64 * 100.0 / (*of).max(1) as f64
        );
    }
    println!("between levels — observed gain, spread among tiles, left after correction:");
    for p in &solved.pairs {
        println!(
            "  {:>2} → {:>2}: {:>4} tiles, {:+.2} {:+.2} {:+.2}, spread {:.2}, left {:+.2} {:+.2} {:+.2}",
            p.level,
            p.ancestor,
            p.observations,
            p.gain[0],
            p.gain[1],
            p.gain[2],
            p.spread,
            p.residual[0],
            p.residual[1],
            p.residual[2]
        );
    }
    std::fs::write(out, solved.to_json())?;
    println!("written {out} — for the store, as {layer}/tone.json");
    Ok(())
}
