// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_PROCEDURAL_H
#define TUILE_HYDRA_PROCEDURAL_H

#include <pxr/pxr.h>
#include <pxr/imaging/hdGp/generativeProcedural.h>
#include <pxr/usd/sdf/path.h>

PXR_NAMESPACE_OPEN_SCOPE

/// Emits terrain tiles for whichever camera the scene says to render through.
///
/// # Why a camera dependency is legitimate here
///
/// Hydra's contract is that a scene can be re-rendered from any camera without
/// changing, and level-of-detail selection appears to break it. It does not,
/// because the camera we refine for is *named by the scene*, not taken from a
/// viewport: the geometry stays a function of (stage, time), which is what
/// makes a frame reproducible on a farm.
///
/// This is a sanctioned use. OpenUSD 25.05 added `primaryCameraPrim` to
/// `HdSceneGlobalsSchema` with the note that it "is intended for use by scene
/// indexes that want to do camera-dependent scene transformations, filtering,
/// or generation", and `UpdateDependencies` returns a map keyed by arbitrary
/// `SdfPath`, so depending on another prim's transform is a first-class
/// feature rather than a trick.
///
/// # Which camera
///
/// Resolved in this order, most authored first, because each step down is one
/// step further from something a pipeline can pin:
///
///  1. `tuile:cameras` on this prim — explicit, and the only way to refine for
///     several views at once (a stereo pair must share one selection or the
///     eyes disagree at LOD boundaries);
///  2. the active render settings' camera, which also carries the resolution
///     needed to turn a geometric error into a screen-space one;
///  3. `HdSceneGlobalsSchema`'s primary camera — what `usdrecord` sets, and
///     what a viewport points at its free camera, so deterministic in batch and
///     not in a viewport;
///  4. nothing, and we fall back to a fixed geometric error. That mode is
///     view-*independent*, which makes the asset usable by someone who opens
///     the stage knowing nothing about us.
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

    /// Distance from the resolved camera to the origin, or a fixed value when
    /// no camera resolves.
    double _CameraDistance(const HdSceneIndexBaseRefPtr &scene) const;

    /// How finely to grid the patch at that distance. Static and pure so that
    /// `Update`, which names the prim from it, and `GetChildPrim`, which builds
    /// the geometry from it, cannot disagree.
    static int _Divisions(double distance);

    SdfPath _primPath;
    /// Cached between `UpdateDependencies` and `Update` so both agree on which
    /// camera this cook is about.
    SdfPath _cameraPath;
    /// The child emitted by the last `Update`. Its name encodes the refinement,
    /// so a change of refinement removes one prim and adds another instead of
    /// mutating one in place — see the comment in `Update` for why that is not
    /// optional.
    SdfPath _childPath;
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif
