// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_VIEW_H
#define TUILE_HYDRA_VIEW_H

#include "ffi.h"

#include <pxr/pxr.h>
#include <pxr/imaging/hd/sceneIndex.h>
#include <pxr/usd/sdf/path.h>

PXR_NAMESPACE_OPEN_SCOPE

/// Turns a camera prim into the view the traversal selects tiles against.
///
/// This is the half of "which camera" that the resolution ladder does not do.
/// The ladder answers *which prim*; this answers *what the traversal is given*,
/// and getting it wrong is quiet: the tiles are all valid, just refined for a
/// view that is not the one being rendered. Too coarse looks like a slow
/// network; too fine looks like the cache is broken.
///
/// # The three quantities, and where each comes from
///
/// **Position, direction, up** come from the camera's world transform. USD
/// cameras look down **-Z** with **+Y** up in their own space, so the basis is
/// read out of the matrix rather than assumed to be axis-aligned.
///
/// **The vertical field of view** is `2 * atan(verticalAperture / (2 *
/// focalLength))`. Both are authored in the same unit, so the ratio is
/// unit-free and no conversion is needed — which is why this does not care that
/// USD apertures are conventionally in tenths of a scene unit.
///
/// **The viewport in pixels** is what converts a geometric error into a
/// screen-space one, so it must be the *render* resolution and not a guess. It
/// comes from the render product when one resolves; the caller supplies a
/// fallback for when none does.
///
/// # Precision
///
/// Everything is f64 and ECEF. The camera transform is already a `GfMatrix4d`,
/// so nothing is narrowed on the way in — narrowing a position on the globe to
/// float is what jitter is made of.
struct TuileViewFromCamera
{
    /// Reads `cameraPath` out of `scene`. Returns false when the prim has no
    /// usable transform, which is the one case a caller must not paper over: a
    /// view built from an identity matrix selects tiles at the centre of the
    /// Earth.
    static bool Read(
        const HdSceneIndexBaseRefPtr &scene,
        const SdfPath &cameraPath,
        const double fallbackViewportPx[2],
        TuileViewState *out);

    /// The render resolution the active render settings state, if any.
    ///
    /// Returns false when nothing resolves — a viewport session, or a stage
    /// with no render settings — and the caller falls back rather than
    /// inventing a resolution. Inventing one silently changes the level of
    /// detail, so it is worth knowing which happened.
    static bool ResolutionFromRenderSettings(
        const HdSceneIndexBaseRefPtr &scene,
        double out[2]);
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif
