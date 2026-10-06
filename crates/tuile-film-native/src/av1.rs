// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What an mp4 needs to know of an AV1 stream that an encoder does not
//! hand over: a hardware encoder gives temporal units, and the track's
//! `av1C` record has to be made from the sequence header in the first.

/// Reads bits of an AV1 sequence header, most significant first.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn take(&mut self, count: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = *self.bytes.get(self.at / 8)?;
            value = value << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(value)
    }
}

/// The OBUs of a temporal unit whose OBUs carry their sizes: type, and the
/// whole OBU with its header.
pub fn obus(unit: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < unit.len() {
        let header = unit[at];
        let (extension, has_size) = (header >> 2 & 1 == 1, header >> 1 & 1 == 1);
        let mut body = at + 1 + usize::from(extension);
        if !has_size {
            // Without a size it runs to the end.
            out.push((header >> 3 & 0xf, &unit[at..]));
            break;
        }
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let Some(byte) = unit.get(body) else {
                return out;
            };
            size |= usize::from(byte & 0x7f) << shift;
            body += 1;
            shift += 7;
            if byte & 0x80 == 0 || shift > 56 {
                break;
            }
        }
        let end = (body + size).min(unit.len());
        out.push((header >> 3 & 0xf, &unit[at..end]));
        at = end;
    }
    out
}

/// The `av1C` record of a stream, from its sequence header OBU (ISO/IEC
/// 14496-15 AV1 binding, §2.3): marker and version, profile and level,
/// tier and chroma layout, then the sequence header itself.
///
/// What the render gives the encoder is 8-bit 4:2:0, so the chroma layout
/// is known; profile, level and tier are read from the header.
pub fn av1c_of(sequence_header: &[u8]) -> Option<Vec<u8>> {
    let header = *sequence_header.first()?;
    let mut body = 1 + usize::from(header >> 2 & 1 == 1);
    if header >> 1 & 1 == 1 {
        // Past the size, a LEB128.
        while *sequence_header.get(body)? & 0x80 != 0 {
            body += 1;
        }
        body += 1;
    }
    let mut bits = Bits {
        bytes: sequence_header.get(body..)?,
        at: 0,
    };
    let profile = bits.take(3)?;
    let _still_picture = bits.take(1)?;
    let (level, tier) = if bits.take(1)? == 1 {
        // A reduced header: the level, and nothing else.
        (bits.take(5)?, 0)
    } else {
        if bits.take(1)? == 1 {
            // Timing info: two 32-bit counts, then perhaps a picture
            // interval, then whether a decoder model follows — which an
            // encoder of films does not write, and is not read here.
            bits.take(32)?;
            bits.take(32)?;
            if bits.take(1)? == 1 {
                let mut zeros = 0;
                while bits.take(1)? == 0 {
                    zeros += 1;
                }
                bits.take(zeros)?;
            }
            if bits.take(1)? == 1 {
                return None;
            }
        }
        let _initial_display_delay_present = bits.take(1)?;
        let _operating_points_minus_1 = bits.take(5)?;
        let _operating_point_idc = bits.take(12)?;
        let level = bits.take(5)?;
        let tier = if level > 7 { bits.take(1)? } else { 0 };
        (level, tier)
    };
    let mut record = vec![
        0x81,
        (profile << 5 | level) as u8,
        // Tier; 8 bits a sample, in colour; chroma halved both ways.
        (tier << 7) as u8 | 0b0000_1100,
        0,
    ];
    record.extend_from_slice(sequence_header);
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_av1c_record_says_what_its_sequence_header_says() {
        // OBU header: type 1 (sequence header), with a size. Then: profile
        // 0, not still, not reduced, no timing info, no display delay, one
        // operating point of idc 0, level 13 (5.1), tier 1.
        //   000 0 0 0 0 00000 | 000000000000 | 01101 | 1 …
        let bits =
            "000".to_string() + "0" + "0" + "0" + "0" + "00000" + "000000000000" + "01101" + "1";
        let mut body = Vec::new();
        for chunk in format!("{bits:0<32}").as_bytes().chunks(8) {
            body.push(
                u8::from_str_radix(std::str::from_utf8(chunk).expect("bits"), 2).expect("a byte"),
            );
        }
        let mut obu = vec![0b0000_1010, body.len() as u8];
        obu.extend(&body);
        let record = av1c_of(&obu).expect("a record");
        assert_eq!(record[0], 0x81);
        assert_eq!(record[1], 13);
        assert_eq!(record[2], 0b1000_1100);
        assert_eq!(&record[4..], obu.as_slice());
        // Too short to say: nothing, not a guess.
        assert_eq!(av1c_of(&[0b0000_1010, 1, 0]), None);
    }

    #[test]
    fn a_temporal_unit_is_cut_into_its_obus() {
        // A temporal delimiter (type 2, size 0), a sequence header of two
        // bytes, a frame of three.
        let unit = [0x12, 0, 0x0a, 2, 9, 9, 0x32, 3, 7, 7, 7];
        let cut = obus(&unit);
        assert_eq!(cut.len(), 3);
        assert_eq!(cut[0], (2, &unit[0..2]));
        assert_eq!(cut[1], (1, &unit[2..6]));
        assert_eq!(cut[2], (6, &unit[6..11]));
    }
}
