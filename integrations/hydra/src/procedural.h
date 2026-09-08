// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_PROCEDURAL_H
#define TUILE_HYDRA_PROCEDURAL_H

#include "ffi.h"

#include <pxr/pxr.h>
#include <pxr/base/gf/vec3d.h>
#include <pxr/base/tf/debug.h>
#include <pxr/imaging/hdGp/generativeProcedural.h>
#include <pxr/usd/sdf/path.h>

#include <map>
#include <string>
#include <vector>

PXR_NAMESPACE_OPEN_SCOPE

// Enable with TF_DEBUG=TUILE_HYDRA_PROCEDURAL. Declared here rather than in
// the implementation because the plugin's Construct counts instances with it,
// and the count is only meaningful next to the cook count.
TF_DEBUG_CODES(TUILE_HYDRA_PROCEDURAL);

/// Emits terrain tiles for whichever camera the scene says to render through.
///
/// The real thing: each cook opens (once) a streaming session over the Rust
/// core through the C ABI, resolves the camera the scene names, blocks until
/// the selection converges — eager and fatal, per docs/15 — and emits one mesh
/// child per selected tile, plus one material child per textured tile whose
/// `UsdUVTexture` points at a `tuile://` URI served by the Ar resolver.
///
/// # Why a camera dependency is legitimate here
///
/// Hydra's contract is that a scene can be re-rendered from any camera without
/// changing, and level-of-detail selection appears to break it. It does not,
/// because the camera we refine for is *named by the scene*, not taken from a
/// viewport: the geometry stays a function of (stage, time), which is what
/// makes a frame reproducible on a farm.
///
/// # Which camera
///
/// Resolved in this order, most authored first, because each step down is one
/// step further from something a pipeline can pin:
///
///  1. `tuile:cameras` on this prim — explicit, and the only way to refine for
///     several views at once;
///  2. the active render settings' camera, which also carries the resolution
///     needed to turn a geometric error into a screen-space one;
///  3. `HdSceneGlobalsSchema`'s primary camera — what `usdrecord` sets;
///  4. nothing, and we fall back to a fixed view above the render origin —
///     view-independent, usable by someone who knows nothing about us.
///
/// # Configuration
///
/// Primvars on the procedural prim (the manifest writes them):
/// `tuile:renderOrigin` (double3, every child xform is relative to it and the
/// camera is un-rebased with it), `tuile:terrainAssetId` / `tuile:imageryAssetId`
/// (ints, the ABI's encoding), `tuile:maxSse` (double, 0 keeps the default),
/// `tuile:viewportPx` (double2, the SSE fallback resolution). The ion token
/// deliberately never crosses a stage: it comes from `TUILE_ION_TOKEN` in the
/// environment, and an optional `TUILE_CACHE_DIR` names the tile cache.
class TuileGlobeProcedural final : public HdGpGenerativeProcedural
{
public:
    explicit TuileGlobeProcedural(const SdfPath &proceduralPrimPath);
    ~TuileGlobeProcedural() override;

    DependencyMap UpdateDependencies(
        const HdSceneIndexBaseRefPtr &inputScene) override;

    ChildPrimTypeMap Update(
        const HdSceneIndexBaseRefPtr &inputScene,
        const ChildPrimTypeMap &previousResult,
        const DependencyMap &dirtiedDependencies,
        HdSceneIndexObserver::DirtiedPrimEntries *outputDirtiedPrims) override;

    /// Called from several threads at once — everything it reads must already
    /// be settled by `Update`.
    HdSceneIndexPrim GetChildPrim(
        const HdSceneIndexBaseRefPtr &inputScene,
        const SdfPath &childPrimPath) override;

private:
    /// The camera to refine for, per the ladder above. Empty when none
    /// resolves, which is the view-independent case rather than an error.
    SdfPath _ResolveCamera(const HdSceneIndexBaseRefPtr &inputScene) const;

    /// Opens the session on first use. Fatal and sticky on failure: a globe
    /// that cannot open must not be retried on every cook of every frame.
    bool _EnsureSession(const HdSceneIndexBaseRefPtr &inputScene);

    /// The view this cook selects tiles against — the resolved camera through
    /// `TuileViewFromCamera`, or the fixed fallback above the render origin.
    bool _ViewForCook(const HdSceneIndexBaseRefPtr &inputScene,
                      TuileViewState *out) const;

    SdfPath _primPath;
    /// Cached between `UpdateDependencies` and `Update` so both agree on which
    /// camera this cook is about.
    SdfPath _cameraPath;

    /// Cooks served by this instance — the other half of the plugin's
    /// construction count.
    uint64_t _cooks = 0;

    TuileSession *_session = nullptr;
    /// Set after a session open failed: the error was reported once, loudly,
    /// and every later cook stays empty instead of re-failing per frame.
    bool _sessionFailed = false;
    /// The converged frame the current children read from. Freed at the start
    /// of the next cook — `GetChildPrim` copies into retained data sources, so
    /// nothing outlives it.
    TuileFrame *_frame = nullptr;

    GfVec3d _renderOrigin = GfVec3d(0.0);
    double _fallbackViewportPx[2] = {1920.0, 1440.0};

    /// One entry per selected tile, in traversal order — the order the frame
    /// hands them out, which is the order every farm node agrees on.
    struct _Tile
    {
        size_t index = 0;
        uint64_t id = 0;
        /// The first texture's `tuile://` URI, empty when untextured. Held
        /// here because the material child needs it after the textures were
        /// already pushed into the resolver's store.
        std::string textureUri;
    };
    std::map<SdfPath, _Tile> _tilesByPath;
    /// Material child path -> the tile whose texture it binds.
    std::map<SdfPath, _Tile> _materialsByPath;
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif
