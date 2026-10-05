// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::collections::BTreeMap;
use std::ops::Range;

use tuile_pack::{fb, Pack};

use crate::{Cursor, FilmError};

/// One read to make from the blob region, and which of the wanted spans it
/// serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    pub range: Range<u64>,
    /// Indices into the spans handed to [`coalesce`].
    pub serves: Vec<usize>,
}

/// Merges the spans a frame needs into as few reads as possible.
///
/// Spans closer than `gap` bytes are read together: a round trip costs far
/// more than a few kilobytes of bytes nobody asked for, and the bake writes
/// tiles that entered together side by side, so a frame's newcomers mostly
/// come back as one or two ranges.
///
/// No read grows past `max` bytes by merging. A frame can bring in a hundred
/// megabytes of tiles at once — the first of a film usually does — and one
/// read of that size is more than a server will answer and more than a
/// reader should hold: it is cut into several, each still whole tiles. A
/// single span larger than `max` is its own read; it cannot be cut.
pub fn coalesce(spans: &[Range<u64>], gap: u64, max: u64) -> Vec<Fetch> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| spans[i].start);
    let mut out: Vec<Fetch> = Vec::new();
    for i in order {
        let span = &spans[i];
        match out.last_mut() {
            Some(f)
                if span.start <= f.range.end.saturating_add(gap)
                    && f.range.end.max(span.end) - f.range.start <= max =>
            {
                f.range.end = f.range.end.max(span.end);
                f.serves.push(i);
            }
            _ => out.push(Fetch {
                range: span.clone(),
                serves: vec![i],
            }),
        }
    }
    out
}

