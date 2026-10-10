// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! PROBE (exploration, not a feature): what a pack's table alone says of
//! the seams of its frames — no tile is read.
//!
//! ```text
//! cargo run --release -p tuile-film --example seam_survey -- <pack> <out.csv> [frame …]
//! ```
//!
//! For each frame, every pair of drawn tiles that share a stretch of edge:
//! their levels, the level of the terrain tile each surface is from (a
//! tile cut from an ancestor is drawn with no skirt), whether the two are
//! cut from the same terrain tile, and the finest imagery level of each
//! drape. And, once, the colours the pack multiplies its tiles by.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use tuile_core::source::TileId;
use tuile_film::Cursor;
use tuile_pack::{blob_start, refs_of, Pack};

const DEEP: u32 = 30;

struct Drawn {
    level: u32,
    x: u64,
    y: u64,
    source: (u8, u32, u32),
    imagery: u8,
    layers: usize,
}

impl Drawn {
    fn span(&self, side: usize) -> (u64, (u64, u64)) {
        let shift = DEEP - self.level;
        let (x, y, one) = (self.x << shift, self.y << shift, 1u64 << shift);
        match side {
            0 => (x, (y, y + one)),
            2 => ((x + one) % (2u64 << DEEP), (y, y + one)),
            1 => (y, (x, x + one)),
            _ => (y + one, (x, x + one)),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: seam_survey <pack> <out.csv> [frame …]");
    let out = args.next().expect("an output csv");
    let bytes = std::fs::read(&path).expect("read");
    let start = blob_start(&bytes).expect("a pack") as usize;
    let pack = Pack::open_table(&bytes[..start]).expect("table");
    let (first, last) = pack.frame_range();
    let mut frames: Vec<u32> = args.map(|f| f.parse().expect("a frame")).collect();
    if frames.is_empty() {
        frames = vec![first, (first + last) / 2, last];
    }
    let mut csv = std::fs::File::create(&out).expect("csv");
    writeln!(
        csv,
        "frame,axis,a_level,a_source,a_imagery,b_level,b_source,b_imagery,same_surface,share"
    )
    .expect("csv");
    let mut factors: BTreeMap<String, u32> = BTreeMap::new();
    for frame in frames {
        let diff = Cursor::new(&pack, frame, frame)
            .expect("cursor")
            .advance(&pack)
            .expect("a frame")
            .expect("diff");
        let mut drawn = Vec::new();
        let (mut unreferenced, mut bare) = (0, 0);
        for tile in &diff.enter {
            if let Some(f) = tile.base_color_factor() {
                let f: Vec<String> = f.iter().map(|c| format!("{c:.2}")).collect();
                *factors.entry(f.join(" ")).or_default() += 1;
            }
            let (level, x, y) = TileId(tile.id()).terrain_coord();
            let Some(refs) = refs_of(tile) else {
                unreferenced += 1;
                continue;
            };
            bare += usize::from(refs.imagery.is_empty());
            drawn.push(Drawn {
                level,
                x,
                y,
                source: (refs.terrain.level, refs.terrain.x, refs.terrain.y),
                imagery: refs.imagery.iter().map(|p| p.tile.level).max().unwrap_or(0),
                layers: refs.imagery.len(),
            });
        }
        let mut lines: HashMap<(usize, u64), Vec<(usize, usize)>> = HashMap::new();
        for (n, tile) in drawn.iter().enumerate() {
            for side in 0..4 {
                lines
                    .entry((side % 2, tile.span(side).0))
                    .or_default()
                    .push((n, side));
            }
        }
        let mut pairs = 0u32;
        for sides in lines.values() {
            for &(a, side_a) in sides.iter().filter(|s| s.1 >= 2) {
                for &(b, side_b) in sides.iter().filter(|s| s.1 + 2 == side_a) {
                    let (ta, tb) = (&drawn[a], &drawn[b]);
                    let ((_, (a0, a1)), (_, (b0, b1))) = (ta.span(side_a), tb.span(side_b));
                    let (g0, g1) = (a0.max(b0), a1.min(b1));
                    if g0 >= g1 {
                        continue;
                    }
                    pairs += 1;
                    writeln!(
                        csv,
                        "{frame},{},{},{},{},{},{},{},{},{:.6}",
                        if side_a == 2 { "meridian" } else { "parallel" },
                        ta.level,
                        ta.source.0,
                        ta.imagery,
                        tb.level,
                        tb.source.0,
                        tb.imagery,
                        u8::from(ta.source == tb.source),
                        (g1 - g0) as f64 / (1u64 << (DEEP - ta.level.max(tb.level))) as f64,
                    )
                    .expect("csv");
                }
            }
        }
        let cut = drawn
            .iter()
            .filter(|t| u32::from(t.source.0) < t.level)
            .count();
        let layers: usize = drawn.iter().map(|t| t.layers).sum();
        println!(
            "frame {frame}: {} tiles drawn ({unreferenced} without references), {cut} cut from an \
             ancestor's terrain, {bare} with no imagery, {layers} drape layers, {pairs} shared edges",
            drawn.len()
        );
        let mut by_level: BTreeMap<(u32, u8), u32> = BTreeMap::new();
        for t in &drawn {
            *by_level.entry((t.level, t.source.0)).or_default() += 1;
        }
        for ((level, source), count) in by_level {
            println!("  level {level:>2} from terrain level {source:>2}: {count}");
        }
    }
    println!("base colour factors: {factors:?}");
}
