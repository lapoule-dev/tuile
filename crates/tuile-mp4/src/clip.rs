// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A clip from a phone, read for decoding: its video track, how it is to
//! be turned to be shown upright, and the colour its pictures are in.
//!
//! A phone writes an mp4 or a QuickTime movie (`.mov`): the same boxes, a
//! few of their own beside them (sound descriptions of version 1 or 2, a
//! metadata track, `nclc` colour), all of which are read or passed over
//! here. What a decoder alone does not say, and a viewer of the clip must
//! know, is in the container:
//!
//! - **Its turn.** A phone held upright records the sensor's landscape
//!   pictures and says, in the track header's matrix, that they are to be
//!   shown turned a quarter clockwise ([`Rotation`]).
//! - **Its colour.** A phone records in HDR by default — BT.2020 primaries,
//!   the HLG or PQ transfer, ten bits — and says so in the sample entry's
//!   `colr` box, or else in the stream's own parameters (the HEVC sequence
//!   parameter set's VUI). A clip that says nothing is BT.709 ([`Colour`]).
//!
//! The samples are not read here: [`Clip::track`] says where each lies, as
//! [`crate::film::read_track`] does for a film.

use std::io::{Read, Seek};

use mp4_atom::{Codec, FourCC, Matrix, Trak};

use crate::film::{read_moov, track_of, Track};
use crate::Mp4Error;

/// How a clip's pictures are turned to be shown upright: clockwise, in
/// quarter turns, as the track header's matrix says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    None,
    /// A quarter turn clockwise: a phone held upright.
    Quarter,
    Half,
    /// Three quarters clockwise (a quarter counter-clockwise).
    ThreeQuarters,
}

impl Rotation {
    /// Clockwise, in degrees.
    pub fn degrees(self) -> u32 {
        match self {
            Rotation::None => 0,
            Rotation::Quarter => 90,
            Rotation::Half => 180,
            Rotation::ThreeQuarters => 270,
        }
    }

    /// The size a picture of `size` is shown at once turned.
    pub fn shown(self, (width, height): (u32, u32)) -> (u32, u32) {
        match self {
            Rotation::Quarter | Rotation::ThreeQuarters => (height, width),
            Rotation::None | Rotation::Half => (width, height),
        }
    }
}

/// How the track header's matrix turns the pictures, and whether it
/// mirrors them: `None` for a matrix that is neither a turn by quarters nor
/// one mirrored (a scale, a shear), which no phone writes.
///
/// The matrix maps a picture's point `(p, q)` — `q` down — to
/// `(a·p + c·q + x, b·p + d·q + y)` (ISO/IEC 14496-12, 8.3.2.3), its
/// entries in 16.16 fixed point: `a = d = 0, b = 1, c = −1` is a quarter
/// turn clockwise, the top of the picture shown on its right.
pub fn rotation_of(matrix: &Matrix) -> Option<(Rotation, bool)> {
    let unit = |v: i32| -> Option<i32> {
        match v {
            0 => Some(0),
            0x1_0000 => Some(1),
            -0x1_0000 => Some(-1),
            _ => None,
        }
    };
    let (a, b, c, d) = (
        unit(matrix.a)?,
        unit(matrix.b)?,
        unit(matrix.c)?,
        unit(matrix.d)?,
    );
    // The first column says where the picture's x axis goes; a mirror is a
    // second column turned the other way (a negative determinant).
    let mirrored = a * d - b * c < 0;
    let turn = match (a, b) {
        (1, 0) => Rotation::None,
        (0, 1) => Rotation::Quarter,
        (-1, 0) => Rotation::Half,
        (0, -1) => Rotation::ThreeQuarters,
        _ => return None,
    };
    // A turn's second column is its first turned a quarter clockwise.
    let (want_c, want_d) = if mirrored { (b, -a) } else { (-b, a) };
    (c == want_c && d == want_d).then_some((turn, mirrored))
}

/// The colour of a clip's pictures, as ITU-T H.273 numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
    /// Where it was read: the container, the stream, or neither (BT.709
    /// then, as an SDR clip that says nothing is).
    pub said_by: ColourSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourSource {
    Container,
    Stream,
    Assumed,
}

