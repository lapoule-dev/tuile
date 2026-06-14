// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! quantized-mesh-1.0 decoding (and a minimal encoder for tests).
//!
//! Binary layout (little-endian), modelled on cesium-native's
//! `QuantizedMeshLoader`:
//! - header, 88 bytes: center ECEF (3×f64), min/max height (2×f32),
//!   bounding sphere center+radius (4×f64), horizon occlusion (3×f64)
//! - `vertex_count: u32`, then u / v / height as `u16` arrays, each
//!   zig-zag + delta encoded, normalized by 32767
//! - indices: `triangle_count: u32` then `3·triangle_count` indices
//!   (u16 if `vertex_count ≤ 65536`, else u32 with 4-byte padding before),
//!   high-water-mark encoded
//! - four edge-index arrays (west/south/east/north skirts), each a `u32`
//!   count then that many direct indices
//! - extension records until EOF: `id: u8`, `len: u32`, payload.
//!   id 1 = oct-encoded normals (2 bytes/vertex)
//!
//! The decoder keeps u/v/height **normalized** (0..1) and the raw indices;
//! turning them into ECEF positions needs the tile rectangle and lives in
//! [`crate::mesh`].

const SCALE: f64 = 32767.0;
const EXT_OCT_NORMALS: u8 = 1;
const EXT_METADATA: u8 = 4;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("quantized-mesh truncated reading {0}")]
    Truncated(&'static str),
    #[error("index {idx} out of range (vertex_count {count})")]
    BadIndex { idx: u32, count: u32 },
    #[error("gzip: {0}")]
    Gzip(String),
}

/// Header of a quantized-mesh tile (88 bytes).
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub center: [f64; 3],
    pub min_height: f32,
    pub max_height: f32,
    pub bounding_sphere_center: [f64; 3],
    pub bounding_sphere_radius: f64,
    pub horizon_occlusion: [f64; 3],
}

/// A decoded quantized-mesh tile: normalized vertex coordinates, triangle
/// indices, optional per-vertex oct-decoded normals, and the skirt edges.
#[derive(Debug, Clone)]
pub struct QuantizedMesh {
    pub header: Header,
    /// Per-vertex u in [0,1] (west→east across the tile rectangle).
    pub u: Vec<f64>,
    /// Per-vertex v in [0,1] (south→north).
    pub v: Vec<f64>,
    /// Per-vertex height in [0,1] (min_height→max_height).
    pub height: Vec<f64>,
    /// Triangle indices (3 per triangle).
    pub indices: Vec<u32>,
    /// Unit normals (ECEF) if the octvertexnormals extension was present.
    pub normals: Option<Vec<[f32; 3]>>,
    /// Edge vertex indices, in `[west, south, east, north]` order — the
    /// skirt borders.
    pub edges: [Vec<u32>; 4],
    /// Deeper tile availability from the `metadata` extension, if present:
    /// `metadata_available[offset]` lists tiles existing at level
    /// `this_tile_level + offset + 1`. The driver of multi-level refinement on
    /// Cesium World Terrain.
    pub metadata_available: Option<Vec<Vec<crate::layer::AvailabilityRange>>>,
}

