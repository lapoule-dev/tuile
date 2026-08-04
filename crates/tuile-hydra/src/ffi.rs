// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The `extern "C"` surface, and nothing else.
//!
//! Every function here obeys the same three rules, and they are the reason this
//! module is separate from [`crate::session`]:
//!
//! 1. **Nothing unwinds.** A panic crossing `extern "C"` aborts the process, so
//!    each entry point catches and returns a [`TuileStatus`].
//! 2. **Rust owns every allocation.** The C++ side borrows pointers valid until
//!    it releases the frame that produced them, and frees nothing itself.
//! 3. **Null is checked, never assumed.** These pointers come from another
//!    language; treating them as trustworthy is how a plugin turns a mistake
//!    into a crash inside the renderer.

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::session::{Frame, FrameError, Session};

/// The outcome of a call. Zero is success; everything else is a reason.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuileStatus {
    Ok = 0,
    /// A required pointer was null, or an index was out of range.
    BadArgument = 1,
    /// The frame did not converge within its timeout.
    TimedOut = 2,
    /// The geometry server ended.
    ServerGone = 3,
    /// The frame converged but tiles failed to load; the geometry would be
    /// silently coarser than asked for.
    TilesFailed = 4,
    /// A panic was caught at the boundary. Always a bug on this side.
    InternalError = 5,
    /// A texture could not be encoded.
    EncodeFailed = 6,
    /// The index named nothing — an untextured tile, or past the end. Distinct
    /// from [`TuileStatus::BadArgument`] because it is an ordinary answer, not a
    /// mistake: a caller enumerating textures stops on it.
    NotFound = 7,
}

/// A borrowed span of a Rust-owned buffer.
///
/// Valid until the frame it came from is released. Deliberately not a
/// null-terminated anything: these are binary buffers with an exact length.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TuileBuffer {
    pub data: *const u8,
    pub len: usize,
}

impl TuileBuffer {
    const EMPTY: Self = Self {
        data: std::ptr::null(),
        len: 0,
    };

    /// Borrows a slice. The lifetime is the caller's to respect — this is the
    /// point at which Rust's guarantees stop and the contract starts.
    fn of<T>(slice: &[T]) -> Self {
        Self {
            data: slice.as_ptr().cast::<u8>(),
            len: std::mem::size_of_val(slice),
        }
    }
}

/// One tile's geometry, as pointers into buffers Rust still owns.
///
/// Positions, normals and uvs are f32 triples/pairs; indices are u32. All of
/// them are already contiguous on the Rust side, so nothing is copied to build
/// this — the arrays are handed over exactly as they were decoded.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TuileTile {
    /// The tile's identity, stable across frames — what a consumer should build
    /// a prim path from, so an unchanged tile keeps its path and the renderer
    /// can update incrementally instead of rebuilding.
    pub tile_id: u64,
    /// The tile's rebasing origin, ECEF, **double precision**.
    ///
    /// Positions are f32 relative to this. A consumer must do the subtraction
    /// against its own render origin in double and only then narrow — doing it
    /// in f32 reintroduces exactly the jitter this arrangement removes.
    pub origin_ecef: [f64; 3],
    pub positions: TuileBuffer,
    pub normals: TuileBuffer,
    pub uvs: TuileBuffer,
    pub indices: TuileBuffer,
    pub vertex_count: u32,
    pub index_count: u32,
    pub base_color_factor: [f32; 4],
    /// Index into **this tile's** textures, or `-1` for an untextured tile.
    /// Pass it back to [`tuile_frame_texture`] along with the tile's own index.
    pub base_color_texture: i32,
}

impl From<&FrameError> for TuileStatus {
    fn from(e: &FrameError) -> Self {
        match e {
            FrameError::TimedOut(_) => TuileStatus::TimedOut,
            FrameError::ServerGone => TuileStatus::ServerGone,
            FrameError::TilesFailed { .. } => TuileStatus::TilesFailed,
            FrameError::TextureEncode(_) => TuileStatus::EncodeFailed,
            FrameError::Poisoned => TuileStatus::InternalError,
        }
    }
}

