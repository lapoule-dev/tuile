// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film as a viewer gets it: one video track, every box written by
//! `mp4-atom`, laid out so that it plays from its first bytes.
//!
//! Three things every delivered film needs and that the encoders' own
//! muxers did not all give:
//!
//! - **Faststart.** `ftyp`, then `moov`, then `mdat`: a browser fetching the
//!   film plays as soon as the index is in, instead of fetching the end of
//!   the file first.
//! - **Its colour, said.** A `colr` box (`nclx`: BT.709 primaries, transfer
//!   and matrix, limited range) and a square-pixel `pasp` in the sample
//!   entry, so that no player has to guess. The renderer is SDR,
//!   display-referred, BT.709: there is nothing else to say.
//! - **Frames shown in another order than they are decoded.** An encoder
//!   with B-frames hands its pictures in decoding order, each with the place
//!   it is shown at. The composition offsets (`ctts`) are kept non-negative,
//!   and an edit list (`elst`) starts the film at the first picture shown —
//!   the layout every player reads.
//!
//! H.264 (`avc1`), HEVC (`hvc1`, one layer or two) and AV1 (`av01`) are
//! read and written alike: a join of segments ([`join`]) copies their
//! samples without decoding a picture, and refuses segments whose stream
//! descriptions differ.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use mp4_atom::{
    Co64, Codec, Ctts, CttsEntry, Decode, Dinf, Dref, Edts, Elst, ElstEntry, Encode, FourCC, Ftyp,
    Hdlr, Mdhd, Mdia, Minf, Moov, Mvhd, Pasp, Stbl, Stco, Stsc, StscEntry, Stsd, Stss, Stsz,
    StszSamples, Stts, SttsEntry, Tkhd, Trak, Url, Vmhd,
};

use crate::Mp4Error;

pub use mp4_atom::{Codec as Entry, Colr};

/// The colour of every film the renderer makes: BT.709 primaries, transfer
/// and matrix (code points 1, 1, 1 of ITU-T H.273), limited range.
pub fn bt709() -> Colr {
    Colr::Nclx {
        colour_primaries: 1,
        transfer_characteristics: 1,
        matrix_coefficients: 1,
        full_range_flag: false,
    }
}

/// The sample entry with what a player should not have to guess: its colour
/// ([`bt709`]) and square pixels, where the entry does not say otherwise.
pub fn dressed(entry: Codec) -> Codec {
    let pasp = || {
        Some(Pasp {
            h_spacing: 1,
            v_spacing: 1,
        })
    };
    match entry {
        Codec::Avc1(mut e) => {
            e.colr = e.colr.or_else(|| Some(bt709()));
            e.pasp = e.pasp.or_else(pasp);
            Codec::Avc1(e)
        }
        Codec::Hvc1(mut e) => {
            e.colr = e.colr.or_else(|| Some(bt709()));
            e.pasp = e.pasp.or_else(pasp);
            Codec::Hvc1(e)
        }
        Codec::Av01(mut e) => {
            e.colr = e.colr.or_else(|| Some(bt709()));
            e.pasp = e.pasp.or_else(pasp);
            Codec::Av01(e)
        }
        other => other,
    }
}

/// The width and height an entry describes, for the video codecs read here.
fn size_of(entry: &Codec) -> Option<(u16, u16)> {
    match entry {
        Codec::Avc1(e) => Some((e.visual.width, e.visual.height)),
        Codec::Hvc1(e) => Some((e.visual.width, e.visual.height)),
        Codec::Av01(e) => Some((e.visual.width, e.visual.height)),
        _ => None,
    }
}

/// Where one sample lies in its file, and how it is timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Byte offset in the file.
    pub offset: u64,
    pub size: u32,
    pub key: bool,
    /// Ticks of the track's timescale.
    pub duration: u32,
    /// Presentation against decoding, in ticks, as the file says it.
    pub composition: i64,
}

/// A film's video track, as its `moov` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub entry: Codec,
    pub timescale: u32,
    pub samples: Vec<Sample>,
    /// Where the edit list starts the film in the media, in ticks: the
    /// first picture shown when frames are reordered, else zero.
    pub start: i64,
}

impl Track {
    /// Its length, from its samples' durations.
    pub fn seconds(&self) -> f64 {
        let ticks: u64 = self.samples.iter().map(|s| u64::from(s.duration)).sum();
        ticks as f64 / f64::from(self.timescale.max(1))
    }

    pub fn width_height(&self) -> Option<(u16, u16)> {
        size_of(&self.entry)
    }
}

fn io(e: std::io::Error) -> Mp4Error {
    Mp4Error::Atom(e.to_string())
}

