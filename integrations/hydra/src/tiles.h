// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_TILES_H
#define TUILE_HYDRA_TILES_H

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

/// The bytes behind a `tuile://` URI.
///
/// A deliberately C-shaped seam. The store is C++ today, holding buffers the
/// plugin itself produced; in the finished plugin the same three calls are
/// answered by Rust, which is why nothing here is a `std::shared_ptr` or a
/// `std::string_view` into a container — the borrow is a pointer, a length and
/// a handle to give back, which is all an FFI boundary can carry.
class TuileSpikeTiles
{
public:
    /// A borrow of a stored buffer, valid until [Release] is called on
    /// `handle`.
    struct Bytes
    {
        const uint8_t *data = nullptr;
        size_t size = 0;
        /// What [Release] needs to drop this borrow. Zero means "nothing
        /// borrowed", and releasing it is a no-op.
        uint64_t handle = 0;
    };

    /// Stores `bytes` under `uri`, replacing any previous entry.
    ///
    /// Replacing does not invalidate outstanding borrows: an in-flight texture
    /// read keeps the buffer it was given until it releases it. That is the
    /// whole reason borrows are refcounted rather than pointing at a map slot.
    static void Put(const std::string &uri, std::vector<uint8_t> bytes);

    /// Whether the store holds `uri`. Cheap — the resolver calls it per lookup.
    static bool Has(const std::string &uri);

    /// Borrows `uri`'s bytes, or an empty [Bytes] if absent.
    static Bytes Get(const std::string &uri);

    /// Releases a borrow. Safe to call with `0`, and safe to call from any
    /// thread — Ar frees assets on whichever thread finishes with them.
    static void Release(uint64_t handle);

    /// Drops every entry. Outstanding borrows survive until released.
    static void Clear();
};

#endif
