// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The last stage of a film: its segments joined, then encoded once more
//! into the file a viewer gets, by a **delivery profile**.
//!
//! ```text
//! render segments (mezzanine) ─► join ─► compress <profile> ─► <run>/films/<profile>-v<n>.mp4
//! ```
//!
//! The render nodes write a near-transparent HEVC Main10 *mezzanine*
//! ([`Profile::mezzanine`]); the join copies its samples
//! ([`crate::concat`]); the compression decodes the joined film and encodes
//! it again in **one** encoder session for the whole film — lookahead,
//! B-frames and adaptive quantisation across the segments' joins, a size
//! cap that holds over the whole film — and lays the result out for a
//! viewer (faststart, BT.709 said).
//!
//! The profiles here are generic, typed encoder specs; a product names its
//! own choices after them. A profile's [`version`](Profile::version) moves
//! whenever its settings do, and is part of its file's key (`<name>-v<version>`): a retuned
//! profile makes new files and never overwrites a delivered one.
//!
//! This module is the contract (the profiles, the order, the answer); the
//! decode and the encode are the render service's (`tuile-film-native`).

use serde::{Deserialize, Serialize};

/// What a profile encodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoCodec {
    /// H.264 High: the one codec every player, browser and messaging app
    /// takes. Eight bits only.
    H264,
    /// HEVC Main or Main10, in an `hvc1` entry.
    Hevc,
}

/// Bitrates of a profile at one pixel rate (width × height × frames a
/// second): the mean a constant-quality encode of our films is expected to
/// land on, and the ceiling it may not pass. Megabits a second.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RatePoint {
    pub pixels_per_second: f64,
    pub mean_mbps: f64,
    pub max_mbps: f64,
}

/// A generic encoder spec for one delivered file (or the mezzanine).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Its name among [`Profile::preset`]'s.
    pub name: String,
    /// Raised whenever a setting below changes: the key of every file made
    /// with it (`<name>-v<version>`).
    pub version: u32,
    pub codec: VideoCodec,
    /// Bits per sample of the encoded stream: 8 or 10.
    pub bit_depth: u8,
    /// The constant quality aimed at, on the encoder's scale (NVENC's
    /// `targetQuality`, 1 best to 51); the bitrate follows the picture,
    /// under [`Self::rates`]' ceiling.
    pub quality: u8,
    /// The encoder's speed/quality preset, 1 (fastest) to 7 (best).
    pub preset: u8,
    /// B-frames between two references (0: none).
    pub b_frames: u8,
    /// Frames the rate control looks ahead (0: none).
    pub lookahead: u8,
    /// A key frame (an IDR: a closed GOP) every this many seconds.
    pub gop_seconds: f32,
    /// Spatial adaptive quantisation (flat areas, the sky's gradients, get
    /// finer steps) and its temporal counterpart.
    pub spatial_aq: bool,
    pub temporal_aq: bool,
    /// A full-resolution first pass for each picture's rate decisions.
    pub multipass: bool,
    /// The ceiling and mean bitrate by pixel rate; between two points, by
    /// the line through them in log–log, beyond the ends by their nearest
    /// segment's slope.
    pub rates: Vec<RatePoint>,
    /// The whole file's largest size, bytes: a film that comes out larger
    /// is encoded again, at the mean rate that fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cap_bytes: Option<u64>,
}

/// The pixel rates of the films made today, as rate tables name them.
const P720X1280_30: f64 = 720.0 * 1280.0 * 30.0;
const P1080_30: f64 = 1920.0 * 1080.0 * 30.0;
const P1080_60: f64 = 1920.0 * 1080.0 * 60.0;
const P2160_30: f64 = 3840.0 * 2160.0 * 30.0;
const P2160_60: f64 = 3840.0 * 2160.0 * 60.0;

fn table(points: [(f64, f64, f64); 5]) -> Vec<RatePoint> {
    points
        .into_iter()
        .map(|(pixels_per_second, mean_mbps, max_mbps)| RatePoint {
            pixels_per_second,
            mean_mbps,
            max_mbps,
        })
        .collect()
}