/// Runs a closure, turning any panic into a status rather than an abort.
fn guard<F: FnOnce() -> TuileStatus>(f: F) -> TuileStatus {
    // Nothing may propagate; the renderer has no way to recover from an unwind
    // through its own stack frames.
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(TuileStatus::InternalError)
}

/// Releases a session created by the host.
///
/// # Safety
/// `session` must come from a session constructor and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn tuile_session_free(session: *mut Session) {
    if session.is_null() {
        return;
    }
    let _ = guard(|| {
        drop(unsafe { Box::from_raw(session) });
        TuileStatus::Ok
    });
}

/// Releases a frame and every buffer borrowed from it.
///
/// # Safety
/// `frame` must come from [`tuile_frame_take`] and must not be used again, nor
/// any [`TuileBuffer`] obtained from it.
#[no_mangle]
pub unsafe extern "C" fn tuile_frame_free(frame: *mut Frame) {
    if frame.is_null() {
        return;
    }
    let _ = guard(|| {
        drop(unsafe { Box::from_raw(frame) });
        TuileStatus::Ok
    });
}

/// How many tiles a frame holds.
///
/// # Safety
/// `frame` must be a live frame.
#[no_mangle]
pub unsafe extern "C" fn tuile_frame_tile_count(frame: *const Frame, out: *mut usize) -> TuileStatus {
    if frame.is_null() || out.is_null() {
        return TuileStatus::BadArgument;
    }
    guard(|| {
        let frame = unsafe { &*frame };
        unsafe { *out = frame.tiles.len() };
        TuileStatus::Ok
    })
}

/// Describes one tile of a frame.
///
/// The buffers written into `out` stay valid until the frame is freed.
///
/// # Safety
/// `frame` must be a live frame and `out` must point at a writable
/// [`TuileTile`].
#[no_mangle]
pub unsafe extern "C" fn tuile_frame_tile(
    frame: *const Frame,
    index: usize,
    out: *mut TuileTile,
) -> TuileStatus {
    if frame.is_null() || out.is_null() {
        return TuileStatus::BadArgument;
    }
    guard(|| {
        let frame = unsafe { &*frame };
        let Some(tile) = frame.tiles.get(index) else {
            return TuileStatus::BadArgument;
        };
        // One prim per tile, so a tile with several meshes is flattened by the
        // consumer; the spike takes the first, which is what terrain produces.
        let Some(mesh) = tile.content.meshes.first() else {
            return TuileStatus::BadArgument;
        };

        let origin = tile.origin_ecef;
        unsafe {
            *out = TuileTile {
                tile_id: tile.tile.0,
                origin_ecef: [origin.x, origin.y, origin.z],
                positions: TuileBuffer::of(&mesh.positions),
                normals: mesh.normals.as_ref().map_or(TuileBuffer::EMPTY, |n| {
                    TuileBuffer::of(n)
                }),
                uvs: mesh
                    .uvs
                    .as_ref()
                    .map_or(TuileBuffer::EMPTY, |u| TuileBuffer::of(u)),
                indices: TuileBuffer::of(&mesh.indices),
                vertex_count: mesh.positions.len() as u32,
                index_count: mesh.indices.len() as u32,
                base_color_factor: mesh.material.base_color_factor,
                base_color_texture: mesh
                    .material
                    .base_color_texture
                    .and_then(|i| i32::try_from(i).ok())
                    .unwrap_or(-1),
            }
        };
        TuileStatus::Ok
    })
}

/// One tile texture: the URI it is published under, and its PNG bytes.
///
/// Both borrows stay valid until the frame is freed.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TuileTexture {
    /// UTF-8, **not** null-terminated — the length is in the buffer.
    pub uri: TuileBuffer,
    pub png: TuileBuffer,
}