/// Code points of ITU-T H.273 a clip may say.
pub mod h273 {
    pub const BT709: u16 = 1;
    pub const UNSPECIFIED: u16 = 2;
    pub const BT2020_PRIMARIES: u16 = 9;
    pub const PQ: u16 = 16;
    pub const HLG: u16 = 18;
    pub const BT2020_NCL: u16 = 9;
}

impl Colour {
    /// BT.709, limited range: what a clip that says nothing is.
    pub fn assumed() -> Self {
        Colour {
            primaries: h273::BT709,
            transfer: h273::BT709,
            matrix: h273::BT709,
            full_range: false,
            said_by: ColourSource::Assumed,
        }
    }

    /// Its pictures are in a high dynamic range: the HLG or PQ transfer.
    pub fn hdr(&self) -> bool {
        matches!(self.transfer, h273::HLG | h273::PQ)
    }

    /// Its primaries are BT.2020's.
    pub fn wide(&self) -> bool {
        self.primaries == h273::BT2020_PRIMARIES
    }

    /// Unspecified fields as BT.709's: a stream may say its transfer and
    /// leave its matrix unspecified.
    fn settled(mut self) -> Self {
        let or_709 = |v: u16| {
            if v == h273::UNSPECIFIED {
                h273::BT709
            } else {
                v
            }
        };
        self.primaries = or_709(self.primaries);
        self.transfer = or_709(self.transfer);
        self.matrix = or_709(self.matrix);
        self
    }
}

/// A clip's video track, its turn and its colour.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub track: Track,
    pub rotation: Rotation,
    /// Shown mirrored, left for right, before it is turned.
    pub mirrored: bool,
    pub colour: Colour,
}

impl Clip {
    /// Its pictures' size as they are coded.
    pub fn coded_size(&self) -> Option<(u32, u32)> {
        crate::film::size_of(&self.track.entry).map(|(w, h)| (u32::from(w), u32::from(h)))
    }

    /// Its pictures' size as they are shown, once turned.
    pub fn shown_size(&self) -> Option<(u32, u32)> {
        self.coded_size().map(|s| self.rotation.shown(s))
    }

    /// Its length in seconds.
    pub fn seconds(&self) -> f64 {
        self.track.seconds()
    }
}

/// The clip in `file`: its first enabled video track in H.264 or HEVC
/// (`avc1`, `hvc1` or `hev1`). `Ok(None)` for a file with no `moov` or no
/// such track; an error for a matrix no phone writes.
pub fn read_clip<R: Read + Seek>(file: &mut R) -> Result<Option<Clip>, Mp4Error> {
    let Some(moov) = read_moov(file)? else {
        return Ok(None);
    };
    let video = |t: &&Trak| t.mdia.hdlr.handler == FourCC::new(b"vide");
    let readable = |t: &&Trak| {
        matches!(
            t.mdia.minf.stbl.stsd.codecs.first(),
            Some(Codec::Avc1(_) | Codec::Hvc1(_) | Codec::Hev1(_))
        )
    };
    // The track shown: enabled, else the first there is.
    let Some(trak) = moov
        .trak
        .iter()
        .filter(video)
        .filter(readable)
        .find(|t| t.tkhd.enabled)
        .or_else(|| moov.trak.iter().filter(video).find(readable))
    else {
        return Ok(None);
    };
    let entry = trak.mdia.minf.stbl.stsd.codecs[0].clone();
    let (rotation, mirrored) = rotation_of(&trak.tkhd.matrix).ok_or_else(|| {
        Mp4Error::Atom(format!(
            "a clip shown through a matrix that is not a turn: {:?}",
            trak.tkhd.matrix
        ))
    })?;
    let colour = colour_of(&entry);
    Ok(Some(Clip {
        track: track_of(trak, entry)?,
        rotation,
        mirrored,
        colour,
    }))
}

