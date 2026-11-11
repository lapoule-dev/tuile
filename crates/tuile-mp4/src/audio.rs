// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's sound: AAC frames laid beside its pictures, in one file a
//! viewer plays from its first bytes.
//!
//! The pictures are copied as they are, never decoded: [`with_audio`] reads
//! the film's video track, takes the sound's frames as an encoder handed
//! them, and writes both into a new file —
//!
//! - **faststart**, as every film here: `ftyp`, `moov`, then `mdat`;
//! - **interleaved by the second**: a second of pictures, then the second
//!   of sound that goes with it, so that a player reading the file from its
//!   start has both at once and never seeks from one end to the other;
//! - **the encoder's delay hidden**: an AAC encoder hands out `priming`
//!   samples of silence before the sound begins, and the sound track's edit
//!   list starts it after them, for as long as the sound lasts and never
//!   past the pictures' end — the film's length is the film's.
//!
//! The sound's frames come from wherever they were encoded; nothing here
//! encodes. Its sample entry is an `mp4a` from its `AudioSpecificConfig`
//! ([`aac_entry`]).

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use mp4_atom::{
    Co64, Codec, Dinf, Dref, Edts, Elst, ElstEntry, Encode, Esds, FourCC, Hdlr, Mdhd, Mdia, Minf,
    Moov, Mvhd, Smhd, Stbl, Stsc, StscEntry, Stsd, Stsz, StszSamples, Stts, SttsEntry, Tkhd, Trak,
    Url,
};

use crate::film::{atom, dressed, ftyp, io, read_track, Table, Written};
use crate::Mp4Error;

/// A film's sound, as an AAC encoder hands it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    /// Its sample entry ([`aac_entry`]).
    pub entry: Codec,
    /// Samples a second: the track's timescale.
    pub sample_rate: u32,
    /// Samples each frame decodes to (1024 for AAC-LC).
    pub frame_samples: u32,
    /// The frames, in order, as the encoder wrote them (raw, no ADTS).
    pub frames: Vec<Vec<u8>>,
    /// Samples of the encoder's own before the sound begins.
    pub priming: u32,
    /// Samples of sound, the priming left out: where the sound ends.
    pub length: u64,
}

/// The `esds` of an AAC stream from its `AudioSpecificConfig` (ISO/IEC
/// 14496-3, 1.6.2.1), at `avg_bitrate` bits a second.
pub fn aac_esds(asc: &[u8], avg_bitrate: u32) -> Esds {
    let (profile, freq_index, chan_conf) = match asc {
        [a, b, ..] => (a >> 3, ((a & 7) << 1) | (b >> 7), (b >> 3) & 0x0f),
        _ => (0, 0, 0),
    };
    Esds {
        es_desc: mp4_atom::esds::EsDescriptor {
            es_id: 1,
            dec_config: mp4_atom::esds::DecoderConfig {
                // MPEG-4 audio, an audio stream.
                object_type_indication: 0x40,
                stream_type: 5,
                up_stream: 0,
                buffer_size_db: [0, 0x18, 0].into(),
                max_bitrate: avg_bitrate,
                avg_bitrate,
                dec_specific: Some(mp4_atom::esds::DecoderSpecific {
                    profile,
                    freq_index,
                    chan_conf,
                    raw: asc.to_vec(),
                }),
            },
            sl_config: mp4_atom::esds::SLConfig::default(),
        },
    }
}

/// An `mp4a` sample entry: AAC, `channels` at `sample_rate`, from its
/// `AudioSpecificConfig`.
pub fn aac_entry(
    asc: &[u8],
    channels: u16,
    sample_rate: u32,
    avg_bitrate: u32,
) -> Result<Codec, Mp4Error> {
    let rate = u16::try_from(sample_rate)
        .map_err(|_| Mp4Error::Atom(format!("a sample rate of {sample_rate} in an mp4a entry")))?;
    Ok(Codec::Mp4a(mp4_atom::Mp4a {
        audio: mp4_atom::Audio {
            data_reference_index: 1,
            channel_count: channels,
            sample_size: 16,
            sample_rate: rate.into(),
        },
        esds: aac_esds(asc, avg_bitrate),
        btrt: None,
        taic: None,
    }))
}

