// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "tiles.h"

#include <memory>
#include <mutex>
#include <unordered_map>

namespace {

using Buffer = std::shared_ptr<const std::vector<uint8_t>>;

struct Store
{
    std::mutex mutex;
    /// URI → the buffer currently published under it.
    std::unordered_map<std::string, Buffer> published;
    /// Handle → a buffer someone is still reading.
    ///
    /// Separate from `published` on purpose: replacing a URI must not pull the
    /// bytes out from under a texture read that is already in flight, and Ar
    /// gives no guarantee about when it finishes with an asset.
    std::unordered_map<uint64_t, Buffer> borrowed;
    uint64_t nextHandle = 1;
};

Store &_TheStore()
{
    // Function-local static: initialised on first use, so the store is ready
    // whether the resolver or the plugin's own registration gets there first.
    static Store store;
    return store;
}

}  // namespace

void
TuileSpikeTiles::Put(const std::string &uri, std::vector<uint8_t> bytes)
{
    Buffer buffer = std::make_shared<const std::vector<uint8_t>>(std::move(bytes));
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    store.published[uri] = std::move(buffer);
}

bool
TuileSpikeTiles::Has(const std::string &uri)
{
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    return store.published.find(uri) != store.published.end();
}

TuileSpikeTiles::Bytes
TuileSpikeTiles::Get(const std::string &uri)
{
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);

    auto it = store.published.find(uri);
    if (it == store.published.end() || !it->second) {
        return Bytes();
    }

    const uint64_t handle = store.nextHandle++;
    // The borrow holds its own reference, so the bytes outlive any later Put or
    // Clear on the same URI.
    store.borrowed[handle] = it->second;

    Bytes bytes;
    bytes.data = it->second->data();
    bytes.size = it->second->size();
    bytes.handle = handle;
    return bytes;
}

void
TuileSpikeTiles::Release(uint64_t handle)
{
    if (handle == 0) {
        return;
    }
    // Held past the lock: dropping the last reference frees the buffer, and
    // doing that under the mutex would serialise deallocation across threads
    // for no reason.
    Buffer released;
    {
        Store &store = _TheStore();
        std::lock_guard<std::mutex> lock(store.mutex);
        auto it = store.borrowed.find(handle);
        if (it == store.borrowed.end()) {
            return;
        }
        released = std::move(it->second);
        store.borrowed.erase(it);
    }
}

void
TuileSpikeTiles::Clear()
{
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    store.published.clear();
}