impl Profile {
    /// H.264 High, 8 bits dithered from the mezzanine's ten: plays
    /// everywhere. Constant quality with a ceiling by pixel rate, three
    /// B-frames, lookahead, spatial and temporal AQ, a key frame every two
    /// seconds. Rates: the starting points of the study
    /// (`docs/compression-pipeline.md` §3.2), to be calibrated.
    pub fn h264() -> Self {
        Self {
            name: "h264".into(),
            version: 1,
            codec: VideoCodec::H264,
            bit_depth: 8,
            quality: 24,
            preset: 6,
            b_frames: 3,
            lookahead: 32,
            gop_seconds: 2.0,
            spatial_aq: true,
            temporal_aq: true,
            multipass: true,
            rates: table([
                (P720X1280_30, 3.0, 5.0),
                (P1080_30, 6.0, 10.0),
                (P1080_60, 9.0, 14.0),
                (P2160_30, 25.0, 40.0),
                (P2160_60, 38.0, 60.0),
            ]),
            size_cap_bytes: None,
        }
    }

    /// HEVC Main10: the high-quality file, for Apple devices, editing and
    /// re-encoding platforms. Four B-frames, a key frame a second.
    pub fn hevc_main10() -> Self {
        Self {
            name: "hevc-main10".into(),
            version: 1,
            codec: VideoCodec::Hevc,
            bit_depth: 10,
            quality: 21,
            preset: 7,
            b_frames: 4,
            lookahead: 32,
            gop_seconds: 1.0,
            spatial_aq: true,
            temporal_aq: true,
            multipass: true,
            rates: table([
                (P720X1280_30, 6.0, 10.0),
                (P1080_30, 14.0, 24.0),
                (P1080_60, 20.0, 32.0),
                (P2160_30, 45.0, 70.0),
                (P2160_60, 65.0, 100.0),
            ]),
            size_cap_bytes: None,
        }
    }

    /// What a render node writes for the final stage to read: HEVC Main10,
    /// near-transparent (a ceiling of 2.5 × [`Self::hevc_main10`]'s), a
    /// closed GOP a second so that every segment decodes on its own. No
    /// B-frames and no lookahead: a render node's encoder stays one picture
    /// in, one out, a few pictures behind the render.
    pub fn mezzanine() -> Self {
        let high = Self::hevc_main10();
        Self {
            name: "mezzanine".into(),
            version: 1,
            codec: VideoCodec::Hevc,
            bit_depth: 10,
            quality: 17,
            preset: 5,
            b_frames: 0,
            lookahead: 0,
            gop_seconds: 1.0,
            spatial_aq: true,
            temporal_aq: false,
            multipass: false,
            rates: high
                .rates
                .iter()
                .map(|p| RatePoint {
                    pixels_per_second: p.pixels_per_second,
                    mean_mbps: p.mean_mbps * 2.5,
                    max_mbps: p.max_mbps * 2.5,
                })
                .collect(),
            size_cap_bytes: None,
        }
    }

    /// A profile by its name: `h264`, `hevc-main10`, `mezzanine`.
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "h264" => Some(Self::h264()),
            "hevc-main10" => Some(Self::hevc_main10()),
            "mezzanine" => Some(Self::mezzanine()),
            _ => None,
        }
    }

    /// The key of the files it makes: `<name>-v<version>` — letters,
    /// digits and dashes, what any store key and any URL take as they are.
    pub fn key(&self) -> String {
        format!("{}-v{}", self.name, self.version)
    }

    /// The (mean, ceiling) bitrates of a `width`×`height` film at `fps`, in
    /// bits a second.
    pub fn bitrates(&self, width: u32, height: u32, fps: f64) -> (u32, u32) {
        let rate = f64::from(width) * f64::from(height) * fps.max(1.0);
        let at = |pick: fn(&RatePoint) -> f64| interpolate(&self.rates, rate, pick);
        let bits = |mbps: f64| (mbps * 1e6).round().clamp(1e5, f64::from(u32::MAX)) as u32;
        (bits(at(|p| p.mean_mbps)), bits(at(|p| p.max_mbps)))
    }

    /// The mean bitrate, bits a second, at which `seconds` of film fit in
    /// the size cap with room for the container (2 %); `None` without a cap.
    pub fn capped_bitrate(&self, seconds: f64) -> Option<u32> {
        let cap = self.size_cap_bytes?;
        let bits = cap as f64 * 8.0 * 0.98 / seconds.max(0.1);
        Some(bits.clamp(1e5, f64::from(u32::MAX)) as u32)
    }

    /// Frames between two key frames at `fps`.
    pub fn gop_frames(&self, fps: u32) -> u32 {
        ((self.gop_seconds * fps as f32).round() as u32).max(1)
    }
}

/// `pick` of `points` at `rate`, by the line through the two points around
/// it in log–log (the nearest two at either end).
fn interpolate(points: &[RatePoint], rate: f64, pick: fn(&RatePoint) -> f64) -> f64 {
    match points {
        [] => 0.0,
        [one] => pick(one) * rate / one.pixels_per_second,
        _ => {
            let i = points
                .windows(2)
                .position(|w| rate <= w[1].pixels_per_second)
                .unwrap_or(points.len() - 2);
            let (a, b) = (&points[i], &points[i + 1]);
            let (x0, x1) = (a.pixels_per_second.ln(), b.pixels_per_second.ln());
            let (y0, y1) = (pick(a).ln(), pick(b).ln());
            let t = (rate.ln() - x0) / (x1 - x0);
            (y0 + t * (y1 - y0)).exp()
        }
    }
}

/// How a film's delivered file is made from its joined segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Delivery {
    /// Decoded and encoded again by this profile.
    Encode { profile: Profile },
    /// The joined film as it is, laid out for a viewer: for a film whose
    /// segments are already its delivery (a spatial film's MV-HEVC).
    Copy,
}

impl Delivery {
    /// A delivery by name: `copy`, or a profile's ([`Profile::preset`]),
    /// with a size cap if given one.
    pub fn named(name: &str, size_cap_bytes: Option<u64>) -> Option<Self> {
        if name == "copy" {
            return Some(Delivery::Copy);
        }
        let mut profile = Profile::preset(name)?;
        profile.size_cap_bytes = size_cap_bytes;
        Some(Delivery::Encode { profile })
    }

    /// Its files' key: the profile's, or `copy-v1`.
    pub fn key(&self) -> String {
        match self {
            Delivery::Encode { profile } => profile.key(),
            Delivery::Copy => "copy-v1".into(),
        }
    }
}

/// Where a film's files are kept: `<run>/films/<delivery key>.mp4`.
pub fn film_key(run_prefix: &str, delivery: &Delivery) -> String {
    format!(
        "{}/films/{}.mp4",
        run_prefix.trim_end_matches('/'),
        delivery.key()
    )
}

/// Which store a key is in: the run store (packs, receipts, delivered
/// films), or the mezzanine store (segments that only the final stage
/// reads, kept near the cards that write and read them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Store {
    #[default]
    Run,
    Mezzanine,
}

/// A film to compress, as the render service is asked for it
/// (`POST /compress`): every segment a render's receipts vouch for, joined,
/// then delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompressOrder {
    /// The run's prefix in the run store (`<track>/<stamp>`).
    pub run: String,
    /// The render whose receipts name the segments.
    pub render_id: String,
    /// Its chunks: as many receipts are expected.
    pub chunks: u32,
    /// Frames a second the film must last at.
    pub fps: f64,
    /// Where the segments are.
    #[serde(default)]
    pub segments: Store,
    pub delivery: Delivery,
    /// Frames laid over by other segments, spliced in at the join
    /// ([`crate::concat::splice`]) before the delivery: the compression
    /// itself never knows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inserts: Vec<crate::concat::Insert>,
    /// The delivered file's key in the run store; [`film_key`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<String>,
}

