// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "resolver.h"
#include "tiles.h"

#include <pxr/base/tf/diagnostic.h>
#include <pxr/base/tf/registryManager.h>
#include <pxr/usd/ar/defineResolver.h>
#include <pxr/usd/ar/inMemoryAsset.h>

#include <cstring>

PXR_NAMESPACE_OPEN_SCOPE

AR_DEFINE_RESOLVER(Tuile_SpikeResolver, ArResolver);

Tuile_SpikeResolver::Tuile_SpikeResolver() = default;
Tuile_SpikeResolver::~Tuile_SpikeResolver() = default;

std::string
Tuile_SpikeResolver::_CreateIdentifier(
    const std::string &assetPath,
    const ArResolvedPath &anchorAssetPath) const
{
    // A tile URI is absolute by construction and never relative to the layer
    // that mentions it, so anchoring would only corrupt it.
    (void)anchorAssetPath;
    return assetPath;
}

std::string
Tuile_SpikeResolver::_CreateIdentifierForNewAsset(
    const std::string &assetPath,
    const ArResolvedPath &anchorAssetPath) const
{
    (void)anchorAssetPath;
    return assetPath;
}

ArResolvedPath
Tuile_SpikeResolver::_Resolve(const std::string &assetPath) const
{
    // Resolving means "does this exist, and where" — for us, the URI is the
    // location, so the only real question is whether we hold the bytes.
    // Returning an empty path for an unknown tile is what makes Hydra report a
    // missing texture rather than hand Hio an empty buffer to choke on.
    return TuileSpikeTiles::Has(assetPath) ? ArResolvedPath(assetPath)
                                           : ArResolvedPath();
}

ArResolvedPath
Tuile_SpikeResolver::_ResolveForNewAsset(const std::string &assetPath) const
{
    return ArResolvedPath(assetPath);
}

std::shared_ptr<ArAsset>
Tuile_SpikeResolver::_OpenAsset(const ArResolvedPath &resolvedPath) const
{
    const std::string &uri = resolvedPath.GetPathString();
    TuileSpikeTiles::Bytes bytes = TuileSpikeTiles::Get(uri);
    if (!bytes.data || bytes.size == 0) {
        return nullptr;
    }

    // Ownership crosses here. The buffer was allocated by the producer (Rust,
    // in the real plugin) and must be released by it: the deleter carries that
    // obligation, so Ar can hold the asset for as long as it likes and free it
    // on whatever thread it finishes with.
    std::shared_ptr<const char> buffer(
        reinterpret_cast<const char *>(bytes.data),
        [handle = bytes.handle](const char *) { TuileSpikeTiles::Release(handle); });

    return ArInMemoryAsset::FromBuffer(std::move(buffer), bytes.size);
}

std::shared_ptr<ArWritableAsset>
Tuile_SpikeResolver::_OpenAssetForWrite(
    const ArResolvedPath &resolvedPath,
    WriteMode writeMode) const
{
    (void)resolvedPath;
    (void)writeMode;
    // Tiles come from a service; nothing writes back through this scheme.
    return nullptr;
}

PXR_NAMESPACE_CLOSE_SCOPE
