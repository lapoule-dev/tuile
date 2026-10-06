// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Measures, on a tile store's own tiles, how far the levels of an imagery
//! layer disagree in tone, and what the harmonisation makes of it.
//!
//! For a square of tiles at one level, each tile is compared with its parent
//! over the same ground: how far its tone is from the parent's, block by
//! block, before and after the field computed from the pair is applied; and
//! each tile with its neighbours across their shared edge.
//!
//! ```bash
//! TUILE_STORE_BUCKET=<tile store bucket> cargo run --release -p tuile-pack-api \
//!     --example radiometry_bench -- <layer> <level> <x> <y> <tiles a side>
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tuile_farm::{BucketConfig, ObjectRunStore, Tuning};
use tuile_radiometry::{apply, linear_of, transfer, BlockStats, GainField, Params, BLOCKS};
use tuile_repository::{ArchivedTiles, Objects, TileRepository};

struct Image {
    rgba: Vec<u8>,
    side: u32,
}

fn median(mut values: Vec<f32>) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

fn percentile(mut values: Vec<f32>, p: f32) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    values.sort_by(f32::total_cmp);
    values[((values.len() - 1) as f32 * p) as usize]
}

/// The gaps between a child's tone and its parent's, in stops: one per block
/// and channel where both have something to say.
fn gaps(child: &BlockStats, parent: &BlockStats) -> Vec<f32> {
    let mut out = Vec::new();
    for b in 0..BLOCKS * BLOCKS {
        if child.valid[b] < 0.5 || parent.valid[b] < 0.5 {
            continue;
        }
        for c in 0..3 {
            if child.mean[b][c] > 0.0 && parent.mean[b][c] > 0.0 {
                out.push((parent.mean[b][c] / child.mean[b][c]).log2().abs());
            }
        }
    }
    out
}

/// The step in tone across the edge two tiles share: the mean of a strip of
/// four texels on each side, in stops, worst channel.
fn edge_step(a: &Image, b: &Image, horizontal: bool) -> f32 {
    let strip = |image: &Image, far: bool| {
        let (mut sum, mut n) = ([0.0f64; 3], 0u32);
        for along in 0..image.side {
            for across in 0..4 {
                let across = if far { image.side - 1 - across } else { across };
                let (x, y) = if horizontal { (across, along) } else { (along, across) };
                let i = ((y * image.side + x) * 4) as usize;
                for c in 0..3 {
                    sum[c] += f64::from(linear_of(image.rgba[i + c]));
                }
                n += 1;
            }
        }
        sum.map(|s| (s / f64::from(n.max(1))) as f32)
    };
    let (left, right) = (strip(a, true), strip(b, false));
    (0..3)
        .map(|c| (right[c].max(1e-4) / left[c].max(1e-4)).log2().abs())
        .fold(0.0, f32::max)
}