impl QuantizedMesh {
    pub fn vertex_count(&self) -> usize {
        self.u.len()
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(DecodeError::Truncated(what))?;
        let s = self
            .bytes
            .get(self.pos..end)
            .ok_or(DecodeError::Truncated(what))?;
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &'static str) -> Result<u16, DecodeError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn f32(&mut self, what: &'static str) -> Result<f32, DecodeError> {
        let b = self.take(4, what)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn f64(&mut self, what: &'static str) -> Result<f64, DecodeError> {
        let b = self.take(8, what)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f64x3(&mut self, what: &'static str) -> Result<[f64; 3], DecodeError> {
        Ok([self.f64(what)?, self.f64(what)?, self.f64(what)?])
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
}

/// `(value >> 1) ^ (-(value & 1))` — zig-zag decode.
fn zigzag(value: u16) -> i32 {
    let v = value as i32;
    (v >> 1) ^ (-(v & 1))
}

/// Decodes a quantized-mesh tile. Transparently gunzips the input when it
/// is gzip-framed (`.terrain` tiles are gzip on the wire; a transport that
/// already decoded Content-Encoding passes raw bytes — both work).
pub fn decode(bytes: &[u8]) -> Result<QuantizedMesh, DecodeError> {
    let owned;
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| DecodeError::Gzip(e.to_string()))?;
        owned = out;
        owned.as_slice()
    } else {
        bytes
    };
    let mut r = Reader::new(bytes);

    let header = Header {
        center: r.f64x3("center")?,
        min_height: r.f32("minHeight")?,
        max_height: r.f32("maxHeight")?,
        bounding_sphere_center: r.f64x3("bsCenter")?,
        bounding_sphere_radius: r.f64("bsRadius")?,
        horizon_occlusion: r.f64x3("horizon")?,
    };

    let vertex_count = r.u32("vertexCount")? as usize;
    let u = decode_zigzag_delta(&mut r, vertex_count, "u")?;
    let v = decode_zigzag_delta(&mut r, vertex_count, "v")?;
    let height = decode_zigzag_delta(&mut r, vertex_count, "height")?;

    let wide = vertex_count > 65536;
    if wide {
        // 4-byte align before 32-bit index data.
        let pad = (4 - (r.pos % 4)) % 4;
        r.take(pad, "indexPadding")?;
    }

    let triangle_count = r.u32("triangleCount")? as usize;
    let index_count = triangle_count * 3;
    let indices = decode_indices_high_water_mark(&mut r, index_count, wide, vertex_count as u32)?;

    let edges = [
        read_edge(&mut r, wide, "west")?,
        read_edge(&mut r, wide, "south")?,
        read_edge(&mut r, wide, "east")?,
        read_edge(&mut r, wide, "north")?,
    ];

    // Extension records until EOF.
    let mut normals = None;
    let mut metadata_available = None;
    while r.remaining() > 0 {
        let id = r.u8("extId")?;
        let len = r.u32("extLen")? as usize;
        let payload = r.take(len, "extData")?;
        if id == EXT_OCT_NORMALS && payload.len() == vertex_count * 2 {
            normals = Some(decode_oct_normals(payload));
        } else if id == EXT_METADATA {
            metadata_available = decode_metadata_availability(payload);
        }
        // Watermask (id 2) is skipped.
    }

    Ok(QuantizedMesh {
        header,
        u,
        v,
        height,
        indices,
        normals,
        edges,
        metadata_available,
    })
}

/// The `metadata` extension payload: `stringLength: u32` then a JSON document
/// whose `available` field carries the deeper availability. Malformed metadata
/// is non-fatal (returns `None`) — it only costs refinement depth, not the tile.
fn decode_metadata_availability(
    payload: &[u8],
) -> Option<Vec<Vec<crate::layer::AvailabilityRange>>> {
    let len_bytes = payload.get(0..4)?;
    let string_len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]])
        as usize;
    let json = payload.get(4..4 + string_len)?;
    let meta: crate::layer::TileMetadata = serde_json::from_slice(json).ok()?;
    (!meta.available.is_empty()).then_some(meta.available)
}

fn decode_zigzag_delta(
    r: &mut Reader<'_>,
    count: usize,
    what: &'static str,
) -> Result<Vec<f64>, DecodeError> {
    let mut out = Vec::with_capacity(count);
    let mut acc: i32 = 0;
    for _ in 0..count {
        acc = acc.wrapping_add(zigzag(r.u16(what)?));
        out.push(acc as f64 / SCALE);
    }
    Ok(out)
}

/// High-water-mark decode: `decoded = highest - code; if code == 0 { highest += 1 }`.
fn decode_indices_high_water_mark(
    r: &mut Reader<'_>,
    count: usize,
    wide: bool,
    vertex_count: u32,
) -> Result<Vec<u32>, DecodeError> {
    let mut out = Vec::with_capacity(count);
    let mut highest: u32 = 0;
    for _ in 0..count {
        let code = if wide {
            r.u32("index")?
        } else {
            u32::from(r.u16("index")?)
        };
        let idx = highest.wrapping_sub(code);
        if idx >= vertex_count {
            return Err(DecodeError::BadIndex {
                idx,
                count: vertex_count,
            });
        }
        out.push(idx);
        if code == 0 {
            highest += 1;
        }
    }
    Ok(out)
}

/// Edge indices are direct (not high-water-mark encoded).
fn read_edge(r: &mut Reader<'_>, wide: bool, what: &'static str) -> Result<Vec<u32>, DecodeError> {
    let count = r.u32(what)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(if wide {
            r.u32(what)?
        } else {
            u32::from(r.u16(what)?)
        });
    }
    Ok(out)
}

/// `x/255 → snorm [-1,1]`, reconstruct z, fold if negative, normalize.
fn decode_oct_normals(payload: &[u8]) -> Vec<[f32; 3]> {
    payload
        .chunks_exact(2)
        .map(|c| oct_decode(c[0], c[1]))
        .collect()
}