fn atom(e: mp4_atom::Error) -> Mp4Error {
    Mp4Error::Atom(e.to_string())
}

/// The video track of the film in `file`: its `moov` read, its `mdat` left
/// where it is. `Ok(None)` for a file with no `moov`, or whose first track
/// is not H.264, HEVC or AV1 video.
pub fn read_track<R: Read + Seek>(file: &mut R) -> Result<Option<Track>, Mp4Error> {
    let end = file.seek(SeekFrom::End(0)).map_err(io)?;
    let mut at = 0u64;
    let mut moov = None;
    while at + 8 <= end {
        file.seek(SeekFrom::Start(at)).map_err(io)?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head[..8]).map_err(io)?;
        let size = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
            0 => end - at,
            1 => {
                file.read_exact(&mut head[8..]).map_err(io)?;
                u64::from_be_bytes([
                    head[8], head[9], head[10], head[11], head[12], head[13], head[14], head[15],
                ])
            }
            n => u64::from(n),
        };
        if size < 8 {
            return Err(Mp4Error::Atom(format!("a box of {size} bytes at {at}")));
        }
        if &head[4..8] == b"moov" {
            file.seek(SeekFrom::Start(at)).map_err(io)?;
            let mut bytes = vec![0u8; size as usize];
            file.read_exact(&mut bytes).map_err(io)?;
            moov = Some(Moov::decode(&mut bytes.as_slice()).map_err(atom)?);
            break;
        }
        at += size;
    }
    let Some(moov) = moov else {
        return Ok(None);
    };
    let Some(trak) = moov
        .trak
        .iter()
        .find(|t| t.mdia.hdlr.handler == FourCC::new(b"vide"))
    else {
        return Ok(None);
    };
    let stbl = &trak.mdia.minf.stbl;
    let entry = match stbl.stsd.codecs.first() {
        Some(e @ (Codec::Avc1(_) | Codec::Hvc1(_) | Codec::Av01(_))) => e.clone(),
        _ => return Ok(None),
    };
    let sizes: Vec<u32> = match &stbl.stsz.samples {
        StszSamples::Different { sizes } => sizes.clone(),
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
    };
    let chunks: Vec<u64> = match (&stbl.co64, &stbl.stco) {
        (Some(c), _) => c.entries.clone(),
        (None, Some(c)) => c.entries.iter().map(|&o| u64::from(o)).collect(),
        _ => return Err(Mp4Error::Atom("no chunk offsets".into())),
    };
    let expand = |runs: Vec<(u32, i64)>| -> Vec<i64> {
        runs.into_iter()
            .flat_map(|(n, v)| std::iter::repeat_n(v, n as usize))
            .collect()
    };
    let durations = expand(
        stbl.stts
            .entries
            .iter()
            .map(|e| (e.sample_count, i64::from(e.sample_delta)))
            .collect(),
    );
    let composition = stbl.ctts.as_ref().map_or_else(Vec::new, |c| {
        expand(
            c.entries
                .iter()
                .map(|e| (e.sample_count, e.sample_offset))
                .collect(),
        )
    });
    let sync: std::collections::HashSet<u32> = stbl
        .stss
        .as_ref()
        .map_or_else(Default::default, |s| s.entries.iter().copied().collect());
    let mut samples = Vec::with_capacity(sizes.len());
    for (c, &offset) in chunks.iter().enumerate() {
        let chunk = c as u32 + 1;
        let per = stbl
            .stsc
            .entries
            .iter()
            .rev()
            .find(|r| r.first_chunk <= chunk)
            .ok_or_else(|| Mp4Error::Atom("a chunk of no run".into()))?
            .samples_per_chunk;
        let mut at = offset;
        for _ in 0..per {
            let n = samples.len();
            let Some(&size) = sizes.get(n) else { break };
            samples.push(Sample {
                offset: at,
                size,
                key: stbl.stss.is_none() || sync.contains(&(n as u32 + 1)),
                duration: durations.get(n).copied().unwrap_or(0) as u32,
                composition: composition.get(n).copied().unwrap_or(0),
            });
            at += u64::from(size);
        }
    }
    // The first edit that shows media says where the film starts in it.
    let start = trak
        .edts
        .as_ref()
        .and_then(|e| e.elst.as_ref())
        .and_then(|l| l.entries.iter().find_map(|e| e.media_time))
        .map_or(0, |t| t as i64);
    Ok(Some(Track {
        entry,
        timescale: trak.mdia.mdhd.timescale,
        samples,
        start,
    }))
}

