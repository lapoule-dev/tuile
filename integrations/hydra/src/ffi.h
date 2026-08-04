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
};

/// Mirrors `TuileTexture`. `uri` is UTF-8 and is NOT null-terminated — build a
/// `std::string` from (data, len), never from `data` alone.
struct TuileTexture
{
    TuileBuffer uri;
    TuileBuffer png;
};

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
