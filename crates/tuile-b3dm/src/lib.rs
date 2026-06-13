// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-b3dm
//!
//! Codec for the Batched 3D Model container (b3dm, 3D Tiles 1.0): a 28-byte
//! header, feature/batch tables, and an embedded binary glTF.
//!
//! This crate only handles the *container*: [`parse`] hands back table
//! slices and the glb payload without copying; [`encode`] builds a valid
//! b3dm around a glb (handy for tests and tilers). Decoding the embedded
//! glTF is the caller's business.
//!
//! No I/O, no unsafe, wasm-clean.

use serde_json::Value;

pub const HEADER_LEN: usize = 28;
pub const MAGIC: &[u8; 4] = b"b3dm";

#[derive(Debug, thiserror::Error)]
pub enum B3dmError {
    #[error("not a b3dm payload (bad magic)")]
    BadMagic,
    #[error("unsupported b3dm version {0} (expected 1)")]
    UnsupportedVersion(u32),
    #[error("truncated or inconsistent b3dm: {0}")]
    Truncated(&'static str),
    #[error("feature table JSON: {0}")]
    FeatureTable(#[from] serde_json::Error),
    #[error("binary-referenced RTC_CENTER is not supported")]
    BinaryRtcCenter,
}

/// A parsed b3dm: borrowed views into the original payload.
#[derive(Debug, Clone, Copy)]
pub struct B3dm<'a> {
    pub feature_table_json: &'a [u8],
    pub feature_table_binary: &'a [u8],
    pub batch_table_json: &'a [u8],
    pub batch_table_binary: &'a [u8],
    /// The embedded binary glTF.
    pub glb: &'a [u8],
}

/// Parses a b3dm container. Zero-copy: the result borrows `bytes`.
pub fn parse(bytes: &[u8]) -> Result<B3dm<'_>, B3dmError> {
    if bytes.len() < HEADER_LEN {
        return Err(B3dmError::Truncated("header"));
    }
    if &bytes[0..4] != MAGIC {
        return Err(B3dmError::BadMagic);
    }
    let u32_at = |o: usize| -> u32 {
        let mut b = [0u8; 4];
        b.copy_from_slice(&bytes[o..o + 4]);
        u32::from_le_bytes(b)
    };
    let version = u32_at(4);
    if version != 1 {
        return Err(B3dmError::UnsupportedVersion(version));
    }
    let byte_length = u32_at(8) as usize;
    let ft_json = u32_at(12) as usize;
    let ft_bin = u32_at(16) as usize;
    let bt_json = u32_at(20) as usize;
    let bt_bin = u32_at(24) as usize;

    let glb_start = HEADER_LEN
        .checked_add(ft_json)
        .and_then(|v| v.checked_add(ft_bin))
        .and_then(|v| v.checked_add(bt_json))
        .and_then(|v| v.checked_add(bt_bin))
        .ok_or(B3dmError::Truncated("table lengths overflow"))?;
    if byte_length > bytes.len() || glb_start > byte_length {
        return Err(B3dmError::Truncated("table lengths"));
    }

    let mut offset = HEADER_LEN;
    let mut take = |len: usize| {
        let s = &bytes[offset..offset + len];
        offset += len;
        s
    };
    Ok(B3dm {
        feature_table_json: take(ft_json),
        feature_table_binary: take(ft_bin),
        batch_table_json: take(bt_json),
        batch_table_binary: take(bt_bin),
        glb: &bytes[glb_start..byte_length],
    })
}

impl B3dm<'_> {
    /// The `RTC_CENTER` translation from the feature table, if any.
    ///
    /// Only the inline-array form is supported; the binary-referenced form
    /// (`{ "byteOffset": … }`) yields a typed error.
    pub fn rtc_center(&self) -> Result<Option<[f64; 3]>, B3dmError> {
        if self.feature_table_json.is_empty() {
            return Ok(None);
        }
        let ft: Value = serde_json::from_slice(self.feature_table_json)?;
        let Some(c) = ft.get("RTC_CENTER") else {
            return Ok(None);
        };
        let arr: Option<Vec<f64>> = c
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_f64).collect());
        match arr.as_deref() {
            Some([x, y, z]) => Ok(Some([*x, *y, *z])),
            _ => Err(B3dmError::BinaryRtcCenter),
        }
    }
}

/// Builds a b3dm container around a glb. The feature table JSON is padded
/// to the spec's 8-byte alignment.
pub fn encode(glb: &[u8], feature_table_json: &str) -> Vec<u8> {
    let mut ft = feature_table_json.as_bytes().to_vec();
    while !ft.len().is_multiple_of(8) {
        ft.push(b' ');
    }
    let total = HEADER_LEN + ft.len() + glb.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(ft.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&ft);
    out.extend_from_slice(glb);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_parse_round_trip() {
        let glb = b"glTF-fake-payload";
        let b = encode(glb, r#"{"BATCH_LENGTH":0,"RTC_CENTER":[1.5,2.5,3.5]}"#);
        let parsed = parse(&b).expect("parse");
        assert_eq!(parsed.glb, glb);
        assert_eq!(parsed.rtc_center().expect("rtc"), Some([1.5, 2.5, 3.5]));
        assert!(parsed.batch_table_json.is_empty());
    }

    #[test]
    fn no_feature_table_means_no_rtc() {
        let b = encode(b"glb", "");
        let parsed = parse(&b).expect("parse");
        assert_eq!(parsed.rtc_center().expect("rtc"), None);
    }

    #[test]
    fn bad_magic_and_version() {
        assert!(matches!(
            parse(b"nope4567890123456789012345678"),
            Err(B3dmError::BadMagic)
        ));
        let mut b = encode(b"glb", "");
        b[4] = 2;
        assert!(matches!(parse(&b), Err(B3dmError::UnsupportedVersion(2))));
    }

    #[test]
    fn truncations_are_typed_errors() {
        assert!(matches!(parse(b"b3dm"), Err(B3dmError::Truncated(_))));
        let mut b = encode(b"glb-payload", r#"{"BATCH_LENGTH":0}"#);
        // Corrupt the feature-table length so tables overflow the payload.
        b[12..16].copy_from_slice(&9999u32.to_le_bytes());
        assert!(matches!(parse(&b), Err(B3dmError::Truncated(_))));
    }

    #[test]
    fn binary_rtc_center_is_rejected() {
        let b = encode(b"glb", r#"{"RTC_CENTER":{"byteOffset":0}}"#);
        let parsed = parse(&b).expect("parse");
        assert!(matches!(
            parsed.rtc_center(),
            Err(B3dmError::BinaryRtcCenter)
        ));
    }
}