/// The seconds a chunk of the file holds of each track.
const CHUNK_SECONDS: f64 = 1.0;

/// Which samples go in each chunk: for each second of the film, the
/// pictures then the sound that start in it, as `(track, first, count)` —
/// track 0 the pictures, 1 the sound.
fn interleave(video_starts: &[f64], audio_starts: &[f64]) -> Vec<(usize, usize, usize)> {
    let seconds = |starts: &[f64]| {
        starts
            .last()
            .map_or(0, |&t| (t / CHUNK_SECONDS) as usize + 1)
    };
    let windows = seconds(video_starts).max(seconds(audio_starts));
    let mut chunks = Vec::new();
    let mut next = [0usize; 2];
    for w in 0..windows {
        let end = (w + 1) as f64 * CHUNK_SECONDS;
        for (track, starts) in [video_starts, audio_starts].into_iter().enumerate() {
            let first = next[track];
            let last = if w + 1 == windows {
                starts.len()
            } else {
                first + starts[first..].iter().take_while(|&&t| t < end).count()
            };
            if last > first {
                chunks.push((track, first, last - first));
                next[track] = last;
            }
        }
    }
    chunks
}

/// The `stsc` of chunks of `counts` samples: runs of equal counts.
fn stsc_of(counts: &[u32]) -> Vec<StscEntry> {
    let mut entries: Vec<StscEntry> = Vec::new();
    for (i, &n) in counts.iter().enumerate() {
        if entries.last().is_none_or(|e| e.samples_per_chunk != n) {
            entries.push(StscEntry {
                first_chunk: i as u32 + 1,
                samples_per_chunk: n,
                sample_description_index: 1,
            });
        }
    }
    entries
}

