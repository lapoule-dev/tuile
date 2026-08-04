// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_SPIKE_RESOLVER_H
#define TUILE_SPIKE_RESOLVER_H

#include <pxr/pxr.h>
#include <pxr/usd/ar/resolver.h>
#include <pxr/usd/ar/resolvedPath.h>
#include <pxr/usd/ar/asset.h>

#include <string>

PXR_NAMESPACE_OPEN_SCOPE

/// Serves texture bytes that only ever exist in memory, under `tuile://`.
///
/// This is the whole texture story, and it needs no image plugin: Hio's stock
/// stb reader already opens its input through `ArGetResolver().OpenAsset()` and
/// decodes from the returned buffer. Give it bytes and a URI whose extension it
/// recognises, and a JPEG that was never written to disk loads exactly like one
/// that was — for every render delegate, not just Storm.
///
/// A URI resolver may not also be the primary resolver (Ar sets
/// `canBePrimaryResolver = uriSchemes.empty()`), so this one only ever sees
/// paths in its own scheme and the filesystem resolver keeps everything else.
class Tuile_SpikeResolver final : public ArResolver
{
public:
    Tuile_SpikeResolver();
    ~Tuile_SpikeResolver() override;

protected:
    std::string _CreateIdentifier(
        const std::string &assetPath,
        const ArResolvedPath &anchorAssetPath) const override;

    std::string _CreateIdentifierForNewAsset(
        const std::string &assetPath,
        const ArResolvedPath &anchorAssetPath) const override;

    /// A `tuile://` URI is already its own resolved form: there is no search
    /// path to walk and no file to stat, so resolution is the identity.
    ArResolvedPath _Resolve(const std::string &assetPath) const override;

    ArResolvedPath _ResolveForNewAsset(
        const std::string &assetPath) const override;

    std::shared_ptr<ArAsset> _OpenAsset(
        const ArResolvedPath &resolvedPath) const override;

    std::shared_ptr<ArWritableAsset> _OpenAssetForWrite(
        const ArResolvedPath &resolvedPath,
        WriteMode writeMode) const override;
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif
