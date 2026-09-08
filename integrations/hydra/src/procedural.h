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

    /// Builds one tile's mesh prim, copying everything it needs out of the
    /// frame. Called once per tile per *appearance*, not once per cook.
    HdSceneIndexPrim _BuildTilePrim(const TuileTile &tile,
                                    bool textured,
                                    const SdfPath &materialPath) const;

    SdfPath _primPath;
    /// Cached between `UpdateDependencies` and `Update` so both agree on which
    /// camera this cook is about.
    SdfPath _cameraPath;

    /// Cooks served by this instance — the other half of the plugin's
    /// construction count.
    uint64_t _cooks = 0;

    /// The session this prim reads, owned by a process-wide registry rather
    /// than by this instance — see `_SharedSession` in the implementation.
    /// Null until the first cook resolves it.
    struct _SharedSession *_shared = nullptr;
    /// The frame being read, alive only for the duration of one `Update`.
    ///
    /// Freed before that cook returns: everything it lends is copied into the
    /// prims below and into the resolver's texture store first. It used to
    /// survive until the *next* cook, because `GetChildPrim` read from it —
    /// which was safe only as long as nothing was ever kept.
    TuileFrame *_frame = nullptr;

    GfVec3d _renderOrigin = GfVec3d(0.0);
    double _fallbackViewportPx[2] = {1920.0, 1440.0};

    /// The render origin the prims below were built against.
    ///
    /// It is authored per trajectory, not per frame, so it almost never
    /// changes — but every child's transform is `origin − renderOrigin`, so
    /// when it does, every one of them is stale and has to be rebuilt.
    GfVec3d _builtOrigin = GfVec3d(0.0);
    bool _haveBuilt = false;

    /// One tile, **built**, kept across cooks.
    ///
    /// This is what makes a cook incremental. hdGp emits nothing at all for a
    /// child re-declared with the same path and type — it does not even call
    /// `GetChildPrim` again — so a tile that stays in the selection costs
    /// exactly nothing, provided its prim is still here to be handed back.
    struct _Tile
    {
        uint64_t id = 0;
        /// What its imagery was composed from. The same tile re-draped keeps
        /// its path and needs new pixels: the only change a kept prim can
        /// undergo, and the reason this is remembered.
        uint64_t drape = 0;
        bool textured = false;
        /// The first texture's `tuile://` URI, empty when untextured.
        std::string textureUri;
        /// Built once, handed out unchanged afterwards.
        HdSceneIndexPrim prim;
    };
    std::map<SdfPath, _Tile> _tilesByPath;
    /// Material child path -> its built prim. Same lifetime rules.
    std::map<SdfPath, _Tile> _materialsByPath;
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif
