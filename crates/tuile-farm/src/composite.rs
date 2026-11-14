// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Frames laid over a range of a film's mezzanine, before the join.
//!
//! ```text
//! render segments (IDR forced at the range's first frame and the frame after its last)
//!   └─ composite: decode the range only ─► lay a still or a clip ─► encode the range only
//!        └─ an inserted segment, key frame to key frame, in the segments' store
//!             └─ join: spliced in by sample copy (crate::concat::splice) ─► compress
//! ```
//!
//! A layer covers each view of the picture (one, or two side by side for
//! a stereo film's eyes): scaled to cover it, centred, zoomed slowly from
//! [`CompositeOrder::zoom`]'s first value to its second over the range,
//! faded in from the film and back out to it over
//! [`CompositeOrder::fade_seconds`] at either end. A clip is resampled to
//! the film's cadence, its last picture held if it is shorter than the
//! range; its sound is not read.
//!
//! The inserted segment is encoded by the render's own encoder — its codec
//! and bitrate are in the order — so that its stream description is the
//! render's and the join takes it as it is.
//!
//! This module is the contract; the decode, the composite and the encode
//! are the render service's (`tuile-film-native`).

use serde::{Deserialize, Serialize};

use crate::compress::Store;

/// What is laid over the frames: a key in the run store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", deny_unknown_fields)]
pub enum Layer {
    /// A picture the `image` crate reads (JPEG, PNG), upright as it is
    /// stored: its orientation is its maker's to have applied.
    Still { key: String },
    /// A film: its first H.264 or HEVC video track, turned as its track
    /// header says; its sound is not read.
    Clip { key: String },
}

impl Layer {
    pub fn key(&self) -> &str {
        match self {
            Layer::Still { key } | Layer::Clip { key } => key,
        }
    }
}

fn one() -> u32 {
    1
}

fn no_zoom() -> (f64, f64) {
    (1.0, 1.0)
}

/// Frames `first..=last` of a render (numbered as its receipts number
/// them) with a layer laid over, as the render service is asked for it
/// (`POST /composite`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeOrder {
    /// The run's prefix in the run store.
    pub run: String,
    /// The render whose receipts name the segments.
    pub render_id: String,
    /// Its chunks: as many receipts are expected.
    pub chunks: u32,
    /// Where the render's segments are; the inserted segment goes there
    /// too, under [`Self::out`].
    #[serde(default)]
    pub segments: Store,
    /// Frames a second of the film.
    pub fps: f64,
    pub first: u64,
    pub last: u64,
    pub layer: Layer,
    /// Seconds of the fade in from the film, and of the fade out back to
    /// it; at most a quarter of the range each. 0: cut.
    #[serde(default)]
    pub fade_seconds: f64,
    /// The layer's zoom at the range's first frame and at its end (1: it
    /// just covers a view), changing linearly between.
    #[serde(default = "no_zoom")]
    pub zoom: (f64, f64),
    /// Views side by side in the picture, each given the layer: 2 for a
    /// stereo film's eyes packed side by side, the layer at the screen's
    /// depth.
    #[serde(default = "one")]
    pub views: u32,
    /// The render's encoder, as its segments were asked for it
    /// (`RenderSpec::codec`), and its megabits a second (`RenderSpec::mbps`):
    /// the inserted segment must be the same stream.
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mbps: Option<f64>,
    /// The inserted segment's key in the segments' store.
    pub out: String,
}

impl CompositeOrder {
    pub fn frames(&self) -> u64 {
        self.last + 1 - self.first
    }

    /// The splice the join is then asked for ([`crate::concat::splice`]).
    pub fn insert(&self) -> crate::concat::Insert {
        crate::concat::Insert {
            first: self.first,
            last: self.last,
            key: self.out.clone(),
        }
    }

    /// The frames a key frame is forced on in the render, for these frames
    /// to be cut out and spliced in: the first, and the one after the last.
    pub fn key_frames(&self) -> [u64; 2] {
        [self.first, self.last + 1]
    }
}

/// What a composite made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompositeDone {
    /// The inserted segment's key in the segments' store.
    pub key: String,
    pub frames: u64,
    pub bytes: u64,
    /// The render's segments and the layer, fetched.
    pub fetch_seconds: f64,
    /// Decode, composite and encode, which run together.
    pub composite_seconds: f64,
}

/// The layer's opacity at frame `t` of `n` (from 0), the fade lasting
/// `fade` frames at either end, at most a quarter of the range: 0 on the
/// first and last frames — the film's own picture — and 1 between the
/// fades.
pub fn opacity(t: u64, n: u64, fade: f64) -> f32 {
    let fade = fade.min(n as f64 / 4.0);
    if fade < 1.0 {
        return 1.0;
    }
    let from_edge = t.min(n.saturating_sub(1).saturating_sub(t)) as f64;
    (from_edge / fade).min(1.0) as f32
}

/// The layer's zoom at frame `t` of `n` (from 0): `from` on the first,
/// moving linearly by an `n`th of the way each frame.
pub fn zoom_at(t: u64, n: u64, (from, to): (f64, f64)) -> f64 {
    from + (to - from) * t as f64 / n.max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fade starts and ends on the film's own picture, is symmetric,
    /// and opaque between; one longer than a quarter of the range is held
    /// to a quarter.
    #[test]
    fn the_layer_fades_in_from_the_film_and_back_out_to_it() {
        let n = 90;
        let fade = 15.0;
        assert_eq!(opacity(0, n, fade), 0.0);
        assert_eq!(opacity(n - 1, n, fade), 0.0);
        assert_eq!(opacity(45, n, fade), 1.0);
        for t in 0..n {
            assert_eq!(opacity(t, n, fade), opacity(n - 1 - t, n, fade), "{t}");
        }
        assert!((opacity(5, n, fade) - 5.0 / 15.0).abs() < 1e-6);
        assert_eq!(opacity(15, n, fade), 1.0);
        // Twelve frames: fades of three, whatever was asked.
        assert!((opacity(1, 12, 15.0) - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(opacity(0, 12, 0.0), 1.0, "no fade: a cut");
    }

    /// The zoom moves from its first value by an `n`th a frame, as the
    /// host's ffmpeg pass did it (`1 + 0.08 · on / frames`).
    #[test]
    fn the_zoom_moves_linearly_over_the_range() {
        assert_eq!(zoom_at(0, 100, (1.0, 1.08)), 1.0);
        assert!((zoom_at(50, 100, (1.0, 1.08)) - 1.04).abs() < 1e-12);
        assert!((zoom_at(99, 100, (1.0, 1.08)) - 1.0792).abs() < 1e-12);
    }

    /// An order reads from JSON with only what it must say; its insert and
    /// key frames follow from its frames.
    #[test]
    fn an_order_reads_from_json_and_says_its_splice() {
        let order: CompositeOrder = serde_json::from_value(serde_json::json!({
            "run": "1/2", "render_id": "r", "chunks": 3, "fps": 30.0,
            "first": 121, "last": 270,
            "layer": {"kind": "still", "key": "1/2/media/0.jpg"},
            "codec": "mezzanine", "out": "1/2/inserts/0.mp4",
        }))
        .expect("an order");
        assert_eq!(
            (order.views, order.zoom, order.fade_seconds),
            (1, (1.0, 1.0), 0.0)
        );
        assert_eq!(order.segments, Store::Run);
        assert_eq!(order.frames(), 150);
        assert_eq!(order.key_frames(), [121, 271]);
        assert_eq!(order.insert().key, "1/2/inserts/0.mp4");
        assert_eq!(order.layer.key(), "1/2/media/0.jpg");
        assert!(serde_json::from_value::<CompositeOrder>(serde_json::json!({
            "run": "1/2", "render_id": "r", "chunks": 3, "fps": 30.0, "first": 1, "last": 2,
            "layer": {"kind": "still", "key": "k"}, "codec": "mezzanine", "out": "o",
            "zoom_to": 1.08,
        }))
        .is_err());
    }
}
