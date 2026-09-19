// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "view.h"

#include "hostpaths.h"

#include <pxr/base/gf/matrix4d.h>
#include <pxr/base/gf/vec3d.h>
#include <pxr/imaging/hd/cameraSchema.h>
#include <pxr/imaging/hd/renderProductSchema.h>
#include <pxr/imaging/hd/renderSettingsSchema.h>
#include <pxr/imaging/hd/sceneGlobalsSchema.h>
#include <pxr/imaging/hd/xformSchema.h>

#include <cmath>

PXR_NAMESPACE_OPEN_SCOPE

namespace {

/// The camera's transform in world space, accumulated up the prim tree.
///
/// `HdXformSchema::GetMatrix()` gives a prim's **local** transform, and a scene
/// index is only flattened if somebody put an `HdFlatteningSceneIndex` in the
/// chain. Blender's USD export writes a camera as an Xform holding a Camera —
/// `/shot/shot` — so the pose lives on the parent and the camera prim itself is
/// identity.
///
/// What that cost: `Read` saw an identity matrix, correctly concluded that
/// nothing had been authored, and returned false. The procedural then fell back
/// to its view-independent mode — straight down from the render origin, at the
/// centre of the orbit — and every baked frame was 8000 m away, that being the
/// orbit's radius. The camera had arrived; only its pose had been left behind.
///
/// `resetXformStack` stops the climb, which is what it is for: a prim that
/// declares it is expressed in world space already.
GfMatrix4d
_WorldTransform(const HdSceneIndexBaseRefPtr &scene,
                const SdfPath &path,
                const HdMatrixDataSourceHandle &localDs,
                const HdXformSchema &localXform)
{
    GfMatrix4d accumulated(1.0);
    if (localDs) {
        accumulated = localDs->GetTypedValue(0.0f);
    }
    if (localXform) {
        if (HdBoolDataSourceHandle reset = localXform.GetResetXformStack()) {
            if (reset->GetTypedValue(0.0f)) {
                return accumulated;
            }
        }
    }
    for (SdfPath at = path.GetParentPath(); !at.IsEmpty() && at != SdfPath::AbsoluteRootPath();
         at = at.GetParentPath())
    {
        HdSceneIndexPrim prim = scene->GetPrim(at);
        HdXformSchema xform = HdXformSchema::GetFromParent(prim.dataSource);
        if (!xform) {
            continue;
        }
        if (HdMatrixDataSourceHandle ds = xform.GetMatrix()) {
            // Row-vector convention, as the rest of this file reads it: a child
            // is expressed in its parent, so local comes first.
            accumulated = accumulated * ds->GetTypedValue(0.0f);
        }
        if (HdBoolDataSourceHandle reset = xform.GetResetXformStack()) {
            if (reset->GetTypedValue(0.0f)) {
                break;
            }
        }
    }
    return accumulated;
}

}  // namespace

