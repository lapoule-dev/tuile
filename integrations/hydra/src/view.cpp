// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "view.h"

#include <pxr/base/gf/matrix4d.h>
#include <pxr/base/gf/vec3d.h>
#include <pxr/imaging/hd/cameraSchema.h>
#include <pxr/imaging/hd/renderProductSchema.h>
#include <pxr/imaging/hd/renderSettingsSchema.h>
#include <pxr/imaging/hd/sceneGlobalsSchema.h>
#include <pxr/imaging/hd/xformSchema.h>

#include <cmath>

PXR_NAMESPACE_OPEN_SCOPE

bool
TuileViewFromCamera::Read(
    const HdSceneIndexBaseRefPtr &scene,
    const SdfPath &cameraPath,
    const double renderOrigin[3],
    const double fallbackViewportPx[2],
    TuileViewState *out)
{
    if (!out || cameraPath.IsEmpty()) {
        return false;
    }
    HdSceneIndexPrim camera = scene->GetPrim(cameraPath);

    HdXformSchema xform = HdXformSchema::GetFromParent(camera.dataSource);
    if (!xform) {
        return false;
    }
    HdMatrixDataSourceHandle matrixDs = xform.GetMatrix();
    if (!matrixDs) {
        return false;
    }
    const GfMatrix4d m = matrixDs->GetTypedValue(0.0f);

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
        return false;
    }
    const GfVec3d direction = -back;
    if (direction.GetLength() < 1e-12 || up.GetLength() < 1e-12) {
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
    ResolutionFromRenderSettings(scene, viewport);
    if (!(viewport[0] > 0.0) || !(viewport[1] > 0.0)) {
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
    double out[2])
{
    HdSceneGlobalsSchema globals = HdSceneGlobalsSchema::GetFromSceneIndex(scene);
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