fn oct_decode(x: u8, y: u8) -> [f32; 3] {
    let snorm = |b: u8| (b as f64 / 255.0) * 2.0 - 1.0;
    let sign_not_zero = |v: f64| if v >= 0.0 { 1.0 } else { -1.0 };
    let mut nx = snorm(x);
    let mut ny = snorm(y);
    let nz = 1.0 - (nx.abs() + ny.abs());
    if nz < 0.0 {
        let old_x = nx;
        nx = (1.0 - ny.abs()) * sign_not_zero(old_x);
        ny = (1.0 - old_x.abs()) * sign_not_zero(ny);
    }
    let len = (nx * nx + ny * ny + nz * nz).sqrt();
    [(nx / len) as f32, (ny / len) as f32, (nz / len) as f32]
}

// ---------------------------------------------------------------------------
// Minimal encoder — for round-trip tests (and, later, a terrain tiler).
// ---------------------------------------------------------------------------

/// Builds a quantized-mesh payload from normalized vertex data. Triangle
/// and edge indices are written un-encoded-friendly (high-water-mark for
/// triangles, direct for edges) so [`decode`] round-trips.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    header: &Header,
    u: &[f64],
    v: &[f64],
    height: &[f64],
    indices: &[u32],
    normals: Option<&[[f32; 3]]>,
    edges: &[Vec<u32>; 4],
) -> Vec<u8> {
    let mut out = Vec::new();
    let push_f64 = |o: &mut Vec<u8>, x: f64| o.extend_from_slice(&x.to_le_bytes());
    let push_f32 = |o: &mut Vec<u8>, x: f32| o.extend_from_slice(&x.to_le_bytes());
    let push_u32 = |o: &mut Vec<u8>, x: u32| o.extend_from_slice(&x.to_le_bytes());

    for c in header.center {
        push_f64(&mut out, c);
    }
    push_f32(&mut out, header.min_height);
    push_f32(&mut out, header.max_height);
    for c in header.bounding_sphere_center {
        push_f64(&mut out, c);
    }
    push_f64(&mut out, header.bounding_sphere_radius);
    for c in header.horizon_occlusion {
        push_f64(&mut out, c);
    }

    let vc = u.len();
    push_u32(&mut out, vc as u32);
    encode_zigzag_delta(&mut out, u);
    encode_zigzag_delta(&mut out, v);
    encode_zigzag_delta(&mut out, height);

    let wide = vc > 65536;
    if wide {
        let pad = (4 - (out.len() % 4)) % 4;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    push_u32(&mut out, (indices.len() / 3) as u32);
    encode_indices_high_water_mark(&mut out, indices, wide);

    for edge in edges {
        push_u32(&mut out, edge.len() as u32);
        for &i in edge {
            if wide {
                push_u32(&mut out, i);
            } else {
                out.extend_from_slice(&(i as u16).to_le_bytes());
            }
        }
    }

    if let Some(normals) = normals {
        let payload: Vec<u8> = normals.iter().flat_map(|n| oct_encode(*n)).collect();
        out.push(EXT_OCT_NORMALS);
        push_u32(&mut out, payload.len() as u32);
        out.extend_from_slice(&payload);
    }
    out
}

fn encode_zigzag_delta(out: &mut Vec<u8>, values: &[f64]) {
    let mut prev: i32 = 0;
    for &val in values {
        let q = (val * SCALE).round() as i32;
        let delta = q - prev;
        prev = q;
        let zz = ((delta << 1) ^ (delta >> 31)) as u16;
        out.extend_from_slice(&zz.to_le_bytes());
    }
}

fn encode_indices_high_water_mark(out: &mut Vec<u8>, indices: &[u32], wide: bool) {
    let mut highest: u32 = 0;
    for &idx in indices {
        let code = highest.wrapping_sub(idx);
        if wide {
            out.extend_from_slice(&code.to_le_bytes());
        } else {
            out.extend_from_slice(&(code as u16).to_le_bytes());
        }
        if idx == highest {
            highest += 1;
        }
    }
}

fn oct_encode(n: [f32; 3]) -> [u8; 2] {
    let (x, y, z) = (n[0] as f64, n[1] as f64, n[2] as f64);
    let l1 = x.abs() + y.abs() + z.abs();
    let (mut px, mut py) = (x / l1, y / l1);
    if z < 0.0 {
        let sign = |v: f64| if v >= 0.0 { 1.0 } else { -1.0 };
        let ox = px;
        px = (1.0 - py.abs()) * sign(ox);
        py = (1.0 - ox.abs()) * sign(py);
    }
    let to_u8 = |v: f64| (((v.clamp(-1.0, 1.0) + 1.0) / 2.0) * 255.0).round() as u8;
    [to_u8(px), to_u8(py)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_header() -> Header {
        Header {
            center: [1.0, 2.0, 3.0],
            min_height: -100.0,
            max_height: 900.0,
            bounding_sphere_center: [1.0, 2.0, 3.0],
            bounding_sphere_radius: 1234.5,
            horizon_occlusion: [0.1, 0.2, 0.3],
        }
    }

    #[test]
    fn zigzag_round_trips() {
        for v in [0i32, 1, -1, 2, -2, 1000, -1000, 16383, -16384] {
            let enc = ((v << 1) ^ (v >> 31)) as u16;
            assert_eq!(zigzag(enc), v, "v = {v}");
        }
    }

    #[test]
    fn oct_normals_round_trip_within_tolerance() {
        let cases: [[f32; 3]; 6] = [
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.577, 0.577, 0.577],
            [-0.5, 0.5, -0.707],
        ];
        for n in cases {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            let unit = [n[0] / len, n[1] / len, n[2] / len];
            let [x, y] = oct_encode(unit);
            let back = oct_decode(x, y);
            let dot = unit[0] * back[0] + unit[1] * back[1] + unit[2] * back[2];
            assert!(dot > 0.999, "normal {unit:?} → {back:?}, dot {dot}");
        }
    }

    #[test]
    fn high_water_mark_round_trips() {
        // A small mesh: two triangles sharing an edge.
        let indices = vec![0u32, 1, 2, 2, 1, 3];
        let mut buf = Vec::new();
        encode_indices_high_water_mark(&mut buf, &indices, false);
        let mut r = Reader::new(&buf);
        let decoded =
            decode_indices_high_water_mark(&mut r, indices.len(), false, 4).expect("decode");
        assert_eq!(decoded, indices);
    }

    #[test]
    fn full_round_trip() {
        // A 2×2 grid of vertices, two triangles, with normals and one skirt edge.
        let u = vec![0.0, 1.0, 0.0, 1.0];
        let v = vec![0.0, 0.0, 1.0, 1.0];
        let height = vec![0.0, 0.25, 0.5, 1.0];
        let indices = vec![0u32, 1, 2, 2, 1, 3];
        let normals = vec![[0.0f32, 0.0, 1.0]; 4];
        let edges = [vec![0u32, 2], vec![0u32, 1], vec![1u32, 3], vec![2u32, 3]];

        let bytes = encode(
            &test_header(),
            &u,
            &v,
            &height,
            &indices,
            Some(&normals),
            &edges,
        );
        let m = decode(&bytes).expect("decode");

        assert_eq!(m.vertex_count(), 4);
        assert_eq!(m.indices, indices);
        for i in 0..4 {
            assert!((m.u[i] - u[i]).abs() < 1e-4, "u[{i}]");
            assert!((m.v[i] - v[i]).abs() < 1e-4, "v[{i}]");
            assert!((m.height[i] - height[i]).abs() < 1e-4, "height[{i}]");
        }
        assert_eq!(m.header.min_height, -100.0);
        assert_eq!(m.header.max_height, 900.0);
        let ns = m.normals.expect("normals");
        assert!((ns[0][2] - 1.0).abs() < 1e-3);
        assert_eq!(m.edges[0], vec![0, 2]);
        assert_eq!(m.edges[3], vec![2, 3]);
    }

    #[test]
    fn truncated_is_a_typed_error() {
        assert!(matches!(decode(&[0u8; 10]), Err(DecodeError::Truncated(_))));
    }

    #[test]
    fn decodes_gzip_framed_input() {
        use std::io::Write;
        let u = vec![0.0, 1.0, 0.0, 1.0];
        let v = vec![0.0, 0.0, 1.0, 1.0];
        let height = vec![0.0, 0.0, 0.0, 0.0];
        let indices = vec![0u32, 1, 2, 2, 1, 3];
        let edges = [vec![0u32, 2], vec![0u32, 1], vec![1u32, 3], vec![2u32, 3]];
        let raw = encode(&test_header(), &u, &v, &height, &indices, None, &edges);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).expect("gzip");
        let gzipped = gz.finish().expect("finish");
        assert_eq!(&gzipped[0..2], &[0x1f, 0x8b], "gzip framed");

        // decode() must transparently gunzip and match the raw decode.
        let from_gz = decode(&gzipped).expect("decode gzip");
        let from_raw = decode(&raw).expect("decode raw");
        assert_eq!(from_gz.vertex_count(), from_raw.vertex_count());
        assert_eq!(from_gz.indices, from_raw.indices);
    }
}
