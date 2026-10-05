// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::ops::Range;

use tuile_pack::{fb, Pack};

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
pub fn coalesce(spans: &[Range<u64>], gap: u64) -> Vec<Fetch> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| spans[i].start);
    let mut out: Vec<Fetch> = Vec::new();
    for i in order {
        let span = &spans[i];
        match out.last_mut() {
            Some(f) if span.start <= f.range.end.saturating_add(gap) => {
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
) -> Vec<Fetch> {
    let spans: Vec<Range<u64>> = tiles
        .iter()
        .map(|t| {
            pack.span_of(t)
                .map_or(0..0, |r| blob_start + r.start..blob_start + r.end)
        })
        .collect();
    coalesce(&spans, gap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::tests::three_frames;
    use crate::{texture, texture_of_span, Mesh};

    #[test]
    fn file_reads_bring_in_what_the_whole_pack_holds() {
        let bytes = three_frames();
        let whole = Pack::open(&bytes).expect("open");
        let start = tuile_pack::blob_start(&bytes).expect("preamble");
        let table = Pack::open_table(&bytes[..start as usize]).expect("table");
        let tiles = table.frame(11).expect("frame");
        let reads = file_reads(&table, start, &tiles, 0);
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
        let f = coalesce(&spans, 32);
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
