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

use glam::{DVec2, DVec3};
use tuile_core::traversal::ViewStateParams;

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
    /// What the tile's imagery was composed from, or `0` when it carries none.
    ///
    /// A tile keeps its id across frames; its imagery does not, because the
    /// camera moves and it is re-draped at another level. A consumer that
    /// keeps prims between frames needs to tell those two cases apart, and
    /// this makes it an integer compare instead of a string rebuild.
    pub drape: u64,
}

impl From<&FrameError> for TuileStatus {
    fn from(e: &FrameError) -> Self {
        match e {
            FrameError::TimedOut(_) => TuileStatus::TimedOut,
            FrameError::ServerGone => TuileStatus::ServerGone,
            FrameError::TilesFailed { .. } => TuileStatus::TilesFailed,
            // A selected tile with no content is the same class of failure as
            // a tile that failed to load: the frame is not the one asked for.
            FrameError::MissingContent { .. } => TuileStatus::TilesFailed,
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

/// A UTF-8 string the caller lends us for the duration of one call.
///
/// Length-carrying rather than null-terminated, for the same reason as
/// [`TuileBuffer`]: a C++ `std::string` may contain anything, and asking the
/// caller to guarantee a terminator is asking it to build a temporary it does
/// not otherwise need.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TuileStr {
    pub data: *const u8,
    pub len: usize,
}

impl TuileStr {
    /// Borrows the string, or `None` if it is null, empty or not UTF-8.
    ///
    /// # Safety
    /// `data` must point at `len` readable bytes for the duration of the call.
    unsafe fn as_str<'a>(&self) -> Option<&'a str> {
        if self.data.is_null() || self.len == 0 {
            return None;
        }
        std::str::from_utf8(unsafe { std::slice::from_raw_parts(self.data, self.len) }).ok()
    }
}

/// What a host states to open a globe.
///
/// Every field is a plain value or a borrowed string, so C++ can build one on
/// the stack without allocating and without owning anything afterwards.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TuileGlobeConfig {
    /// A Cesium ion access token. Required.
    pub ion_token: TuileStr,
    /// The ion asset holding terrain. `0` selects Cesium World Terrain.
    pub terrain_asset_id: i64,
    /// The ion asset holding imagery. `0` selects Bing Aerial; **negative**
    /// disables imagery entirely, which is the terrain-only debug view.
    pub imagery_asset_id: i64,
    /// Where to keep the tile cache. Empty selects the per-user default.
    pub cache_dir: TuileStr,
    /// Screen-space error target. `0` or less keeps the traversal default.
    pub maximum_screen_space_error: f64,
    /// How long one frame may take to converge. `0` or less keeps the default.
    pub frame_timeout_seconds: f64,
    /// Whether a frame that lost tiles is an error. Should stay true anywhere
    /// the output is kept: a failed tile leaves no hole, its ancestor stands
    /// in, and the frame renders plausibly at the wrong level of detail.
    pub fail_on_tile_errors: bool,
}

/// One camera, as the traversal needs it: twelve doubles and nothing else.
///
/// ECEF, f64, and that is not negotiable — narrowing a position on the globe to
/// f32 is what jitter is made of.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TuileViewState {
    pub position: [f64; 3],
    pub direction: [f64; 3],
    pub up: [f64; 3],
    /// Width and height in pixels. What turns a geometric error into a
    /// screen-space one, so it must be the real render resolution.
    pub viewport_px: [f64; 2],
    pub fovy_rad: f64,
}

/// Reads the three meanings packed into `imagery_asset_id`.
///
/// Negative disables imagery; zero means "whatever the default is"; positive
/// names an asset. Three meanings in one integer because a C struct has no
/// `Option`, and the alternative — a separate boolean — is a second field that
/// can contradict the first, which is worse than an encoding one has to read.
fn imagery_asset(id: i64) -> Option<i64> {
    match id {
        id if id < 0 => None,
        0 => Some(crate::globe::BING_AERIAL),
        id => Some(id),
    }
}