bool
TuileViewFromCamera::Read(
    const HdSceneIndexBaseRefPtr &scene,
    const SdfPath &cameraPath,
    const SdfPath &anchor,
    const double renderOrigin[3],
    const double fallbackViewportPx[2],
    TuileViewState *out)
{
    if (!out || cameraPath.IsEmpty()) {
        return false;
    }
    HdSceneIndexPrim camera = scene->GetPrim(cameraPath);

    // Pas de xform SUR la caméra ? Ce n'est pas une panne : c'est le cas
    // normal.
    //
    // Blender exporte une caméra comme un Xform contenant une Camera —
    // `/shot/shot`, vu le 16 septembre 2026 — et la pose est sur le parent. La
    // prim caméra elle-même n'a rien du tout : type `camera`, dataSource
    // présent, aucun xform. La version qui abandonnait ici retombait sur la
    // vue indépendante au centre de l'orbite, et le pack répondait « the
    // nearest is frame 1108, 8000.000 m away » — le rayon de l'orbite, parce
    // que la caméra était à son centre.
    //
    // On part donc de l'identité et on laisse `_WorldTransform` remonter
    // l'arbre. Si personne n'a rien écrit nulle part, le test d'identité plus
    // bas le dira — c'est lui qui distingue « rien d'écrit » de « écrit et
    // lu ».
    HdXformSchema xform = HdXformSchema::GetFromParent(camera.dataSource);
    HdMatrixDataSourceHandle matrixDs = xform ? xform.GetMatrix() : nullptr;
    const GfMatrix4d m = _WorldTransform(scene, cameraPath, matrixDs, xform);

    // The camera's basis, read out of the matrix rather than assumed: rows are
    // the local axes in world space (row-vector convention). USD cameras look
    // down -Z with +Y up in their own space.
    const GfVec3d right(m[0][0], m[0][1], m[0][2]);
    const GfVec3d up(m[1][0], m[1][1], m[1][2]);
    const GfVec3d back(m[2][0], m[2][1], m[2][2]);
    const GfVec3d translation = m.ExtractTranslation();

    // An identity transform means "nobody authored anything", and a view built
    // from it selects tiles at the centre of the Earth. Distinguished from an
    // authored identity by not needing to be: no camera on a globe legitimately
    // sits at the origin with axis-aligned basis.
    if (translation == GfVec3d(0.0) && right == GfVec3d(1.0, 0.0, 0.0) &&
        up == GfVec3d(0.0, 1.0, 0.0))
    {
        printf("CAMERA-READ-FAILED %s: transform is identity — nothing was "
               "authored on it, nor on any ancestor\n", cameraPath.GetText());
        fflush(stdout);
        return false;
    }
    const GfVec3d direction = -back;
    if (direction.GetLength() < 1e-12 || up.GetLength() < 1e-12) {
        printf("CAMERA-READ-FAILED %s: degenerate basis\n", cameraPath.GetText());
        fflush(stdout);
        return false;
    }

    // The vertical field of view is the aperture/focal ratio — both authored
    // in the same unit, so no tenths-of-unit conversion applies.
    double fovy = 45.0 * M_PI / 180.0;
    if (HdCameraSchema cam = HdCameraSchema::GetFromParent(camera.dataSource)) {
        double aperture = 0.0;
        double focal = 0.0;
        if (HdFloatDataSourceHandle a = cam.GetVerticalAperture()) {
            aperture = a->GetTypedValue(0.0f);
        }
        if (HdFloatDataSourceHandle f = cam.GetFocalLength()) {
            focal = f->GetTypedValue(0.0f);
        }
        if (aperture > 0.0 && focal > 0.0) {
            fovy = 2.0 * std::atan(aperture / (2.0 * focal));
        }
    }

    double viewport[2] = {fallbackViewportPx[0], fallbackViewportPx[1]};
    ResolutionFromRenderSettings(scene, anchor, viewport);
    if (!(viewport[0] > 0.0) || !(viewport[1] > 0.0)) {
        printf("CAMERA-READ-FAILED %s: viewport is %gx%g\n",
               cameraPath.GetText(), viewport[0], viewport[1]);
        fflush(stdout);
        return false;
    }

    for (int i = 0; i < 3; ++i) {
        // The one addition the rebasing protocol allows: origin back onto the
        // rebased translation, in double, to recover true ECEF.
        out->position[i] = translation[i] + renderOrigin[i];
        out->direction[i] = direction[i];
        out->up[i] = up[i];
    }
    out->viewport_px[0] = viewport[0];
    out->viewport_px[1] = viewport[1];
    out->fovy_rad = fovy;
    return true;
}

bool
TuileViewFromCamera::ResolutionFromRenderSettings(
    const HdSceneIndexBaseRefPtr &scene,
    const SdfPath &anchor,
    double out[2])
{
    HdSceneGlobalsSchema globals = TuileSceneGlobals(scene, anchor);
    if (!globals) {
        return false;
    }
    HdPathDataSourceHandle settingsPathDs = globals.GetActiveRenderSettingsPrim();
    if (!settingsPathDs) {
        return false;
    }
    const SdfPath settingsPath = settingsPathDs->GetTypedValue(0.0f);
    if (settingsPath.IsEmpty()) {
        return false;
    }
    HdSceneIndexPrim settings = scene->GetPrim(settingsPath);
    HdRenderSettingsSchema rs =
        HdRenderSettingsSchema::GetFromParent(settings.dataSource);
    if (!rs) {
        return false;
    }
    HdRenderProductVectorSchema products = rs.GetRenderProducts();
    if (!products) {
        return false;
    }
    for (size_t i = 0; i < products.GetNumElements(); ++i) {
        HdRenderProductSchema product = products.GetElement(i);
        if (!product) {
            continue;
        }
        if (HdVec2iDataSourceHandle resolution = product.GetResolution()) {
            const GfVec2i r = resolution->GetTypedValue(0.0f);
            if (r[0] > 0 && r[1] > 0) {
                out[0] = r[0];
                out[1] = r[1];
                return true;
            }
        }
    }
    return false;
}

PXR_NAMESPACE_CLOSE_SCOPE