/// The fine texture of an image: how far texels stand from their block's
/// mean, relative to it — so that a gain alone is not counted as detail.
fn detail(image: &Image) -> f64 {
    let stats = BlockStats::of(&image.rgba, image.side, image.side);
    let mut energy = 0.0f64;
    for y in 0..image.side {
        for x in 0..image.side {
            let block = (y as usize * BLOCKS / image.side as usize) * BLOCKS
                + x as usize * BLOCKS / image.side as usize;
            let mean = stats.mean[block][1];
            if mean > 0.0 {
                let g = linear_of(image.rgba[((y * image.side + x) * 4 + 1) as usize]);
                energy += f64::from((g / mean - 1.0).powi(2));
            }
        }
    }
    energy
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [layer, level, x0, y0, n] = args.as_slice() else {
        return Err("usage: radiometry_bench <layer> <level> <x> <y> <tiles a side>".into());
    };
    let (level, x0, y0, n): (u8, u32, u32, u32) = (level.parse()?, x0.parse()?, y0.parse()?, n.parse()?);
    let bucket: Arc<dyn Objects> = Arc::new(ObjectRunStore::bucket(
        &BucketConfig::from_env()?,
        Tuning::from_env(),
    )?);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let store = ArchivedTiles::open(bucket.clone(), bucket, Arc::new(move || now)).await?;
    let params = Params::default();

    let mut images: HashMap<(u8, u32, u32), Option<Image>> = HashMap::new();
    let mut fetch = async |z: u8, x: u32, y: u32| -> Result<bool, Box<dyn std::error::Error>> {
        if !images.contains_key(&(z, x, y)) {
            let image = match store.tile(layer, z, x, y).await? {
                Some(tile) => {
                    let decoded = image::load_from_memory(&tile.bytes)?.to_rgba8();
                    Some(Image {
                        side: decoded.width(),
                        rgba: decoded.into_raw(),
                    })
                }
                None => None,
            };
            images.insert((z, x, y), image);
        }
        Ok(images[&(z, x, y)].is_some())
    };
    for y in y0..y0 + n {
        for x in x0..x0 + n {
            if fetch(level, x, y).await? {
                fetch(level - 1, x / 2, y / 2).await?;
            }
        }
    }

    let (mut before, mut after, mut overall, mut agreement) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut pairs, mut same, mut kept) = (0u32, 0u32, Vec::new());
    let mut corrected: HashMap<(u32, u32), Image> = HashMap::new();
    for y in y0..y0 + n {
        for x in x0..x0 + n {
            let (Some(Some(child)), Some(Some(parent))) =
                (images.get(&(level, x, y)), images.get(&(level - 1, x / 2, y / 2)))
            else {
                continue;
            };
            pairs += 1;
            let quadrant = (x % 2, y % 2);
            let parent_tone = BlockStats::of_quadrant(&parent.rgba, parent.side, parent.side, quadrant);
            let child_tone = BlockStats::of(&child.rgba, child.side, child.side);
            let found = transfer(&child_tone, &parent_tone, &params);
            same += u32::from(found.same_source);
            agreement.push(found.agreement);
            overall.push(found.overall.iter().fold(0.0f32, |m, g| m.max(g.abs())));
            before.extend(gaps(&child_tone, &parent_tone));

            let field = GainField::toward_parent(&found, &params);
            let mut fixed = Image {
                rgba: child.rgba.clone(),
                side: child.side,
            };
            apply(&mut fixed.rgba, fixed.side, fixed.side, &field, 1.0);
            after.extend(gaps(
                &BlockStats::of(&fixed.rgba, fixed.side, fixed.side),
                &parent_tone,
            ));
            let was = detail(child);
            if was > 0.0 {
                kept.push((detail(&fixed) / was) as f32);
            }
            corrected.insert((x, y), fixed);
        }
    }

    // Edges between neighbours of the level, as stored and once corrected.
    let (mut seams_before, mut seams_after) = (Vec::new(), Vec::new());
    for y in y0..y0 + n {
        for x in x0..x0 + n {
            for (dx, dy, horizontal) in [(1u32, 0u32, true), (0, 1, false)] {
                let (Some(Some(a)), Some(Some(b))) =
                    (images.get(&(level, x, y)), images.get(&(level, x + dx, y + dy)))
                else {
                    continue;
                };
                seams_before.push(edge_step(a, b, horizontal));
                if let (Some(a), Some(b)) = (corrected.get(&(x, y)), corrected.get(&(x + dx, y + dy))) {
                    seams_after.push(edge_step(a, b, horizontal));
                }
            }
        }
    }

    println!("{layer} level {level} against {}: {pairs} tiles with their parent", level - 1);
    if pairs == 0 {
        return Ok(());
    }
    println!(
        "  one source resampled: {same} of {pairs}; agreement of shape, median {:.2}; tile gain, median {:.2} stops, 90th {:.2}",
        median(agreement),
        median(overall.clone()),
        percentile(overall, 0.9)
    );
    println!(
        "  tone gap to the parent, per block: median {:.3} → {:.3} stops; 90th {:.3} → {:.3}",
        median(before.clone()),
        median(after.clone()),
        percentile(before, 0.9),
        percentile(after, 0.9)
    );
    println!(
        "  step across neighbours' edges: median {:.3} → {:.3} stops; 90th {:.3} → {:.3}  ({} edges)",
        median(seams_before.clone()),
        median(seams_after.clone()),
        percentile(seams_before.clone(), 0.9),
        percentile(seams_after, 0.9),
        seams_before.len()
    );
    println!(
        "  detail kept: median {:.3}, 10th {:.3} (1 = all of it)",
        median(kept.clone()),
        percentile(kept, 0.1)
    );
    Ok(())
}