/// Opens a session on a Cesium ion globe.
///
/// Blocks while it resolves sources — an ion endpoint and a Bing metadata
/// document have to be fetched before a tile can be asked for. Call it once,
/// off a thread the host can afford to block.
///
/// On success `*out` owns a session the caller must release with
/// [`tuile_session_free`]. On failure `*out` is left null.
///
/// # Safety
/// `config` must point at a readable [`TuileGlobeConfig`] whose strings are
/// valid for the call, and `out` at a writable pointer.
#[no_mangle]
pub unsafe extern "C" fn tuile_session_new(
    config: *const TuileGlobeConfig,
    out: *mut *mut Session,
) -> TuileStatus {
    if config.is_null() || out.is_null() {
        return TuileStatus::BadArgument;
    }
    guard(|| {
        // The host is a render process with no Rust logging of its own:
        // TUILE_LOG=debug turns the crate's tracing into stderr lines, once.
        static LOGGING: std::sync::Once = std::sync::Once::new();
        LOGGING.call_once(|| {
            if let Ok(filter) = std::env::var("TUILE_LOG") {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::io::stderr)
                    .try_init();
            }
        });
        unsafe { *out = std::ptr::null_mut() };
        let config = unsafe { &*config };

        let Some(token) = (unsafe { config.ion_token.as_str() }) else {
            return TuileStatus::BadArgument;
        };

        let mut globe = crate::globe::GlobeConfig::new(token);
        if config.terrain_asset_id != 0 {
            globe.terrain_asset_id = config.terrain_asset_id;
        }
        globe.imagery_asset_id = imagery_asset(config.imagery_asset_id);
        if let Some(dir) = unsafe { config.cache_dir.as_str() } {
            globe.cache_dir = Some(std::path::PathBuf::from(dir));
        }
        if config.maximum_screen_space_error > 0.0 {
            globe.session.traversal.maximum_screen_space_error = config.maximum_screen_space_error;
        }
        globe.session.frame_timeout =
            crate::globe::duration_or(config.frame_timeout_seconds, globe.session.frame_timeout);
        globe.session.fail_on_tile_errors = config.fail_on_tile_errors;

        match Session::globe(globe) {
            Ok(session) => {
                unsafe { *out = Box::into_raw(Box::new(session)) };
                TuileStatus::Ok
            }
            Err(error) => {
                // The token is in the config, never in the message.
                tracing::error!(%error, "opening the globe");
                TuileStatus::ServerGone
            }
        }
    })
}

/// Resolves one frame for the given views, blocking until it converges.
///
/// Several views are a **union**, not a choice: a stereo pair must share one
/// selection or the eyes disagree at level-of-detail boundaries.
///
/// On success `*out` owns a frame the caller must release with
/// [`tuile_frame_free`], and every buffer borrowed from it stays valid until
/// then. On failure `*out` is left null.
///
/// # Safety
/// `session` must be a live session, `views` must point at `view_count`
/// readable [`TuileViewState`]s, and `out` at a writable pointer.
#[no_mangle]
pub unsafe extern "C" fn tuile_session_frame(
    session: *mut Session,
    views: *const TuileViewState,
    view_count: usize,
    out: *mut *mut Frame,
) -> TuileStatus {
    if session.is_null() || views.is_null() || view_count == 0 || out.is_null() {
        return TuileStatus::BadArgument;
    }
    guard(|| {
        unsafe { *out = std::ptr::null_mut() };
        let session = unsafe { &mut *session };
        let views = unsafe { std::slice::from_raw_parts(views, view_count) };

        let views: Vec<ViewStateParams> = views
            .iter()
            .map(|v| ViewStateParams {
                position: DVec3::from_array(v.position),
                direction: DVec3::from_array(v.direction),
                up: DVec3::from_array(v.up),
                viewport_px: DVec2::from_array(v.viewport_px),
                fovy_rad: v.fovy_rad,
            })
            .collect();

        match session.frame(views) {
            Ok(frame) => {
                unsafe { *out = Box::into_raw(Box::new(frame)) };
                TuileStatus::Ok
            }
            Err(error) => {
                tracing::error!(%error, "resolving a frame");
                TuileStatus::from(&error)
            }
        }
    })
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
pub unsafe extern "C" fn tuile_frame_tile_count(
    frame: *const Frame,
    out: *mut usize,
) -> TuileStatus {
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
                normals: mesh
                    .normals
                    .as_ref()
                    .map_or(TuileBuffer::EMPTY, |n| TuileBuffer::of(n)),
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
                drape: tile.drape(),
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
        let frame = Frame::new("test", Vec::new());
        assert_eq!(
            unsafe { tuile_frame_tile_count(&frame, std::ptr::null_mut()) },
            TuileStatus::BadArgument
        );
    }

    #[test]
    fn an_empty_frame_reports_no_tiles() {
        let frame = Frame::new("test", Vec::new());
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
        let frame = Frame::new("test", Vec::new());
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

    /// Three meanings in one integer is exactly the kind of encoding that gets
    /// inverted, and inverting it means silently rendering an untextured globe.
    #[test]
    fn the_imagery_asset_id_encodes_three_things() {
        assert_eq!(imagery_asset(0), Some(crate::globe::BING_AERIAL));
        assert_eq!(imagery_asset(3812), Some(3812));
        assert_eq!(imagery_asset(-1), None, "negative disables imagery");
        assert_eq!(imagery_asset(i64::MIN), None);
    }

    fn as_str(s: &str) -> TuileStr {
        TuileStr {
            data: s.as_ptr(),
            len: s.len(),
        }
    }

    #[test]
    fn a_borrowed_string_round_trips() {
        let text = "a token";
        assert_eq!(unsafe { as_str(text).as_str() }, Some(text));
    }

    /// Null, empty and non-UTF-8 all mean "the caller stated nothing", because
    /// the alternative is reading whatever happens to be at that address.
    #[test]
    fn a_missing_or_invalid_string_is_nothing() {
        let null = TuileStr {
            data: std::ptr::null(),
            len: 7,
        };
        assert_eq!(unsafe { null.as_str() }, None);
        assert_eq!(unsafe { as_str("").as_str() }, None);

        let invalid: [u8; 2] = [0xff, 0xfe];
        let bad = TuileStr {
            data: invalid.as_ptr(),
            len: invalid.len(),
        };
        assert_eq!(unsafe { bad.as_str() }, None);
    }

    /// No network here: these must be refused before anything is attempted, so
    /// a misconfigured host fails immediately rather than after a timeout.
    #[test]
    fn opening_a_session_refuses_bad_arguments() {
        let mut out: *mut Session = std::ptr::null_mut();
        assert_eq!(
            unsafe { tuile_session_new(std::ptr::null(), &mut out) },
            TuileStatus::BadArgument
        );

        let config = TuileGlobeConfig {
            ion_token: as_str(""),
            terrain_asset_id: 0,
            imagery_asset_id: 0,
            cache_dir: as_str(""),
            maximum_screen_space_error: 0.0,
            frame_timeout_seconds: 0.0,
            fail_on_tile_errors: true,
        };
        assert_eq!(
            unsafe { tuile_session_new(&config, &mut out) },
            TuileStatus::BadArgument,
            "an empty token must not reach the network"
        );
        assert!(out.is_null(), "a failed open must leave the pointer null");

        assert_eq!(
            unsafe { tuile_session_new(&config, std::ptr::null_mut()) },
            TuileStatus::BadArgument
        );
    }

    #[test]
    fn resolving_a_frame_refuses_bad_arguments() {
        let view = TuileViewState {
            position: [0.0; 3],
            direction: [0.0, 0.0, -1.0],
            up: [0.0, 1.0, 0.0],
            viewport_px: [512.0, 512.0],
            fovy_rad: 0.8,
        };
        let mut out: *mut Frame = std::ptr::null_mut();
        assert_eq!(
            unsafe { tuile_session_frame(std::ptr::null_mut(), &view, 1, &mut out) },
            TuileStatus::BadArgument
        );
        assert_eq!(
            unsafe { tuile_session_frame(std::ptr::null_mut(), &view, 1, std::ptr::null_mut()) },
            TuileStatus::BadArgument
        );

        // Zero views is a mistake rather than an empty answer: the traversal
        // has nothing to select against, and a frame with no camera is not a
        // frame.
        //
        // The session pointer here is non-null and never dereferenced, which
        // holds only because every argument is validated before any is read.
        // Keep it that way: moving a dereference above the count check would
        // make this test undefined rather than merely failing.
        let unreadable = std::ptr::NonNull::<Session>::dangling().as_ptr();
        assert_eq!(
            unsafe { tuile_session_frame(unreadable, &view, 0, &mut out) },
            TuileStatus::BadArgument
        );
        assert_eq!(
            unsafe { tuile_session_frame(unreadable, std::ptr::null(), 1, &mut out) },
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