/// The reads that bring `tiles` in, **in file offsets**.
///
/// A pack's blocks are addressed from the start of its blob region, which
/// begins `blob_start` bytes into the file (right after the head); a ranged
/// reader works in file offsets. Getting that shift wrong reads the wrong
/// bytes and fails only at decompression, which is why it is done here, once.
/// A tile's bytes are then `bytes[f.range.start - blob_start ..]` of its
/// fetch, i.e. `Mesh::of_span(pack, tile, f.range.start - blob_start, bytes)`.
pub fn file_reads(
    pack: &Pack<'_>,
    blob_start: u64,
    tiles: &[fb::Tile<'_>],
    gap: u64,
    max: u64,
) -> Vec<Fetch> {
    let spans: Vec<Range<u64>> = tiles
        .iter()
        .map(|t| {
            pack.span_of(t)
                .map_or(0..0, |r| blob_start + r.start..blob_start + r.end)
        })
        .collect();
    coalesce(&spans, gap, max)
}

/// The fixed blocks of a pack's file that rendering `first..=last` reads,
/// each with the last frame that reads it.
///
/// A renderer brings a frame's tiles in with [`file_reads`]; this walks the
/// same frames and makes the same reads, so the blocks named here are
/// exactly the ones the render will ask for — no more, which is what lets
/// them all be fetched before the first frame, and no fewer. The last frame
/// of each is when it can be let go.
pub fn block_plan(
    pack: &Pack<'_>,
    blob_start: u64,
    first: u32,
    last: u32,
    block: u64,
    gap: u64,
    max: u64,
) -> Result<BTreeMap<u64, u32>, FilmError> {
    let mut plan = BTreeMap::new();
    let mut cursor = Cursor::new(pack, first, last)?;
    while let Some(diff) = cursor.advance(pack) {
        let diff = diff?;
        for fetch in file_reads(pack, blob_start, &diff.enter, gap, max) {
            if fetch.range.is_empty() {
                continue;
            }
            for index in fetch.range.start / block..fetch.range.end.div_ceil(block) {
                plan.insert(index, diff.frame);
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::tests::three_frames;
    use crate::{texture, texture_of_span, Mesh};

    /// A hundred megabytes of neighbours is not one read: no merged read
    /// passes the cap, every span is still served whole by exactly one, and
    /// a span bigger than the cap stands alone.
    #[test]
    fn no_read_grows_past_the_cap() {
        let spans: Vec<Range<u64>> = (0..40).map(|i| i * 10..i * 10 + 10).collect();
        let reads = coalesce(&spans, 100, 64);
        assert!(reads.len() > 1);
        for f in &reads {
            assert!(f.range.end - f.range.start <= 64, "{:?}", f.range);
            for &i in &f.serves {
                assert!(f.range.start <= spans[i].start && spans[i].end <= f.range.end);
            }
        }
        let mut served: Vec<usize> = reads.iter().flat_map(|f| f.serves.clone()).collect();
        served.sort_unstable();
        assert_eq!(
            served,
            (0..40).collect::<Vec<_>>(),
            "each span in exactly one read"
        );

        let big = coalesce(&[0..10, 10..500, 500..510], 100, 64);
        assert_eq!(
            big.iter().map(|f| f.range.clone()).collect::<Vec<_>>(),
            [0..10, 10..500, 500..510]
        );
    }

    /// The plan names every block the render's own reads touch, and for
    /// each the last frame that needs it.
    #[test]
    fn a_plan_names_the_blocks_a_render_reads_and_when_each_is_done() {
        let bytes = three_frames();
        let start = tuile_pack::blob_start(&bytes).expect("preamble");
        let table = Pack::open_table(&bytes[..start as usize]).expect("table");
        // Tiny blocks, so a small pack spans several.
        let block = 16;
        let plan = block_plan(&table, start, 10, 12, block, 0, u64::MAX).expect("plan");

        let mut cursor = Cursor::new(&table, 10, 12).expect("cursor");
        let mut touched: BTreeMap<u64, u32> = BTreeMap::new();
        while let Some(diff) = cursor.advance(&table) {
            let diff = diff.expect("diff");
            for f in file_reads(&table, start, &diff.enter, 0, u64::MAX) {
                for index in f.range.start / block..f.range.end.div_ceil(block) {
                    touched.insert(index, diff.frame);
                }
            }
        }
        assert!(!plan.is_empty());
        assert_eq!(plan, touched);
        // Frame 12 brings a tile in, so something is still needed then, and
        // nothing is needed outside the slice.
        assert!(plan.values().any(|f| *f == 12));
        assert!(plan.values().all(|f| (10..=12).contains(f)));
        // A slice of one frame needs fewer blocks than the whole film.
        let one = block_plan(&table, start, 11, 11, block, 0, u64::MAX).expect("plan");
        assert!(one.len() <= plan.len());
        assert!(one.values().all(|f| *f == 11));
    }

    #[test]
    fn file_reads_bring_in_what_the_whole_pack_holds() {
        let bytes = three_frames();
        let whole = Pack::open(&bytes).expect("open");
        let start = tuile_pack::blob_start(&bytes).expect("preamble");
        let table = Pack::open_table(&bytes[..start as usize]).expect("table");
        let tiles = table.frame(11).expect("frame");
        let reads = file_reads(&table, start, &tiles, 0, u64::MAX);
        for f in &reads {
            let got = &bytes[f.range.start as usize..f.range.end as usize];
            for &i in &f.serves {
                let tile = &tiles[i];
                let at = f.range.start - start;
                let reference = whole.frame(11).expect("frame")[i];
                assert_eq!(
                    Mesh::of_span(&table, tile, at, got).expect("ranged"),
                    Mesh::of(&whole, &reference).expect("whole")
                );
                assert_eq!(
                    texture_of_span(&table, tile, at, got).expect("ranged"),
                    texture(&whole, &reference).expect("whole")
                );
            }
        }
    }

    #[test]
    fn neighbours_merge_and_strangers_do_not() {
        let spans = [100..200, 0..50, 210..300, 10_000..10_100];
        let f = coalesce(&spans, 32, u64::MAX);
        assert_eq!(f.len(), 3);
        assert_eq!(
            f[0],
            Fetch {
                range: 0..50,
                serves: vec![1]
            }
        );
        assert_eq!(
            f[1],
            Fetch {
                range: 100..300,
                serves: vec![0, 2]
            }
        );
        assert_eq!(f[2].range, 10_000..10_100);
    }
}