/// The colour `entry` says: its `colr` box, else its HEVC stream's VUI,
/// else [`Colour::assumed`].
pub fn colour_of(entry: &Codec) -> Colour {
    let colr = match entry {
        Codec::Avc1(e) => e.colr.as_ref(),
        Codec::Hvc1(e) => e.colr.as_ref(),
        Codec::Hev1(e) => e.colr.as_ref(),
        _ => None,
    };
    let said = |primaries, transfer, matrix, full_range, said_by| {
        Colour {
            primaries,
            transfer,
            matrix,
            full_range,
            said_by,
        }
        .settled()
    };
    match colr {
        Some(mp4_atom::Colr::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range_flag,
        }) => {
            return said(
                *colour_primaries,
                *transfer_characteristics,
                *matrix_coefficients,
                *full_range_flag,
                ColourSource::Container,
            )
        }
        // QuickTime's own: no range said, limited as video is.
        Some(mp4_atom::Colr::Nclc {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
        }) => {
            return said(
                *colour_primaries,
                *transfer_characteristics,
                *matrix_coefficients,
                false,
                ColourSource::Container,
            )
        }
        _ => {}
    }
    let hvcc = match entry {
        Codec::Hvc1(e) => Some(&e.hvcc),
        Codec::Hev1(e) => Some(&e.hvcc),
        _ => None,
    };
    let signal = hvcc.and_then(|hvcc| {
        hvcc.arrays
            .iter()
            .filter(|a| a.nal_unit_type == 33)
            .flat_map(|a| a.nalus.iter())
            .find_map(|sps| {
                let parsed = scuffle_h265::SpsNALUnit::parse(sps.as_slice()).ok()?;
                let vui = parsed.rbsp.vui_parameters?;
                let s = vui.video_signal_type;
                Some((
                    u16::from(s.colour_primaries),
                    u16::from(s.transfer_characteristics),
                    u16::from(s.matrix_coeffs),
                    s.video_full_range_flag,
                ))
            })
    });
    match signal {
        Some((p, t, m, full)) => said(p, t, m, full, ColourSource::Stream),
        None => Colour::assumed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mp4_atom::Encode;

    fn matrix(a: i32, b: i32, c: i32, d: i32) -> Matrix {
        Matrix {
            a,
            b,
            u: 0,
            c,
            d,
            v: 0,
            x: 0,
            y: 0,
            w: 0x4000_0000,
        }
    }

    const ONE: i32 = 0x1_0000;

    /// The four turns a phone writes, and their mirrors, read as such; a
    /// scale is no turn.
    #[test]
    fn the_track_header_says_how_the_clip_is_turned() {
        assert_eq!(
            rotation_of(&matrix(ONE, 0, 0, ONE)),
            Some((Rotation::None, false))
        );
        // A phone held upright: the picture's top shown on its right.
        assert_eq!(
            rotation_of(&matrix(0, ONE, -ONE, 0)),
            Some((Rotation::Quarter, false))
        );
        assert_eq!(
            rotation_of(&matrix(-ONE, 0, 0, -ONE)),
            Some((Rotation::Half, false))
        );
        assert_eq!(
            rotation_of(&matrix(0, -ONE, ONE, 0)),
            Some((Rotation::ThreeQuarters, false))
        );
        assert_eq!(
            rotation_of(&matrix(-ONE, 0, 0, ONE)),
            Some((Rotation::Half, true)),
            "left for right: a half turn of a picture upside down"
        );
        assert_eq!(
            rotation_of(&matrix(ONE, 0, 0, -ONE)),
            Some((Rotation::None, true))
        );
        assert_eq!(rotation_of(&matrix(2 * ONE, 0, 0, ONE)), None);
        assert_eq!(rotation_of(&matrix(0, ONE, ONE, ONE)), None);
        assert_eq!(Rotation::Quarter.shown((1920, 1080)), (1080, 1920));
        assert_eq!(Rotation::Half.shown((1920, 1080)), (1920, 1080));
    }

    /// The direction of a quarter turn, from the matrix's own arithmetic:
    /// the picture's top-right corner is shown at the bottom-right of the
    /// upright picture, its top-left at the top-right.
    #[test]
    fn a_quarter_turn_shows_the_top_of_the_picture_on_its_right() {
        let (w, h) = (1920i64, 1080i64);
        let m = matrix(0, ONE, -ONE, 0);
        // The translation a phone writes with it: the picture kept in view.
        let x = h;
        let map = |p: i64, q: i64| {
            let f = |v: i32| i64::from(v) / i64::from(ONE);
            (f(m.a) * p + f(m.c) * q + x, f(m.b) * p + f(m.d) * q)
        };
        assert_eq!(map(w, 0), (h, w), "top-right → bottom-right");
        assert_eq!(map(0, 0), (h, 0), "top-left → top-right");
        assert_eq!(map(0, h), (0, 0), "bottom-left → top-left");
    }

    /// A sequence parameter set with a VUI that says BT.2020 and HLG,
    /// written bit by bit (ITU-T H.265, 7.3.2.2 and E.2.1): 1920 × 1080,
    /// Main 10.
    fn sps_hlg() -> Vec<u8> {
        let mut w = Bits::default();
        w.put(4, 0); // sps_video_parameter_set_id
        w.put(3, 0); // sps_max_sub_layers_minus1
        w.put(1, 1); // sps_temporal_id_nesting_flag
                     // profile_tier_level(1, 0): Main 10, level 5.1.
        w.put(2, 0);
        w.put(1, 0);
        w.put(5, 2);
        w.put(32, 0x2000_0000);
        w.put(1, 1); // progressive
        w.put(1, 0);
        w.put(1, 0);
        w.put(1, 1); // frame only
        w.put(32, 0);
        w.put(12, 0); // 44 reserved bits in all
        w.put(8, 153); // general_level_idc
        w.ue(0); // sps_seq_parameter_set_id
        w.ue(1); // chroma_format_idc: 4:2:0
        w.ue(1920); // pic_width_in_luma_samples
        w.ue(1080);
        w.put(1, 1); // conformance_window_flag
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(2); // bit_depth_luma_minus8
        w.ue(2);
        w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
        w.put(1, 1); // sps_sub_layer_ordering_info_present_flag
        w.ue(4);
        w.ue(2);
        w.ue(0);
        w.ue(0); // log2_min_luma_coding_block_size_minus3
        w.ue(3);
        w.ue(0);
        w.ue(3);
        w.ue(2); // max_transform_hierarchy_depth_inter
        w.ue(2);
        w.put(1, 0); // scaling_list_enabled_flag
        w.put(1, 1); // amp_enabled_flag
        w.put(1, 1); // sample_adaptive_offset_enabled_flag
        w.put(1, 0); // pcm_enabled_flag
        w.ue(0); // num_short_term_ref_pic_sets
        w.put(1, 0); // long_term_ref_pics_present_flag
        w.put(1, 1); // sps_temporal_mvp_enabled_flag
        w.put(1, 1); // strong_intra_smoothing_enabled_flag
        w.put(1, 1); // vui_parameters_present_flag
                     // vui_parameters()
        w.put(1, 0); // aspect_ratio_info_present_flag
        w.put(1, 0); // overscan_info_present_flag
        w.put(1, 1); // video_signal_type_present_flag
        w.put(3, 5); // video_format
        w.put(1, 0); // video_full_range_flag
        w.put(1, 1); // colour_description_present_flag
        w.put(8, 9); // BT.2020
        w.put(8, 18); // HLG
        w.put(8, 9); // BT.2020 non-constant
        w.put(1, 0); // chroma_loc_info_present_flag
        w.put(1, 0); // neutral_chroma_indication_flag
        w.put(1, 0); // field_seq_flag
        w.put(1, 0); // frame_field_info_present_flag
        w.put(1, 0); // default_display_window_flag
        w.put(1, 0); // vui_timing_info_present_flag
        w.put(1, 0); // bitstream_restriction_flag
        w.put(1, 0); // sps_extension_present_flag
        w.put(1, 1); // rbsp_stop_one_bit
        let mut nal = vec![33 << 1, 1];
        nal.extend(w.bytes());
        nal
    }

    #[derive(Default)]
    struct Bits {
        out: Vec<u8>,
        n: u32,
    }

    impl Bits {
        fn put(&mut self, count: u32, value: u64) {
            for i in (0..count).rev() {
                if self.n.is_multiple_of(8) {
                    self.out.push(0);
                }
                let bit = (value >> i) & 1;
                *self.out.last_mut().unwrap() |= (bit as u8) << (7 - self.n % 8);
                self.n += 1;
            }
        }

        fn ue(&mut self, value: u64) {
            let v = value + 1;
            let len = 64 - v.leading_zeros();
            self.put(len - 1, 0);
            self.put(len, v);
        }

        /// With emulation prevention: no three bytes 0, 0, ≤ 3.
        fn bytes(&self) -> Vec<u8> {
            let mut out = Vec::new();
            let mut zeros = 0;
            for &b in &self.out {
                if zeros == 2 && b <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                out.push(b);
                zeros = if b == 0 { zeros + 1 } else { 0 };
            }
            out
        }
    }

    fn hvc1(sps: Vec<u8>, colr: Option<mp4_atom::Colr>) -> Codec {
        Codec::Hvc1(mp4_atom::Hvc1 {
            visual: mp4_atom::Visual {
                data_reference_index: 1,
                width: 1920,
                height: 1080,
                ..Default::default()
            },
            hvcc: mp4_atom::Hvcc {
                configuration_version: 1,
                general_profile_idc: 2,
                length_size_minus_one: 3,
                chroma_format_idc: 1,
                bit_depth_luma_minus8: 2,
                bit_depth_chroma_minus8: 2,
                arrays: vec![mp4_atom::HvcCArray {
                    completeness: true,
                    nal_unit_type: 33,
                    nalus: vec![sps],
                }],
                ..Default::default()
            },
            colr,
            ..Default::default()
        })
    }

    /// The colour a clip says, from where it says it: the container's
    /// `colr` first (`nclx`, or QuickTime's `nclc`), the stream's VUI
    /// without one, BT.709 when neither does.
    #[test]
    fn a_clip_says_its_colour_in_its_container_or_in_its_stream() {
        let hlg = colour_of(&hvc1(sps_hlg(), None));
        assert_eq!(hlg.said_by, ColourSource::Stream);
        assert_eq!((hlg.primaries, hlg.transfer, hlg.matrix), (9, 18, 9));
        assert!(hlg.hdr() && hlg.wide() && !hlg.full_range);

        let nclc = mp4_atom::Colr::Nclc {
            colour_primaries: 9,
            transfer_characteristics: 16,
            matrix_coefficients: 9,
        };
        let pq = colour_of(&hvc1(sps_hlg(), Some(nclc)));
        assert_eq!(pq.said_by, ColourSource::Container, "the container first");
        assert_eq!(pq.transfer, h273::PQ);

        let unspecified = mp4_atom::Colr::Nclx {
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 2,
            full_range_flag: true,
        };
        let sdr = colour_of(&hvc1(sps_hlg(), Some(unspecified)));
        assert_eq!(sdr.matrix, h273::BT709, "unspecified is BT.709");
        assert!(sdr.full_range && !sdr.hdr());

        let silent =
            crate::film::avc1(64, 48, &[0x67, 0x64, 0, 0x28, 0xac], &[0x68, 0xeb]).expect("avc1");
        assert_eq!(colour_of(&silent), Colour::assumed());
    }

    /// A box of `kind` around `body`.
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(body);
        out
    }

    /// A QuickTime sound description of version 1, as a phone's `.mov`
    /// carries its AAC: the version-0 fields, then four 32-bit ones more,
    /// then its `wave` holding the `esds`.
    fn qt_sound_entry() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend([0u8; 6]);
        body.extend(1u16.to_be_bytes()); // data_reference_index
        body.extend(1u16.to_be_bytes()); // version 1
        body.extend([0u8; 6]);
        body.extend(2u16.to_be_bytes()); // channels
        body.extend(16u16.to_be_bytes());
        body.extend([0xff, 0xfe, 0, 0]); // compression id −2, packet size
        body.extend((44_100u32 << 16).to_be_bytes());
        body.extend(1024u32.to_be_bytes()); // samples per packet
        body.extend(0u32.to_be_bytes());
        body.extend(0u32.to_be_bytes());
        body.extend(2u32.to_be_bytes());
        // `wave`: the format, the `esds`, a terminator.
        let mut esds = Vec::new();
        crate::audio::aac_esds(&[0x11, 0x90], 128_000)
            .encode(&mut esds)
            .expect("esds");
        let wave = [boxed(b"frma", b"mp4a"), esds, boxed(b"\0\0\0\0", &[])].concat();
        body.extend(boxed(b"wave", &wave));
        boxed(b"mp4a", &body)
    }

    /// A video track of `entry`, two samples at 1000, a quarter turn.
    fn video_trak(track_id: u32, enabled: bool, entry: Codec) -> Trak {
        use mp4_atom::*;
        Trak {
            tkhd: Tkhd {
                track_id,
                enabled,
                in_movie: true,
                matrix: matrix(0, ONE, -ONE, 0),
                width: 1920.into(),
                height: 1080.into(),
                ..Default::default()
            },
            mdia: Mdia {
                mdhd: Mdhd {
                    timescale: 600,
                    duration: 40,
                    ..Default::default()
                },
                hdlr: Hdlr {
                    handler: FourCC::new(b"vide"),
                    name: "Core Media Video".into(),
                },
                minf: Minf {
                    vmhd: Some(Vmhd::default()),
                    dinf: Dinf {
                        dref: Dref {
                            urls: vec![Url {
                                location: "".into(),
                            }],
                        },
                    },
                    stbl: Stbl {
                        stsd: Stsd {
                            codecs: vec![entry],
                        },
                        stts: Stts {
                            entries: vec![SttsEntry {
                                sample_count: 2,
                                sample_delta: 20,
                            }],
                        },
                        stss: Some(Stss { entries: vec![1] }),
                        stsc: Stsc {
                            entries: vec![StscEntry {
                                first_chunk: 1,
                                samples_per_chunk: 2,
                                sample_description_index: 1,
                            }],
                        },
                        stsz: Stsz {
                            samples: StszSamples::Different { sizes: vec![5, 3] },
                        },
                        stco: Some(Stco {
                            entries: vec![1000],
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
            ..Default::default()
        }
    }

    /// An iPhone-shaped movie: `ftyp qt`, a sound track whose description
    /// is QuickTime's version 1 with its `esds` in a `wave` — before the
    /// video in the file — then the video: HEVC, HLG said in `nclc`, a
    /// quarter turn. Its `moov` is written box by box, its tracks' tables
    /// empty but for the video's two samples.
    fn iphone_movie() -> Vec<u8> {
        iphone_movie_with(&[])
    }

    /// [`iphone_movie`], `before` written ahead of its video track.
    fn iphone_movie_with(before: &[Trak]) -> Vec<u8> {
        use mp4_atom::*;
        let video = video_trak(
            1,
            true,
            hvc1(
                sps_hlg(),
                Some(Colr::Nclc {
                    colour_primaries: 9,
                    transfer_characteristics: 18,
                    matrix_coefficients: 9,
                }),
            ),
        );
        let mut video_bytes = Vec::new();
        for trak in before {
            trak.encode(&mut video_bytes).expect("trak");
        }
        video.encode(&mut video_bytes).expect("trak");
        // The sound track, by hand: mp4-atom writes version 0 only.
        let mut stbl = Vec::new();
        let mut stsd = vec![0u8; 4];
        stsd.extend(1u32.to_be_bytes());
        stsd.extend(qt_sound_entry());
        stbl.extend(boxed(b"stsd", &stsd));
        for kind in [b"stts", b"stsc", b"stco"] {
            stbl.extend(boxed(kind, &[0u8; 8]));
        }
        stbl.extend(boxed(b"stsz", &[0u8; 12]));
        let mut hdlr = vec![0u8; 8];
        hdlr.extend(b"soun");
        hdlr.extend([0u8; 12]);
        hdlr.push(0);
        let url = [0u8, 0, 0, 1];
        let mut dref = vec![0u8; 4];
        dref.extend(1u32.to_be_bytes());
        dref.extend(boxed(b"url ", &url));
        let minf = [
            boxed(b"smhd", &[0u8; 8]),
            boxed(b"dinf", &boxed(b"dref", &dref)),
            boxed(b"stbl", &stbl),
        ]
        .concat();
        let mut mdhd = vec![0u8; 4];
        mdhd.extend([0u8; 8]);
        mdhd.extend(44_100u32.to_be_bytes());
        mdhd.extend(0u32.to_be_bytes());
        mdhd.extend([0x55, 0xc4, 0, 0]);
        let mut tkhd = vec![0, 0, 0, 3];
        tkhd.extend([0u8; 8]);
        tkhd.extend(2u32.to_be_bytes());
        tkhd.extend([0u8; 4 + 4 + 8]);
        tkhd.extend([0u8; 4]); // layer, alternate group
        tkhd.extend([1, 0, 0, 0]); // volume
        for v in [ONE, 0, 0, 0, ONE, 0, 0, 0, 0x4000_0000] {
            tkhd.extend(v.to_be_bytes());
        }
        tkhd.extend([0u8; 8]);
        let sound = boxed(
            b"trak",
            &[
                boxed(b"tkhd", &tkhd),
                boxed(
                    b"mdia",
                    &[
                        boxed(b"mdhd", &mdhd),
                        boxed(b"hdlr", &hdlr),
                        boxed(b"minf", &minf),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        );
        let mvhd = {
            let mut m = Vec::new();
            Mvhd {
                timescale: 600,
                duration: 40,
                rate: 1.into(),
                volume: 1.into(),
                next_track_id: 3,
                ..Default::default()
            }
            .encode(&mut m)
            .expect("mvhd");
            m
        };
        let moov = boxed(b"moov", &[mvhd.as_slice(), &sound, &video_bytes].concat());
        let mut file = Vec::new();
        let mut ftyp = b"qt  ".to_vec();
        ftyp.extend(0u32.to_be_bytes());
        ftyp.extend(b"qt  ");
        file.extend(boxed(b"ftyp", &ftyp));
        file.extend(boxed(b"wide", &[]));
        file.extend(moov);
        file
    }

    /// A phone's movie read as a clip: its video track found past its
    /// sound track and QuickTime's own descriptions, a quarter turn to be
    /// shown upright, HLG from its `nclc`.
    #[test]
    fn a_phone_movie_is_read_as_a_clip_turned_upright_in_hlg() {
        let file = iphone_movie();
        let clip = read_clip(&mut std::io::Cursor::new(&file))
            .expect("read")
            .expect("a clip");
        assert_eq!(clip.rotation, Rotation::Quarter);
        assert!(!clip.mirrored);
        assert_eq!(clip.coded_size(), Some((1920, 1080)));
        assert_eq!(clip.shown_size(), Some((1080, 1920)));
        assert_eq!(clip.colour.transfer, h273::HLG);
        assert_eq!(clip.colour.said_by, ColourSource::Container);
        assert_eq!(clip.track.timescale, 600);
        let sizes: Vec<u32> = clip.track.samples.iter().map(|s| s.size).collect();
        assert_eq!(sizes, [5, 3]);
        assert_eq!(clip.track.samples[1].offset, 1005);
        assert!(clip.track.samples[0].key && !clip.track.samples[1].key);
        assert!((clip.seconds() - 40.0 / 600.0).abs() < 1e-9);
        // A film's reader, which reads only what the renderer writes,
        // finds the same video track.
        let film = crate::film::read_track(&mut std::io::Cursor::new(&file))
            .expect("read")
            .expect("track");
        assert_eq!(film, clip.track);
    }

    /// A movie with a disabled video track before the one shown: the one
    /// shown is the clip.
    #[test]
    fn the_clip_is_the_track_shown() {
        let hidden = video_trak(
            3,
            false,
            crate::film::avc1(64, 48, &[0x67, 0x64, 0, 0x28, 0xac], &[0x68, 0xeb]).expect("avc1"),
        );
        let file = iphone_movie_with(&[hidden]);
        let clip = read_clip(&mut std::io::Cursor::new(&file))
            .expect("read")
            .expect("a clip");
        assert!(
            matches!(clip.track.entry, Codec::Hvc1(_)),
            "{:?}",
            clip.track.entry
        );
        assert_eq!(clip.colour.transfer, h273::HLG);
    }

    /// A file with no movie in it is no clip.
    #[test]
    fn a_file_with_no_movie_is_no_clip() {
        let file = boxed(b"ftyp", b"isom\0\0\0\0isom");
        assert_eq!(read_clip(&mut std::io::Cursor::new(&file)).unwrap(), None);
    }
}