fn ftyp(entry: &Codec) -> Ftyp {
    let codec = match entry {
        Codec::Avc1(_) => b"avc1",
        Codec::Hvc1(_) => b"hvc1",
        _ => b"av01",
    };
    Ftyp {
        major_brand: FourCC::new(b"isom"),
        minor_version: 512,
        compatible_brands: [b"isom", b"iso2", codec, b"mp41"].map(FourCC::new).to_vec(),
    }
}

/// Values as runs: (count, value).
fn runs<T: PartialEq + Copy>(values: &[T]) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for &v in values {
        match out.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

/// What a film's `moov` says of its one track, its samples in one chunk.
pub(crate) struct Table {
    pub(crate) entry: Codec,
    pub(crate) timescale: u32,
    pub(crate) sizes: Vec<u32>,
    /// 1-based, as `stss` counts.
    pub(crate) sync: Vec<u32>,
    pub(crate) durations: Vec<u32>,
    /// Presentation against decoding, in ticks.
    pub(crate) composition: Vec<i64>,
    /// Where the film starts in its media, in ticks.
    pub(crate) start: i64,
}

impl Table {
    fn moov(&self, first_sample: u64) -> Moov {
        let (width, height) = size_of(&self.entry).unwrap_or((0, 0));
        let frames = self.sizes.len() as u32;
        let media: u64 = self.durations.iter().map(|&d| u64::from(d)).sum();
        let timescale = u64::from(self.timescale.max(1));
        // Rounded, not cut: a film's last frame is a whole frame.
        let movie = (media * 1000 + timescale / 2) / timescale;
        let ctts = self.composition.iter().any(|&o| o != 0).then(|| Ctts {
            entries: runs(&self.composition)
                .into_iter()
                .map(|(sample_count, sample_offset)| CttsEntry {
                    sample_count,
                    sample_offset,
                })
                .collect(),
        });
        let edts = (self.start != 0).then(|| Edts {
            elst: Some(Elst {
                entries: vec![ElstEntry {
                    segment_duration: movie,
                    media_time: Some(self.start as u64),
                    media_rate: 1.into(),
                }],
            }),
        });
        Moov {
            mvhd: Mvhd {
                timescale: 1000,
                duration: movie,
                rate: 1.into(),
                volume: 1.into(),
                next_track_id: 2,
                ..Default::default()
            },
            trak: vec![Trak {
                tkhd: Tkhd {
                    track_id: 1,
                    duration: movie,
                    enabled: true,
                    in_movie: true,
                    width: width.into(),
                    height: height.into(),
                    ..Default::default()
                },
                edts,
                mdia: Mdia {
                    mdhd: Mdhd {
                        timescale: self.timescale,
                        duration: media,
                        language: "und".into(),
                        ..Default::default()
                    },
                    hdlr: Hdlr {
                        handler: FourCC::new(b"vide"),
                        name: "VideoHandler".into(),
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
                                codecs: vec![self.entry.clone()],
                            },
                            stts: Stts {
                                entries: runs(&self.durations)
                                    .into_iter()
                                    .map(|(sample_count, sample_delta)| SttsEntry {
                                        sample_count,
                                        sample_delta,
                                    })
                                    .collect(),
                            },
                            ctts,
                            stss: Some(Stss {
                                entries: self.sync.clone(),
                            }),
                            stsc: Stsc {
                                entries: vec![StscEntry {
                                    first_chunk: 1,
                                    samples_per_chunk: frames,
                                    sample_description_index: 1,
                                }],
                            },
                            stsz: Stsz {
                                samples: StszSamples::Different {
                                    sizes: self.sizes.clone(),
                                },
                            },
                            stco: None::<Stco>,
                            co64: Some(Co64 {
                                entries: vec![first_sample],
                            }),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// `ftyp`, `moov`, and the head of a 64-bit `mdat` of `bytes` bytes:
    /// what goes before the samples. The samples' offset depends on the
    /// `moov`'s size, which does not depend on the offset (a 64-bit entry
    /// either way).
    pub(crate) fn head(&self, bytes: u64) -> Result<Vec<u8>, Mp4Error> {
        let mut out = Vec::new();
        ftyp(&self.entry).encode(&mut out).map_err(atom)?;
        let mut probe = Vec::new();
        self.moov(0).encode(&mut probe).map_err(atom)?;
        let first_sample = (out.len() + probe.len() + 16) as u64;
        self.moov(first_sample).encode(&mut out).map_err(atom)?;
        out.extend(1u32.to_be_bytes());
        out.extend(b"mdat");
        out.extend((bytes + 16).to_be_bytes());
        Ok(out)
    }
}

/// Two entries describe the same stream when they differ at most in what
/// says nothing of the encode: a spatial film's field of view (a camera's
/// field moves during a film), and what [`dressed`] adds.
fn same_stream(a: &Codec, b: &Codec) -> bool {
    let plain = |e: &Codec| match dressed(e.clone()) {
        Codec::Hvc1(mut h) => {
            h.hfov = None;
            Codec::Hvc1(h)
        }
        other => other,
    };
    plain(a) == plain(b)
}

/// Films one after the other into one at `out`, streamed from file to file
/// and laid out for a viewer (see the module): the same stream description
/// in each — a spatial film's field of view aside, the first segment's kept
/// — and the same timescale, or the join is refused. Returns the frames
/// written.
pub fn join(segments: &[PathBuf], out: &Path) -> Result<u64, Mp4Error> {
    join_into(segments, out, false)
}

/// [`join`], each segment's file removed once its samples are copied when
/// `consume` is set: where the disk is memory, the film and its segments
/// are then never held whole twice.
pub fn join_into(segments: &[PathBuf], out: &Path, consume: bool) -> Result<u64, Mp4Error> {
    let pieces: Vec<Piece> = segments.iter().map(|p| Piece::whole(p.clone())).collect();
    join_pieces(&pieces, out, consume)
}

/// A run of one film's samples, for [`join_pieces`]: the film at `path`,
/// all of it or its samples `samples` (decoding order, from 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub path: PathBuf,
    pub samples: Option<std::ops::Range<usize>>,
}

impl Piece {
    /// The whole film at `path`.
    pub fn whole(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            samples: None,
        }
    }

    /// Samples `samples` of the film at `path`: the first must be a sync
    /// sample, or the join refuses the piece.
    pub fn cut(path: impl Into<PathBuf>, samples: std::ops::Range<usize>) -> Self {
        Self {
            path: path.into(),
            samples: Some(samples),
        }
    }
}

/// Pieces of films one after the other into one at `out`, as [`join`]
/// joins whole films: the same stream description and timescale in each,
/// samples copied, nothing decoded. A piece cut out of a film starts on a
/// sync sample — what decodes on its own — or the join is refused, the
/// sample named; an empty piece is refused too. With `consume`, a film's
/// file is removed once the last piece read from it is copied.
pub fn join_pieces(pieces: &[Piece], out: &Path, consume: bool) -> Result<u64, Mp4Error> {
    let mut tracks = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let path = &piece.path;
        let mut file = File::open(path).map_err(io)?;
        let track = read_track(&mut file)?
            .ok_or_else(|| Mp4Error::Atom(format!("{}: no video track", path.display())))?;
        let range = piece.samples.clone().unwrap_or(0..track.samples.len());
        if range.end > track.samples.len() || (piece.samples.is_some() && range.is_empty()) {
            return Err(Mp4Error::Atom(format!(
                "{}: samples {range:?} of {}",
                path.display(),
                track.samples.len()
            )));
        }
        if piece.samples.is_some() && !track.samples[range.start].key {
            return Err(Mp4Error::Atom(format!(
                "{}: sample {} is not a key frame, and a piece cut there would not decode",
                path.display(),
                range.start
            )));
        }
        tracks.push((file, track, range));
    }
    let Some((_, first, _)) = tracks.first() else {
        return Err(Mp4Error::Atom("nothing to join".into()));
    };
    let (entry, timescale) = (dressed(first.entry.clone()), first.timescale);
    for ((_, t, _), piece) in tracks.iter().zip(pieces) {
        if !same_stream(&t.entry, &entry) {
            return Err(Mp4Error::Mismatch("stream description"));
        }
        if t.timescale != timescale {
            return Err(Mp4Error::Atom(format!(
                "{}: another timescale",
                piece.path.display()
            )));
        }
    }
    // Each segment shown from its own start: brought to one start, the
    // latest, so that no offset is negative.
    let start = tracks.iter().map(|(_, t, _)| t.start).max().unwrap_or(0);
    let mut table = Table {
        entry,
        timescale,
        sizes: Vec::new(),
        sync: Vec::new(),
        durations: Vec::new(),
        composition: Vec::new(),
        start,
    };
    let mut bytes = 0u64;
    for (_, track, range) in &tracks {
        for s in &track.samples[range.clone()] {
            table.sizes.push(s.size);
            if s.key {
                table.sync.push(table.sizes.len() as u32);
            }
            table.durations.push(s.duration);
            table.composition.push(s.composition - track.start + start);
            bytes += u64::from(s.size);
        }
    }
    let mut writer = BufWriter::new(File::create(out).map_err(io)?);
    writer.write_all(&table.head(bytes)?).map_err(io)?;
    let mut buffer = Vec::new();
    for (i, ((file, track, range), piece)) in tracks.into_iter().zip(pieces).enumerate() {
        let mut file = file;
        for s in &track.samples[range] {
            buffer.resize(s.size as usize, 0);
            file.seek(SeekFrom::Start(s.offset)).map_err(io)?;
            file.read_exact(&mut buffer).map_err(io)?;
            writer.write_all(&buffer).map_err(io)?;
        }
        drop(file);
        let read_again = pieces[i + 1..].iter().any(|p| p.path == piece.path);
        if consume && !read_again {
            std::fs::remove_file(&piece.path).map_err(io)?;
        }
    }
    writer.flush().map_err(io)?;
    Ok(table.sizes.len() as u64)
}

/// The film in `bytes` laid out again for a viewer: faststart, its colour
/// said. The samples are not touched. Bytes that hold no film this reads
/// come back as they were.
pub fn finished(bytes: Vec<u8>) -> Result<Vec<u8>, Mp4Error> {
    let Some(track) = read_track(&mut std::io::Cursor::new(&bytes))? else {
        return Ok(bytes);
    };
    let total: u64 = track.samples.iter().map(|s| u64::from(s.size)).sum();
    let table = Table {
        entry: dressed(track.entry.clone()),
        timescale: track.timescale,
        sync: track
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.key)
            .map(|(i, _)| i as u32 + 1)
            .collect(),
        sizes: track.samples.iter().map(|s| s.size).collect(),
        durations: track.samples.iter().map(|s| s.duration).collect(),
        composition: track.samples.iter().map(|s| s.composition).collect(),
        start: track.start,
    };
    let mut out = table.head(total)?;
    out.reserve(total as usize);
    for s in &track.samples {
        let at = s.offset as usize;
        let sample = bytes
            .get(at..at + s.size as usize)
            .ok_or_else(|| Mp4Error::Atom(format!("a sample past the end at {at}")))?;
        out.extend_from_slice(sample);
    }
    Ok(out)
}

/// A film written as its encoder hands it out: samples in decoding order,
/// each with the frame it is shown as. The samples wait in a file beside
/// the film (`<film>.samples`) — a film of minutes does not sit in memory —
/// and the film is laid out for a viewer when it is finished.
pub struct FilmWriter {
    path: PathBuf,
    spill: BufWriter<File>,
    spill_path: PathBuf,
    entry: Codec,
    ticks: u32,
    timescale: u32,
    sizes: Vec<u32>,
    sync: Vec<u32>,
    shown: Vec<i64>,
    bytes: u64,
}

/// What a finished film holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub frames: u64,
    pub bytes: u64,
}

impl FilmWriter {
    /// A film at `path` of `entry` (an `avc1`, `hvc1` or `av01`), `fps`
    /// frames a second.
    pub fn create(path: impl Into<PathBuf>, entry: Codec, fps: u32) -> Result<Self, Mp4Error> {
        let path = path.into();
        if size_of(&entry).is_none() {
            return Err(Mp4Error::Atom("not a video sample entry".into()));
        }
        let mut spill_path = path.clone().into_os_string();
        spill_path.push(".samples");
        let spill_path = PathBuf::from(spill_path);
        let spill = BufWriter::new(File::create(&spill_path).map_err(io)?);
        let ticks = crate::TICKS;
        Ok(Self {
            path,
            spill,
            spill_path,
            entry: dressed(entry),
            ticks,
            timescale: fps.max(1) * ticks,
            sizes: Vec::new(),
            sync: Vec::new(),
            shown: Vec::new(),
            bytes: 0,
        })
    }

    /// The next sample in decoding order — length-prefixed NAL units, or an
    /// AV1 temporal unit — shown as frame `shown` of the film (from 0).
    pub fn push(&mut self, sample: &[u8], key: bool, shown: u64) -> Result<(), Mp4Error> {
        if self.sizes.is_empty() && !key {
            return Err(Mp4Error::NoKeyFrame);
        }
        self.spill.write_all(sample).map_err(io)?;
        self.sizes.push(sample.len() as u32);
        if key {
            self.sync.push(self.sizes.len() as u32);
        }
        self.shown.push(shown as i64);
        self.bytes += sample.len() as u64;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.sizes.len() as u64
    }

    /// The composition offsets in ticks, none negative, and where the film
    /// starts in its media: a frame decoded `d` frames before it is shown
    /// at most delays every frame by `d`, and the edit list takes it back.
    fn composition(&self) -> Result<(Vec<i64>, i64), Mp4Error> {
        let mut seen = vec![false; self.shown.len()];
        for &s in &self.shown {
            match seen.get_mut(s as usize) {
                Some(slot) if !*slot => *slot = true,
                _ => {
                    return Err(Mp4Error::Atom(format!(
                        "frame {s} shown twice or past the film's {} frames",
                        self.shown.len()
                    )))
                }
            }
        }
        let ticks = i64::from(self.ticks);
        let delay = self
            .shown
            .iter()
            .enumerate()
            .map(|(i, &s)| i as i64 - s)
            .max()
            .unwrap_or(0)
            .max(0);
        let offsets = self
            .shown
            .iter()
            .enumerate()
            .map(|(i, &s)| (s - i as i64 + delay) * ticks)
            .collect();
        Ok((offsets, delay * ticks))
    }

    pub fn finish(self) -> Result<Written, Mp4Error> {
        let (composition, start) = self.composition()?;
        let mut spill = self.spill;
        spill.flush().map_err(io)?;
        drop(spill);
        let frames = self.sizes.len() as u64;
        let table = Table {
            entry: self.entry,
            timescale: self.timescale,
            durations: vec![self.ticks; self.sizes.len()],
            sizes: self.sizes,
            sync: self.sync,
            composition,
            start,
        };
        let mut writer = BufWriter::new(File::create(&self.path).map_err(io)?);
        let head = table.head(self.bytes)?;
        writer.write_all(&head).map_err(io)?;
        let mut samples = File::open(&self.spill_path).map_err(io)?;
        std::io::copy(&mut samples, &mut writer).map_err(io)?;
        writer.flush().map_err(io)?;
        drop(samples);
        std::fs::remove_file(&self.spill_path).map_err(io)?;
        Ok(Written {
            frames,
            bytes: head.len() as u64 + self.bytes,
        })
    }
}

/// An H.264 sample entry from the encoder's parameter sets (NAL units, no
/// start code), with the record's extension where the profile has one.
pub fn avc1(width: u16, height: u16, sps: &[u8], pps: &[u8]) -> Result<Codec, Mp4Error> {
    let mut avcc = mp4_atom::Avcc::new(sps, pps).map_err(atom)?;
    // High profiles carry chroma format and bit depths in the record.
    if matches!(avcc.avc_profile_indication, 100 | 110 | 122 | 244) {
        avcc.ext = Some(mp4_atom::AvccExt::default());
    }
    Ok(Codec::Avc1(mp4_atom::Avc1 {
        visual: mp4_atom::Visual {
            data_reference_index: 1,
            width,
            height,
            ..Default::default()
        },
        avcc,
        btrt: None,
        colr: None,
        pasp: None,
        taic: None,
        fiel: None,
    }))
}

/// A single-layer HEVC sample entry from the body of the encoder's `hvcC`
/// record ([`crate::spatial::hevc_record`]).
pub fn hvc1(width: u16, height: u16, hvcc: &[u8]) -> Result<Codec, Mp4Error> {
    let mut boxed = Vec::with_capacity(hvcc.len() + 8);
    boxed.extend(((hvcc.len() + 8) as u32).to_be_bytes());
    boxed.extend(b"hvcC");
    boxed.extend(hvcc);
    let hvcc = mp4_atom::Hvcc::decode(&mut boxed.as_slice()).map_err(atom)?;
    Ok(Codec::Hvc1(mp4_atom::Hvc1 {
        visual: mp4_atom::Visual {
            data_reference_index: 1,
            width,
            height,
            ..Default::default()
        },
        hvcc,
        ..Default::default()
    }))
}

/// An AV1 sample entry from the body of the stream's `av1C` record.
pub fn av01(width: u16, height: u16, av1c: &[u8]) -> Result<Codec, Mp4Error> {
    let mut boxed = Vec::with_capacity(av1c.len() + 8);
    boxed.extend(((av1c.len() + 8) as u32).to_be_bytes());
    boxed.extend(b"av1C");
    boxed.extend(av1c);
    let av1c = mp4_atom::Av1c::decode(&mut boxed.as_slice()).map_err(atom)?;
    Ok(Codec::Av01(mp4_atom::Av01 {
        visual: mp4_atom::Visual {
            data_reference_index: 1,
            width,
            height,
            ..Default::default()
        },
        av1c,
        btrt: None,
        ccst: None,
        colr: None,
        pasp: None,
        taic: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

    fn moov_of(file: &[u8]) -> (Vec<[u8; 4]>, Moov) {
        let mut kinds = Vec::new();
        let mut at = 0usize;
        let mut moov = None;
        while at + 8 <= file.len() {
            let size32 = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as u64;
            let size = if size32 == 1 {
                u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap())
            } else {
                size32
            } as usize;
            let kind: [u8; 4] = file[at + 4..at + 8].try_into().unwrap();
            if &kind == b"moov" {
                moov = Some(Moov::decode(&mut &file[at..at + size]).expect("moov"));
            }
            kinds.push(kind);
            at += size;
        }
        (kinds, moov.expect("a moov"))
    }

    fn written(dir: &Path, name: &str, frames: &[(u8, bool, u64)]) -> PathBuf {
        let path = dir.join(name);
        let entry = avc1(64, 48, SPS, PPS).expect("entry");
        let mut w = FilmWriter::create(&path, entry, 30).expect("writer");
        for &(tag, key, shown) in frames {
            w.push(&[0, 0, 0, 2, 0x65, tag], key, shown).expect("push");
        }
        w.finish().expect("finish");
        path
    }

    /// The index before the pictures, the colour said, and nothing left
    /// beside the film.
    #[test]
    fn a_film_plays_from_its_first_bytes_and_says_its_colour() {
        let dir = tempfile::tempdir().expect("dir");
        let path = written(dir.path(), "f.mp4", &[(0, true, 0), (1, false, 1)]);
        let file = std::fs::read(&path).expect("film");
        let (kinds, moov) = moov_of(&file);
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"mdat"], "faststart");
        let stbl = &moov.trak[0].mdia.minf.stbl;
        let Codec::Avc1(entry) = &stbl.stsd.codecs[0] else {
            panic!("avc1")
        };
        assert_eq!(entry.colr, Some(bt709()));
        assert_eq!(
            entry.pasp,
            Some(Pasp {
                h_spacing: 1,
                v_spacing: 1
            })
        );
        assert!(
            entry.avcc.ext.is_some(),
            "a High profile record says its depth"
        );
        let at = stbl.co64.as_ref().expect("co64").entries[0] as usize;
        assert_eq!(&file[at..at + 6], [0, 0, 0, 2, 0x65, 0]);
        assert!(
            stbl.ctts.is_none() && moov.trak[0].edts.is_none(),
            "nothing reordered"
        );
        assert_eq!(moov.mvhd.duration, 67, "two frames at 30 fps, rounded");
        assert!(!dir.path().join("f.mp4.samples").exists());
        // Another reader of mp4 finds the same two samples.
        let size = file.len() as u64;
        let mut r = mp4::Mp4Reader::read_header(std::io::Cursor::new(file), size).expect("mp4");
        let id = *r.tracks().keys().next().expect("track");
        assert_eq!(r.sample_count(id).expect("count"), 2);
        let s = r.read_sample(id, 2).expect("read").expect("sample");
        assert_eq!(s.bytes.as_ref(), [0, 0, 0, 2, 0x65, 1]);
    }

    /// Decoded I P B B, shown I B B P: offsets none negative, the film
    /// started at its first picture shown, each frame shown where it is.
    #[test]
    fn reordered_frames_are_shown_in_their_order_through_an_edit_list() {
        let dir = tempfile::tempdir().expect("dir");
        let order = [(0, true, 0), (1, false, 3), (2, false, 1), (3, false, 2)];
        let path = written(dir.path(), "b.mp4", &order);
        let mut file = File::open(&path).expect("film");
        let track = read_track(&mut file).expect("read").expect("track");
        assert!(track.samples.iter().all(|s| s.composition >= 0));
        let shown: Vec<i64> = track
            .samples
            .iter()
            .enumerate()
            .map(|(i, s)| (i as i64 * 512 + s.composition - track.start) / 512)
            .collect();
        assert_eq!(shown, [0, 3, 1, 2]);
        assert_eq!(track.start, 512, "one frame decoded ahead");
        let mut w = FilmWriter::create(
            dir.path().join("x.mp4"),
            avc1(64, 48, SPS, PPS).unwrap(),
            30,
        )
        .expect("writer");
        w.push(&[0], true, 0).unwrap();
        w.push(&[1], false, 0).unwrap();
        assert!(w.finish().is_err(), "a frame shown twice is refused");
    }

    /// Segments joined: every sample once and in order, their key frames
    /// kept, one start for the whole film — and another stream refused.
    #[test]
    fn segments_join_into_one_film_that_plays_from_its_first_bytes() {
        let dir = tempfile::tempdir().expect("dir");
        let a = written(
            dir.path(),
            "a.mp4",
            &[(0xa0, true, 0), (0xa1, false, 2), (0xa2, false, 1)],
        );
        let b = written(dir.path(), "b.mp4", &[(0xb0, true, 0), (0xb1, false, 1)]);
        let out = dir.path().join("film.mp4");
        assert_eq!(join(&[a.clone(), b], &out).expect("joined"), 5);
        let bytes = std::fs::read(&out).expect("film");
        assert_eq!(moov_of(&bytes).0, [*b"ftyp", *b"moov", *b"mdat"]);
        let track = read_track(&mut File::open(&out).unwrap()).unwrap().unwrap();
        let tags: Vec<u8> = track
            .samples
            .iter()
            .map(|s| bytes[s.offset as usize + 5])
            .collect();
        assert_eq!(tags, [0xa0, 0xa1, 0xa2, 0xb0, 0xb1]);
        let keys: Vec<bool> = track.samples.iter().map(|s| s.key).collect();
        assert_eq!(keys, [true, false, false, true, false]);
        let shown: Vec<i64> = track
            .samples
            .iter()
            .enumerate()
            .map(|(i, s)| (i as i64 * 512 + s.composition - track.start) / 512)
            .collect();
        assert_eq!(
            shown,
            [0, 2, 1, 3, 4],
            "the second segment shown after the first"
        );
        assert!((track.seconds() - 5.0 / 30.0).abs() < 1e-9);

        let other = dir.path().join("o.mp4");
        let entry = avc1(64, 48, &[0x67, 0x42, 0x00, 0x1e, 1], PPS).unwrap();
        let mut w = FilmWriter::create(&other, entry, 30).unwrap();
        w.push(&[0, 0, 0, 1, 0x65], true, 0).unwrap();
        w.finish().unwrap();
        assert!(matches!(
            join(&[a, other], &out),
            Err(Mp4Error::Mismatch(_))
        ));
    }

    /// A film written index-last, laid out again: the index first, the
    /// same samples, its colour said.
    #[test]
    fn a_film_written_index_last_is_finished_for_a_viewer() {
        let sets = crate::ParameterSets {
            sps: SPS.to_vec(),
            pps: PPS.to_vec(),
        };
        let mut m = crate::Muxer::new(64, 48, 30, sets).expect("muxer");
        m.push(0, vec![0, 0, 0, 2, 0x65, 7], true).unwrap();
        m.push(1, vec![0, 0, 0, 2, 0x41, 8], false).unwrap();
        let film = m.finish().expect("film");
        let (kinds, moov) = moov_of(&film);
        assert_eq!(kinds[..2], [*b"ftyp", *b"moov"], "{kinds:?}");
        let Codec::Avc1(entry) = &moov.trak[0].mdia.minf.stbl.stsd.codecs[0] else {
            panic!("avc1")
        };
        assert_eq!(entry.colr, Some(bt709()));
        let track = read_track(&mut std::io::Cursor::new(&film))
            .unwrap()
            .unwrap();
        let s = track.samples[1];
        assert_eq!(&film[s.offset as usize..][..6], [0, 0, 0, 2, 0x41, 8]);
    }

    /// One layer of HEVC: the record made from its parameter sets alone, an
    /// `hvc1` with no second layer, read back as written.
    #[test]
    fn a_single_layer_hevc_film_is_described_by_its_base_layer_alone() {
        const ENTRY: &[u8] = include_bytes!("../tests/spatial_hvc1.bin");
        let apple = mp4_atom::Hvc1::decode(&mut &ENTRY[..]).expect("an hvc1 entry");
        let base: Vec<&[u8]> = apple
            .hvcc
            .arrays
            .iter()
            .filter(|a| a.nal_unit_type != 39)
            .flat_map(|a| a.nalus.iter().map(Vec::as_slice))
            .collect();
        let record = crate::spatial::hevc_record(&base).expect("record");
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("h.mp4");
        let mut w = FilmWriter::create(&path, hvc1(1920, 1080, &record).expect("hvc1"), 30)
            .expect("writer");
        w.push(&[0, 0, 0, 2, 0x26, 0x01], true, 0).expect("push");
        w.finish().expect("finish");
        let track = read_track(&mut File::open(&path).unwrap())
            .unwrap()
            .unwrap();
        let Codec::Hvc1(entry) = &track.entry else {
            panic!("hvc1")
        };
        assert!(entry.lhvc.is_none());
        assert_eq!(
            entry.hvcc.general_profile_idc,
            apple.hvcc.general_profile_idc
        );
        assert_eq!(entry.colr, Some(bt709()));
        assert_eq!(track.width_height(), Some((1920, 1080)));
    }
}