/// Encodes (once) and borrows a tile's texture.
///
/// PNG rather than the raw RGBA the tile decoded to, because the host's image
/// plugin decodes encoded bytes and nothing else. That re-encode is real CPU
/// per texture per frame; it is the price of draping a private copy per tile,
/// and it goes away when imagery is referenced rather than baked.
///
/// Returns [`TuileStatus::NotFound`] when either index names nothing, which is
/// how a caller enumerates: ask for 0, 1, 2… until it stops.
///
/// # Safety
/// `frame` must be a live frame and `out` must point at a writable
/// [`TuileTexture`].
#[no_mangle]
pub unsafe extern "C" fn tuile_frame_texture(
    frame: *const Frame,
    tile_index: usize,
    texture_index: usize,
    out: *mut TuileTexture,
) -> TuileStatus {
    if frame.is_null() || out.is_null() {
        return TuileStatus::BadArgument;
    }
    guard(|| {
        let frame = unsafe { &*frame };
        match frame.texture_png(tile_index, texture_index) {
            Ok(Some(texture)) => {
                unsafe {
                    *out = TuileTexture {
                        uri: TuileBuffer::of(texture.uri.as_bytes()),
                        png: TuileBuffer::of(&texture.png),
                    }
                };
                TuileStatus::Ok
            }
            Ok(None) => TuileStatus::NotFound,
            Err(e) => TuileStatus::from(&e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary must survive a panic, because the alternative is aborting
    /// the renderer.
    #[test]
    #[allow(clippy::panic, reason = "the panic is the thing under test")]
    fn a_panic_becomes_a_status() {
        let status = guard(|| panic!("boundary test"));
        assert_eq!(status, TuileStatus::InternalError);
    }

    #[test]
    fn a_normal_return_passes_through() {
        assert_eq!(guard(|| TuileStatus::Ok), TuileStatus::Ok);
    }

    /// Null is what a mistake on the other side looks like; it must be a
    /// status, never a dereference.
    #[test]
    fn null_arguments_are_refused() {
        let mut count = 0usize;
        assert_eq!(
            unsafe { tuile_frame_tile_count(std::ptr::null(), &mut count) },
            TuileStatus::BadArgument
        );
        let frame = Frame::new(Vec::new());
        assert_eq!(
            unsafe { tuile_frame_tile_count(&frame, std::ptr::null_mut()) },
            TuileStatus::BadArgument
        );
    }

    #[test]
    fn an_empty_frame_reports_no_tiles() {
        let frame = Frame::new(Vec::new());
        let mut count = 99usize;
        assert_eq!(
            unsafe { tuile_frame_tile_count(&frame, &mut count) },
            TuileStatus::Ok
        );
        assert_eq!(count, 0);

        let mut tile = std::mem::MaybeUninit::<TuileTile>::uninit();
        assert_eq!(
            unsafe { tuile_frame_tile(&frame, 0, tile.as_mut_ptr()) },
            TuileStatus::BadArgument,
            "an out-of-range index must not be read"
        );
    }

    /// Asking a frame with no tiles for a texture is absence, not a mistake:
    /// the distinction is what lets a caller enumerate until it stops.
    #[test]
    fn an_absent_texture_is_not_found_rather_than_bad_argument() {
        let frame = Frame::new(Vec::new());
        let mut texture = std::mem::MaybeUninit::<TuileTexture>::uninit();
        assert_eq!(
            unsafe { tuile_frame_texture(&frame, 0, 0, texture.as_mut_ptr()) },
            TuileStatus::NotFound
        );
        assert_eq!(
            unsafe { tuile_frame_texture(std::ptr::null(), 0, 0, texture.as_mut_ptr()) },
            TuileStatus::BadArgument
        );
    }

    /// A buffer's length is in bytes, not elements — getting this wrong reads
    /// a third of a position array and produces silently mangled geometry.
    #[test]
    fn buffer_lengths_are_in_bytes() {
        let positions: Vec<[f32; 3]> = vec![[0.0; 3]; 4];
        let buffer = TuileBuffer::of(&positions);
        assert_eq!(buffer.len, 4 * 3 * std::mem::size_of::<f32>());
    }
}