/// The sound's track, as track 2, its frames in chunks at `chunks` run by
/// `stsc`, shown for `movie_length` milliseconds at most.
fn sound_trak(
    audio: &AudioTrack,
    chunks: Vec<u64>,
    stsc: Vec<StscEntry>,
    movie_length: u64,
) -> Trak {
    let rate = u64::from(audio.sample_rate.max(1));
    let media = audio.frames.len() as u64 * u64::from(audio.frame_samples);
    // What is heard: from the end of the priming to the end of the sound,
    // the pictures' end at the latest.
    let heard = audio
        .length
        .min(media.saturating_sub(u64::from(audio.priming)));
    let shown = ((heard * 1000) / rate).min(movie_length);
    Trak {
        tkhd: Tkhd {
            track_id: 2,
            duration: shown,
            enabled: true,
            in_movie: true,
            alternate_group: 1,
            volume: 1.into(),
            ..Default::default()
        },
        edts: Some(Edts {
            elst: Some(Elst {
                entries: vec![ElstEntry {
                    segment_duration: shown,
                    media_time: Some(u64::from(audio.priming)),
                    media_rate: 1.into(),
                }],
            }),
        }),
        mdia: Mdia {
            mdhd: Mdhd {
                timescale: audio.sample_rate,
                duration: media,
                language: "und".into(),
                ..Default::default()
            },
            hdlr: Hdlr {
                handler: FourCC::new(b"soun"),
                name: "SoundHandler".into(),
            },
            minf: Minf {
                smhd: Some(Smhd::default()),
                dinf: Dinf {
                    dref: Dref {
                        urls: vec![Url {
                            location: "".into(),
                        }],
                    },
                },
                stbl: Stbl {
                    stsd: Stsd {
                        codecs: vec![audio.entry.clone()],
                    },
                    stts: Stts {
                        entries: vec![SttsEntry {
                            sample_count: audio.frames.len() as u32,
                            sample_delta: audio.frame_samples,
                        }],
                    },
                    stsc: Stsc { entries: stsc },
                    stsz: Stsz {
                        samples: StszSamples::Different {
                            sizes: audio.frames.iter().map(|f| f.len() as u32).collect(),
                        },
                    },
                    co64: Some(Co64 { entries: chunks }),
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        ..Default::default()
    }
}

/// The film at `film` with `audio` beside its pictures, written at `out`
/// (see the module). The pictures' samples are copied as they are.
pub fn with_audio(film: &Path, audio: &AudioTrack, out: &Path) -> Result<Written, Mp4Error> {
    if !matches!(audio.entry, Codec::Mp4a(_)) {
        return Err(Mp4Error::Atom("a sound that is not AAC".into()));
    }
    if audio.frames.is_empty() {
        return Err(Mp4Error::Atom("a sound of no frame".into()));
    }
    let mut source = File::open(film).map_err(io)?;
    let track = read_track(&mut source)?
        .ok_or_else(|| Mp4Error::Atom(format!("{}: no video track", film.display())))?;
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

    // When each sample starts, in seconds of its own media.
    let mut t = 0u64;
    let video_starts: Vec<f64> = track
        .samples
        .iter()
        .map(|s| {
            let at = t as f64 / f64::from(track.timescale.max(1));
            t += u64::from(s.duration);
            at
        })
        .collect();
    let rate = f64::from(audio.sample_rate.max(1));
    let audio_starts: Vec<f64> = (0..audio.frames.len())
        .map(|k| {
            (k as f64 * f64::from(audio.frame_samples) - f64::from(audio.priming)).max(0.0) / rate
        })
        .collect();
    let chunks = interleave(&video_starts, &audio_starts);
    let sample_bytes = |track: usize, first: usize, count: usize| -> u64 {
        match track {
            0 => table.sizes[first..first + count]
                .iter()
                .map(|&s| u64::from(s))
                .sum(),
            _ => audio.frames[first..first + count]
                .iter()
                .map(|f| f.len() as u64)
                .sum(),
        }
    };
    let counts = |which: usize| -> Vec<u32> {
        chunks
            .iter()
            .filter(|c| c.0 == which)
            .map(|c| c.2 as u32)
            .collect()
    };
    let (movie, _) = table.lengths();
    let moov = |offsets: &[u64]| -> Moov {
        let of = |which: usize| -> Vec<u64> {
            chunks
                .iter()
                .zip(offsets)
                .filter(|(c, _)| c.0 == which)
                .map(|(_, &o)| o)
                .collect()
        };
        Moov {
            mvhd: Mvhd {
                timescale: 1000,
                duration: movie,
                rate: 1.into(),
                volume: 1.into(),
                next_track_id: 3,
                ..Default::default()
            },
            trak: vec![
                table.trak(1, of(0), stsc_of(&counts(0))),
                sound_trak(audio, of(1), stsc_of(&counts(1)), movie),
            ],
            ..Default::default()
        }
    };

    // The `moov`'s size does not depend on the offsets (64-bit entries
    // either way): measured with zeros, then written with the real ones.
    let mut head = Vec::new();
    ftyp(&table.entry).encode(&mut head).map_err(atom)?;
    let mut probe = Vec::new();
    moov(&vec![0; chunks.len()])
        .encode(&mut probe)
        .map_err(atom)?;
    let mut at = (head.len() + probe.len() + 16) as u64;
    let mut offsets = Vec::with_capacity(chunks.len());
    for &(which, first, count) in &chunks {
        offsets.push(at);
        at += sample_bytes(which, first, count);
    }
    let payload = at - (head.len() + probe.len() + 16) as u64;
    moov(&offsets).encode(&mut head).map_err(atom)?;
    head.extend(1u32.to_be_bytes());
    head.extend(b"mdat");
    head.extend((payload + 16).to_be_bytes());

    let mut writer = BufWriter::new(File::create(out).map_err(io)?);
    writer.write_all(&head).map_err(io)?;
    let mut buffer = Vec::new();
    for &(which, first, count) in &chunks {
        if which == 0 {
            for s in &track.samples[first..first + count] {
                buffer.resize(s.size as usize, 0);
                source.seek(SeekFrom::Start(s.offset)).map_err(io)?;
                source.read_exact(&mut buffer).map_err(io)?;
                writer.write_all(&buffer).map_err(io)?;
            }
        } else {
            for frame in &audio.frames[first..first + count] {
                writer.write_all(frame).map_err(io)?;
            }
        }
    }
    writer.flush().map_err(io)?;
    Ok(Written {
        frames: track.samples.len() as u64,
        bytes: head.len() as u64 + payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::film::{avc1, FilmWriter};
    use mp4_atom::Decode;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];
    /// AAC-LC, 48 kHz, two channels.
    const ASC: [u8; 2] = [0x11, 0x90];

    /// A film of `frames` pictures (a multiple of three) at 30 fps, each
    /// sample tagged by its index, decoded I P B and shown I B P.
    fn film(dir: &Path, frames: u64) -> std::path::PathBuf {
        let path = dir.join("film.mp4");
        let mut w = FilmWriter::create(&path, avc1(64, 48, SPS, PPS).unwrap(), 30).unwrap();
        for i in 0..frames {
            // Each three decoded I P B and shown I B P: the P a frame later,
            // the B a frame earlier.
            let shown = match i % 3 {
                0 => i,
                1 => i + 1,
                _ => i - 1,
            };
            w.push(&[0, 0, 0, 2, 0x65, i as u8], i % 30 == 0, shown)
                .unwrap();
        }
        w.finish().unwrap();
        path
    }

    fn sound(frames: usize, priming: u32, length: u64) -> AudioTrack {
        AudioTrack {
            entry: aac_entry(&ASC, 2, 48_000, 256_000).unwrap(),
            sample_rate: 48_000,
            frame_samples: 1024,
            frames: (0..frames)
                .map(|k| vec![0xa0, (k % 256) as u8, (k / 256) as u8])
                .collect(),
            priming,
            length,
        }
    }

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

    /// Where each sample of `trak` lies in the file, in order.
    fn offsets(trak: &Trak) -> Vec<u64> {
        let stbl = &trak.mdia.minf.stbl;
        let sizes = match &stbl.stsz.samples {
            StszSamples::Different { sizes } => sizes.clone(),
            StszSamples::Identical { count, size } => vec![*size; *count as usize],
        };
        let chunks = &stbl.co64.as_ref().expect("co64").entries;
        let mut out = Vec::new();
        for (c, &offset) in chunks.iter().enumerate() {
            let chunk = c as u32 + 1;
            let per = stbl
                .stsc
                .entries
                .iter()
                .rev()
                .find(|r| r.first_chunk <= chunk)
                .unwrap()
                .samples_per_chunk;
            let mut at = offset;
            for _ in 0..per {
                let n = out.len();
                out.push(at);
                at += u64::from(sizes[n]);
            }
        }
        out
    }

    /// Ninety pictures (3 s) and 3.5 s of sound: one file, faststart, the
    /// pictures as they were and in their order, every frame of sound at
    /// its place, the two interleaved second by second, the priming hidden
    /// and the sound cut at the pictures' end.
    #[test]
    fn the_sound_is_laid_beside_the_pictures_without_touching_them() {
        let dir = tempfile::tempdir().unwrap();
        let film = film(dir.path(), 90);
        let out = dir.path().join("with-sound.mp4");
        // 3.5 s of sound after 2048 samples of priming: 167 frames.
        let length = 168_000;
        let frames = (length as usize + 2048).div_ceil(1024);
        let written = with_audio(&film, &sound(frames, 2048, length), &out).expect("muxed");
        assert_eq!(written.frames, 90);
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(bytes.len() as u64, written.bytes);
        let (kinds, moov) = moov_of(&bytes);
        assert_eq!(kinds, [*b"ftyp", *b"moov", *b"mdat"], "faststart");
        assert_eq!(moov.trak.len(), 2);
        let (video, sound) = (&moov.trak[0], &moov.trak[1]);
        assert_eq!(sound.mdia.hdlr.handler, FourCC::new(b"soun"));

        // The pictures: the same samples, the same order of showing.
        let before = read_track(&mut File::open(&film).unwrap())
            .unwrap()
            .unwrap();
        let after = read_track(&mut File::open(&out).unwrap()).unwrap().unwrap();
        let tags: Vec<u8> = after
            .samples
            .iter()
            .map(|s| bytes[s.offset as usize + 5])
            .collect();
        assert_eq!(tags, (0..90).collect::<Vec<u8>>());
        let shown = |t: &crate::film::Track| -> Vec<(i64, bool)> {
            t.samples.iter().map(|s| (s.composition, s.key)).collect()
        };
        assert_eq!(shown(&after), shown(&before));
        assert_eq!(after.start, before.start);

        // The sound: every frame where the tables say.
        let at = offsets(sound);
        assert_eq!(at.len(), frames);
        for (k, &o) in at.iter().enumerate() {
            let o = o as usize;
            assert_eq!(
                bytes[o..o + 3],
                [0xa0, (k % 256) as u8, (k / 256) as u8],
                "frame {k}"
            );
        }
        // Interleaved: the sound's first second before the pictures' second.
        let video_at = offsets(video);
        assert!(
            at[0] < video_at[30] && video_at[29] < at[0],
            "a second of each in turn"
        );
        let chunks = |t: &Trak| t.mdia.minf.stbl.co64.as_ref().unwrap().entries.len();
        assert_eq!((chunks(video), chunks(sound)), (3, 4));

        // Heard from the end of the priming, never past the film's 3 s.
        let elst = &sound.edts.as_ref().unwrap().elst.as_ref().unwrap().entries[0];
        assert_eq!(elst.media_time, Some(2048));
        assert_eq!(elst.segment_duration, 3000, "cut at the pictures' end");
        assert_eq!(moov.mvhd.duration, 3000);
        let Codec::Mp4a(entry) = &sound.mdia.minf.stbl.stsd.codecs[0] else {
            panic!("mp4a")
        };
        let config = entry.esds.es_desc.dec_config.dec_specific.as_ref().unwrap();
        assert_eq!(
            (config.profile, config.freq_index, config.chan_conf),
            (2, 3, 2)
        );
        assert_eq!(entry.audio.channel_count, 2);

        // Another reader of mp4 finds both tracks and the same sound.
        let size = bytes.len() as u64;
        let mut r = mp4::Mp4Reader::read_header(std::io::Cursor::new(bytes), size).expect("mp4");
        assert_eq!(r.tracks().len(), 2);
        let id = r
            .tracks()
            .iter()
            .find(|(_, t)| t.track_type().ok() == Some(mp4::TrackType::Audio))
            .map(|(&id, _)| id)
            .expect("an audio track");
        assert_eq!(r.sample_count(id).unwrap() as usize, frames);
        let s = r.read_sample(id, 7).unwrap().unwrap();
        assert_eq!(s.bytes.as_ref(), [0xa0, 6, 0]);
    }

    /// A sound shorter than the film is heard for as long as it lasts.
    #[test]
    fn a_short_sound_ends_where_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let film = film(dir.path(), 90);
        let out = dir.path().join("short.mp4");
        // One second of sound, no priming.
        with_audio(&film, &sound(47, 0, 48_000), &out).unwrap();
        let (_, moov) = moov_of(&std::fs::read(&out).unwrap());
        let elst = &moov.trak[1]
            .edts
            .as_ref()
            .unwrap()
            .elst
            .as_ref()
            .unwrap()
            .entries[0];
        assert_eq!(elst.segment_duration, 1000);
        assert_eq!(moov.mvhd.duration, 3000, "the film's length is the film's");
    }

    /// What is not AAC, or no sound at all, is refused.
    #[test]
    fn only_aac_frames_are_laid_under_a_film() {
        let dir = tempfile::tempdir().unwrap();
        let film = film(dir.path(), 3);
        let out = dir.path().join("x.mp4");
        let mut not_aac = sound(3, 0, 3072);
        not_aac.entry = avc1(64, 48, SPS, PPS).unwrap();
        assert!(with_audio(&film, &not_aac, &out).is_err());
        assert!(with_audio(&film, &sound(0, 0, 0), &out).is_err());
    }

    #[test]
    fn chunks_run_second_by_second_pictures_first() {
        let video: Vec<f64> = (0..5).map(|i| f64::from(i) * 0.5).collect();
        let audio: Vec<f64> = (0..3).map(f64::from).collect();
        assert_eq!(
            interleave(&video, &audio),
            [
                (0, 0, 2),
                (1, 0, 1),
                (0, 2, 2),
                (1, 1, 1),
                (0, 4, 1),
                (1, 2, 1)
            ]
        );
        assert_eq!(
            stsc_of(&[2, 2, 1]),
            [
                StscEntry {
                    first_chunk: 1,
                    samples_per_chunk: 2,
                    sample_description_index: 1
                },
                StscEntry {
                    first_chunk: 3,
                    samples_per_chunk: 1,
                    sample_description_index: 1
                },
            ]
        );
    }
}
