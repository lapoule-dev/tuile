// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_FFI_H
#define TUILE_HYDRA_FFI_H

#include <cstddef>
#include <cstdint>

/// The C ABI exported by the `tuile-hydra` Rust crate.
///
/// Hand-written rather than generated: the surface is small, and a header we
/// wrote is a header we can comment. It must stay in step with
/// `crates/tuile-hydra/src/ffi.rs` — the two tests that matter are that
/// `TuileStatus`'s values match and that every struct is `#[repr(C)]` there.
extern "C" {

/// Mirrors `TuileStatus`. Zero is success.
enum TuileStatus : int32_t
{
    TuileStatus_Ok = 0,
    TuileStatus_BadArgument = 1,
    TuileStatus_TimedOut = 2,
    TuileStatus_ServerGone = 3,
    TuileStatus_TilesFailed = 4,
    TuileStatus_InternalError = 5,
    TuileStatus_EncodeFailed = 6,
    /// The index named nothing. An ordinary answer, not a mistake — enumerate
    /// textures by asking for 0, 1, 2… until this comes back.
    TuileStatus_NotFound = 7,
};

/// Mirrors `TuileBuffer`: a borrow of Rust-owned bytes, with an exact length.
struct TuileBuffer
{
    const uint8_t *data;
    size_t len;
};

struct TuileSession;
struct TuileFrame;

/// Mirrors `TuileTile`. See `ffi.rs` for the ownership and precision contract —
/// in particular that `origin_ecef` is double and positions are float relative
/// to it, and that narrowing that subtraction is the one thing not to do.
struct TuileTile
{
    uint64_t tile_id;
    double origin_ecef[3];
    TuileBuffer positions;
    TuileBuffer normals;
    TuileBuffer uvs;
    TuileBuffer indices;
    uint32_t vertex_count;
    uint32_t index_count;
    float base_color_factor[4];
    int32_t base_color_texture;
    /// What the tile's imagery was composed from, or 0 when it carries none.
    /// A tile keeps its id across frames; its imagery does not — it is
    /// re-draped at another level as the camera moves. Comparing this is how a
    /// consumer that keeps prims tells "unchanged" from "same tile, new
    /// pixels" without rebuilding a URI to look at.
    uint64_t drape;
};

/// Mirrors `TuileTexture`. `uri` is UTF-8 and is NOT null-terminated — build a
/// `std::string` from (data, len), never from `data` alone.
struct TuileTexture
{
    TuileBuffer uri;
    TuileBuffer png;
};

/// Mirrors `TuileStr`: UTF-8 the caller lends for the duration of one call.
/// NOT null-terminated — build it from (data, len), and note that `std::string`
/// data() plus size() is exactly the right pair.
struct TuileStr
{
    const uint8_t *data;
    size_t len;
};

/// Mirrors `TuileGlobeConfig`. Every field is a value or a borrowed string, so
/// this can live on the stack and own nothing.
struct TuileGlobeConfig
{
    /// A Cesium ion access token. Required.
    TuileStr ion_token;
    /// ion asset holding terrain. 0 selects Cesium World Terrain.
    int64_t terrain_asset_id;
    /// ion asset holding imagery. 0 selects Bing Aerial; NEGATIVE disables
    /// imagery, which is the terrain-only debug view.
    int64_t imagery_asset_id;
    /// Tile cache directory. Empty selects the per-user default. Worth setting
    /// deliberately on a farm: every node with a cold cache multiplies the
    /// traffic to ion and Bing by the node count.
    TuileStr cache_dir;
    /// Screen-space error target. 0 or less keeps the traversal default.
    double maximum_screen_space_error;
    /// Seconds a frame may take to converge. 0 or less keeps the default.
    double frame_timeout_seconds;
    /// Whether a frame that lost tiles is an error. Leave true where output is
    /// kept: a failed tile leaves no hole — its ancestor stands in — so the
    /// frame renders plausibly at the wrong level of detail.
    bool fail_on_tile_errors;
};

/// Mirrors `TuileViewState`: one camera, twelve doubles.
///
/// ECEF and f64 throughout. Narrowing a position on the globe to float is what
/// jitter is made of, so do not pass a camera that has already been rebased.
struct TuileViewState
{
    double position[3];
    double direction[3];
    double up[3];
    /// The real render resolution — it is what turns a geometric error into a
    /// screen-space one.
    double viewport_px[2];
    double fovy_rad;
};

/// Opens a session on an ion globe. BLOCKS while resolving sources.
/// On success *out owns a session to release with tuile_session_free;
/// on failure *out is left null.
TuileStatus tuile_session_new(const TuileGlobeConfig *config, TuileSession **out);

/// Resolves one frame, blocking until it converges. Several views are a UNION,
/// not a choice: a stereo pair must share one selection or the eyes disagree at
/// level-of-detail boundaries.
/// On success *out owns a frame to release with tuile_frame_free, and every
/// buffer borrowed from it stays valid until then.
TuileStatus tuile_session_frame(
    TuileSession *session,
    const TuileViewState *views,
    size_t view_count,
    TuileFrame **out);

void tuile_session_free(TuileSession *session);
void tuile_frame_free(TuileFrame *frame);
TuileStatus tuile_frame_tile_count(const TuileFrame *frame, size_t *out);
TuileStatus tuile_frame_tile(const TuileFrame *frame, size_t index, TuileTile *out);
TuileStatus tuile_frame_texture(
    const TuileFrame *frame,
    size_t tile_index,
    size_t texture_index,
    TuileTexture *out);

}  // extern "C"

#endif
