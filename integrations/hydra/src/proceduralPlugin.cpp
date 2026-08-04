// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "proceduralPlugin.h"
#include "ffi.h"
#include "procedural.h"

#include <pxr/base/tf/diagnostic.h>
#include <pxr/base/tf/type.h>
#include <pxr/imaging/hdGp/generativeProceduralPluginRegistry.h>

PXR_NAMESPACE_OPEN_SCOPE

namespace {

/// Proves the Rust static library is linked and callable.
///
/// A staticlib whose symbols nothing references is dropped by the linker
/// entirely, so "it built" says nothing about whether the boundary works. One
/// call with arguments the Rust side is contractually required to reject turns
/// that into a fact: a wrong answer here means the ABI drifted, and finding
/// that at plugin-load time beats finding it mid-render.
bool _RustBoundaryAnswers()
{
    return tuile_frame_tile_count(nullptr, nullptr) == TuileStatus_BadArgument;
}

}  // namespace

// The TfType name registered here must match the key under `Info.Types` in
// plugInfo.json, and `displayName` there is what a stage writes into
// `primvars:hdGp:proceduralType` — the registry matches the authored token
// against display names first, and only falls back to the type name.
TF_REGISTRY_FUNCTION(TfType)
{
    HdGpGenerativeProceduralPluginRegistry::Define<
        TuileGlobeProceduralPlugin, HdGpGenerativeProceduralPlugin>();
}

HdGpGenerativeProcedural *
TuileGlobeProceduralPlugin::Construct(const SdfPath &proceduralPrimPath)
{
    // Once per process: a mismatched ABI is a build problem, not a per-prim one.
    static const bool boundaryOk = _RustBoundaryAnswers();
    if (!boundaryOk) {
        TF_CODING_ERROR(
            "tuile: the Rust boundary did not answer as specified; the plugin "
            "and libtuile_hydra.a are out of step. Refusing to construct.");
        return nullptr;
    }
    return new TuileGlobeProcedural(proceduralPrimPath);
}

PXR_NAMESPACE_CLOSE_SCOPE
