// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! AV1 in an mp4, written box by box.
//!
//! The container crate this muxer uses for H.264 has no AV1 sample entry, and
//! what a film needs is small: one video track, a constant frame rate, no
//! reordering. So the boxes are written here — ISO/IEC 14496-12 for the file,
//! the AV1 ISOBMFF binding for the `av01` entry and its `av1C` record.
//!
//! Layout: `ftyp`, one `mdat` holding every sample back to back, then `moov`.
//! The samples are a single chunk, so one offset places them all.

use crate::Mp4Error;

/// The temporal delimiter OBU an encoder puts at the head of each temporal
/// unit: type 2, with a size field of zero. The binding says a sample should
/// not carry it.
const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

pub(crate) struct Av1Writer {
    width: u16,
    height: u16,
    timescale: u32,
    ticks: u32,
    config: Vec<u8>,
    data: Vec<u8>,
    sizes: Vec<u32>,
    sync: Vec<u32>,
}

/// A box: its size, its four letters, its body.
fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend(((body.len() + 8) as u32).to_be_bytes());
    out.extend(kind);
    out.extend(body);
    out
}

/// A full box: version and flags ahead of the body.
fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(body.len() + 4);
    inner.push(version);
    inner.extend(&flags.to_be_bytes()[1..]);
    inner.extend(body);
    boxed(kind, &inner)
}

fn be32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// The identity transform every header carries.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

impl Av1Writer {
    /// `config` is the `av1C` record: the four bytes an encoder derives from
    /// its sequence header, optionally followed by config OBUs.
    pub(crate) fn new(
        width: u16,
        height: u16,
        fps: u32,
        ticks: u32,
        config: &[u8],
    ) -> Result<Self, Mp4Error> {
        // marker 1, version 1.
        if config.len() < 4 || config[0] != 0x81 {
            return Err(Mp4Error::BadAv1c("not an av1C record"));
        }
        Ok(Self {
            width,
            height,
            timescale: fps * ticks,
            ticks,
            config: config.to_vec(),
            data: Vec::new(),
            sizes: Vec::new(),
            sync: Vec::new(),
        })
    }

    pub(crate) fn config(&self) -> &[u8] {
        &self.config
    }

    pub(crate) fn push(&mut self, unit: &[u8], key: bool) {
        let sample = unit.strip_prefix(&TEMPORAL_DELIMITER).unwrap_or(unit);
        self.data.extend_from_slice(sample);
        self.sizes.push(sample.len() as u32);
        if key {
            self.sync.push(self.sizes.len() as u32);
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        let frames = self.sizes.len() as u32;
        let media_duration = frames * self.ticks;
        // The movie header counts in milliseconds.
        let movie_duration =
            (u64::from(media_duration) * 1000 / u64::from(self.timescale.max(1))) as u32;

        let ftyp = boxed(
            b"ftyp",
            &[
                b"isom".as_slice(),
                &512u32.to_be_bytes(),
                b"isomiso2av01mp41",
            ]
            .concat(),
        );
        // A 64-bit size: a long film at full size passes four gigabytes.
        let mdat_header = [
            &1u32.to_be_bytes()[..],
            b"mdat",
            &(self.data.len() as u64 + 16).to_be_bytes(),
        ]
        .concat();
        let first_sample = (ftyp.len() + mdat_header.len()) as u64;

        let mut entry = Vec::new();
        entry.extend([0u8; 6]); // reserved
        entry.extend(1u16.to_be_bytes()); // data reference index
        entry.extend([0u8; 16]); // pre-defined, reserved
        entry.extend(self.width.to_be_bytes());
        entry.extend(self.height.to_be_bytes());
        entry.extend(be32(&[0x0048_0000, 0x0048_0000, 0])); // 72 dpi, reserved
        entry.extend(1u16.to_be_bytes()); // frames per sample
        entry.extend([0u8; 32]); // compressor name
        entry.extend(0x0018u16.to_be_bytes()); // depth
        entry.extend(0xffffu16.to_be_bytes()); // pre-defined
        entry.extend(boxed(b"av1C", &self.config));

        let stbl = [
            full(
                b"stsd",
                0,
                0,
                &[&1u32.to_be_bytes()[..], &boxed(b"av01", &entry)].concat(),
            ),
            full(b"stts", 0, 0, &be32(&[1, frames, self.ticks])),
            full(b"stsc", 0, 0, &be32(&[1, 1, frames, 1])),
            full(
                b"stsz",
                0,
                0,
                &[be32(&[0, frames]), be32(&self.sizes)].concat(),
            ),
            full(
                b"co64",
                0,
                0,
                &[&1u32.to_be_bytes()[..], &first_sample.to_be_bytes()].concat(),
            ),
            full(
                b"stss",
                0,
                0,
                &[be32(&[self.sync.len() as u32]), be32(&self.sync)].concat(),
            ),
        ]
        .concat();
        let minf = [
            full(b"vmhd", 0, 1, &[0u8; 8]),
            boxed(
                b"dinf",
                &full(
                    b"dref",
                    0,
                    0,
                    &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat(),
                ),
            ),
            boxed(b"stbl", &stbl),
        ]
        .concat();
        let mdia = [
            // Language 'und', packed five bits a letter.
            full(
                b"mdhd",
                0,
                0,
                &[
                    be32(&[0, 0, self.timescale, media_duration]),
                    vec![0x55, 0xc4, 0, 0],
                ]
                .concat(),
            ),
            full(
                b"hdlr",
                0,
                0,
                &[&[0u8; 4][..], b"vide", &[0u8; 12], b"VideoHandler\0"].concat(),
            ),
            boxed(b"minf", &minf),
        ]
        .concat();
        let tkhd = [
            be32(&[0, 0, 1, 0, movie_duration, 0, 0]), // times, track 1, reserved, duration, reserved
            vec![0u8; 8],                              // layer, group, volume, reserved
            be32(&MATRIX),
            be32(&[u32::from(self.width) << 16, u32::from(self.height) << 16]),
        ]
        .concat();
        let trak = [full(b"tkhd", 0, 3, &tkhd), boxed(b"mdia", &mdia)].concat();
        let mvhd = [
            be32(&[0, 0, 1000, movie_duration, 0x0001_0000]), // times, timescale, duration, rate
            vec![0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],   // volume, reserved
            be32(&MATRIX),
            vec![0u8; 24],
            be32(&[2]), // next track id
        ]
        .concat();
        let moov = boxed(
            b"moov",
            &[full(b"mvhd", 0, 0, &mvhd), boxed(b"trak", &trak)].concat(),
        );

        [ftyp, mdat_header, self.data, moov].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks the boxes at one level: (kind, body).
    fn boxes(mut bytes: &[u8]) -> Vec<(String, &[u8])> {
        let mut out = Vec::new();
        while bytes.len() >= 8 {
            let size32 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            let kind = String::from_utf8_lossy(&bytes[4..8]).to_string();
            let (header, size) = match size32 {
                1 => (
                    16,
                    u64::from_be_bytes(bytes[8..16].try_into().expect("largesize")) as usize,
                ),
                n => (8, n),
            };
            out.push((kind, &bytes[header..size]));
            bytes = &bytes[size..];
        }
        out
    }

    fn find<'a>(bytes: &'a [u8], path: &[&str]) -> &'a [u8] {
        let mut at = bytes;
        for kind in path {
            at = boxes(at)
                .into_iter()
                .find(|(k, _)| k == kind)
                .unwrap_or_else(|| panic!("no {kind} box"))
                .1;
        }
        at
    }

    const STBL: [&str; 4] = ["moov", "trak", "mdia", "minf"];

    #[test]
    fn the_file_holds_its_samples_where_its_tables_say() {
        let mut w = Av1Writer::new(640, 360, 30, 512, &[0x81, 0x05, 0x0c, 0x00]).expect("writer");
        w.push(&[0x12, 0x00, 0x0a, 0x01, 0xaa], true); // delimiter stripped
        w.push(&[0x32, 0x02, 0xbb, 0xcc], false);
        w.push(&[0x32, 0x01, 0xdd], true);
        let file = w.finish();

        let top: Vec<String> = boxes(&file).into_iter().map(|(k, _)| k).collect();
        assert_eq!(top, ["ftyp", "mdat", "moov"]);
        assert_eq!(
            find(&file, &["mdat"]),
            [0x0a, 0x01, 0xaa, 0x32, 0x02, 0xbb, 0xcc, 0x32, 0x01, 0xdd]
        );

        let stbl = find(find(&file, &STBL), &["stbl"]);
        let u32_at =
            |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().expect("u32"));
        let stsz = find(stbl, &["stsz"]);
        assert_eq!(
            (
                u32_at(stsz, 8),
                u32_at(stsz, 12),
                u32_at(stsz, 16),
                u32_at(stsz, 20)
            ),
            (3, 3, 4, 3)
        );
        let stss = find(stbl, &["stss"]);
        assert_eq!(
            (u32_at(stss, 4), u32_at(stss, 8), u32_at(stss, 12)),
            (2, 1, 3)
        );
        // The one chunk offset points at the first sample's first byte.
        let co64 = find(stbl, &["co64"]);
        let offset = u64::from_be_bytes(co64[8..16].try_into().expect("offset")) as usize;
        assert_eq!(&file[offset..offset + 3], [0x0a, 0x01, 0xaa]);
        // 3 frames of 512 ticks at 30 × 512 per second.
        let stts = find(stbl, &["stts"]);
        assert_eq!((u32_at(stts, 8), u32_at(stts, 12)), (3, 512));
        let mdhd = find(&file, &["moov", "trak", "mdia", "mdhd"]);
        assert_eq!((u32_at(mdhd, 12), u32_at(mdhd, 16)), (30 * 512, 3 * 512));

        // The sample entry is av01, at the film's size, carrying the record.
        let stsd = find(stbl, &["stsd"]);
        let (kind, entry) = boxes(&stsd[8..]).remove(0);
        assert_eq!(kind, "av01");
        assert_eq!(
            (&entry[24..26], &entry[26..28]),
            (&640u16.to_be_bytes()[..], &360u16.to_be_bytes()[..])
        );
        assert_eq!(find(&entry[78..], &["av1C"]), [0x81, 0x05, 0x0c, 0x00]);
    }

    #[test]
    fn a_record_that_is_not_one_is_refused() {
        assert!(Av1Writer::new(64, 64, 30, 512, &[0x01, 0x64, 0x00]).is_err());
        assert!(Av1Writer::new(64, 64, 30, 512, &[]).is_err());
    }
}