/// What a compression made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressDone {
    /// The delivered file's key in the run store.
    pub key: String,
    pub frames: u64,
    pub bytes: u64,
    /// The joined segments' bytes: what the delivery started from.
    pub joined_bytes: u64,
    /// Encodes of the whole film: 1, or 2 when the first passed the size
    /// cap. 0 for a copy.
    pub passes: u32,
    /// The mean bitrate of the file, bits a second.
    pub bitrate: u64,
    pub fetch_seconds: f64,
    pub join_seconds: f64,
    pub encode_seconds: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rates the study set for the films made today come out at their
    /// sizes, and a size between two lands between their rates.
    #[test]
    fn a_profile_s_bitrate_follows_the_pixel_rate_not_one_number() {
        let web = Profile::h264();
        assert_eq!(web.bitrates(720, 1280, 30.0), (3_000_000, 5_000_000));
        assert_eq!(web.bitrates(1920, 1080, 30.0), (6_000_000, 10_000_000));
        assert_eq!(web.bitrates(3840, 2160, 60.0), (38_000_000, 60_000_000));
        let (_, between) = web.bitrates(2560, 1440, 30.0);
        assert!(between > 10_000_000 && between < 40_000_000, "{between}");
        let (_, beyond) = web.bitrates(7680, 4320, 30.0);
        assert!(beyond > 60_000_000, "{beyond}");
        let high = Profile::hevc_main10();
        assert!(high.bitrates(1920, 1080, 30.0).1 > web.bitrates(1920, 1080, 30.0).1);
        let mezzanine = Profile::mezzanine();
        assert_eq!(mezzanine.bitrates(1920, 1080, 30.0).1, 60_000_000);
        assert_eq!((mezzanine.b_frames, mezzanine.lookahead), (0, 0));
    }

    #[test]
    fn a_size_cap_gives_the_rate_that_fits_the_whole_film() {
        let mut web = Profile::h264();
        assert_eq!(web.capped_bitrate(180.0), None);
        web.size_cap_bytes = Some(100_000_000);
        let rate = web.capped_bitrate(180.0).expect("a cap");
        let bytes = f64::from(rate) * 180.0 / 8.0;
        assert!(bytes < 100e6 && bytes > 97e6, "{bytes}");
    }

    /// A delivery is named, keyed by its profile's version, and kept under
    /// the run; an order reads from JSON with only what it must say.
    #[test]
    fn a_delivery_is_named_and_keyed_by_its_version() {
        let web = Delivery::named("h264", Some(100_000_000)).expect("h264");
        assert_eq!(web.key(), "h264-v1");
        assert_eq!(
            film_key("1557732/20261020T1012Z-ab12/", &web),
            "1557732/20261020T1012Z-ab12/films/h264-v1.mp4"
        );
        assert_eq!(Delivery::named("copy", None), Some(Delivery::Copy));
        assert_eq!(Delivery::named("av1", None), None);
        let order: CompressOrder = serde_json::from_value(serde_json::json!({
            "run": "1/2", "render_id": "r", "chunks": 3, "fps": 30.0,
            "delivery": {"kind": "copy"},
        }))
        .expect("an order");
        assert_eq!((order.segments, order.out.clone()), (Store::Run, None));
        let again: CompressOrder = serde_json::from_str(
            &serde_json::to_string(&CompressOrder {
                delivery: web,
                ..order
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            matches!(again.delivery, Delivery::Encode { ref profile } if profile.size_cap_bytes == Some(100_000_000))
        );
    }

    #[test]
    fn a_gop_is_a_whole_number_of_frames() {
        assert_eq!(Profile::h264().gop_frames(30), 60);
        assert_eq!(Profile::hevc_main10().gop_frames(60), 60);
    }
}
