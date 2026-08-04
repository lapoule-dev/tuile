// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "procedural.h"

#include <pxr/base/gf/matrix4d.h>
#include <pxr/base/gf/vec3d.h>
#include <pxr/base/gf/vec3f.h>
#include <pxr/base/tf/debug.h>
#include <pxr/base/tf/diagnostic.h>
#include <pxr/base/tf/registryManager.h>
#include <pxr/base/tf/staticTokens.h>
#include <pxr/base/tf/stringUtils.h>
#include <pxr/base/vt/array.h>
#include <pxr/imaging/hd/cameraSchema.h>
#include <pxr/imaging/hd/meshSchema.h>
#include <pxr/imaging/hd/meshTopologySchema.h>
#include <pxr/imaging/hd/primvarSchema.h>
#include <pxr/imaging/hd/primvarsSchema.h>
#include <pxr/imaging/hd/renderProductSchema.h>
#include <pxr/imaging/hd/renderSettingsSchema.h>
#include <pxr/imaging/hd/retainedDataSource.h>
#include <pxr/imaging/hd/sceneGlobalsSchema.h>
#include <pxr/imaging/hd/sceneIndex.h>
#include <pxr/imaging/hd/tokens.h>
#include <pxr/imaging/hd/xformSchema.h>
#include <pxr/imaging/pxOsd/tokens.h>

#include <algorithm>
#include <cmath>

PXR_NAMESPACE_OPEN_SCOPE

// Enable with TF_DEBUG=TUILE_HYDRA_PROCEDURAL. Kept in the shipped plugin
// rather than removed after the fact: the failure this was written to diagnose
// — a procedural that cooks once and then silently stops — produces a plausible
// image and no error at all, so the next person to hit it needs a way in.
TF_DEBUG_CODES(TUILE_HYDRA_PROCEDURAL);
TF_REGISTRY_FUNCTION(TfDebug)
{
    TF_DEBUG_ENVIRONMENT_SYMBOL(
        TUILE_HYDRA_PROCEDURAL,
        "tuile: generative procedural cooking and camera resolution");
}

TF_DEFINE_PRIVATE_TOKENS(
    _tokens,
    ((cameras, "tuile:cameras"))
    ((tileRoot, "tiles"))
    (displayColor)
);

namespace {

/// Reads a relationship-like path argument off our own prim.
///
/// Procedural arguments arrive as primvars on the procedural prim — that is how
/// hdGp passes configuration — so a "relationship" is a path-valued primvar
/// rather than an `SdfRelationship`.
SdfPath
_PathArg(const HdSceneIndexBaseRefPtr &scene,
         const SdfPath &primPath,
         const TfToken &name)
{
    HdSceneIndexPrim prim = scene->GetPrim(primPath);
    HdPrimvarsSchema primvars = HdPrimvarsSchema::GetFromParent(prim.dataSource);
    if (!primvars) {
        return SdfPath();
    }
    HdPrimvarSchema primvar = primvars.GetPrimvar(name);
    if (!primvar) {
        return SdfPath();
    }
    HdSampledDataSourceHandle value = primvar.GetPrimvarValue();
    if (!value) {
        return SdfPath();
    }
    // Shutter offset 0: the argument is which camera, not where it is, and
    // that does not vary within a frame.
    const VtValue v = value->GetValue(0.0f);
    if (v.IsHolding<SdfPath>()) {
        return v.UncheckedGet<SdfPath>();
    }
    if (v.IsHolding<VtArray<SdfPath>>()) {
        const auto &paths = v.UncheckedGet<VtArray<SdfPath>>();
        return paths.empty() ? SdfPath() : paths[0];
    }
    return SdfPath();
}

/// The camera named by the active render settings, if any.
///
/// Deliberately tolerant: `active` is computed by a filtering scene index that
/// not every host inserts, so a settings prim that does not claim to be active
/// is still worth reading rather than treating as absent.
SdfPath
_RenderSettingsCamera(const HdSceneIndexBaseRefPtr &scene)
{
    HdSceneGlobalsSchema globals = HdSceneGlobalsSchema::GetFromSceneIndex(scene);
    if (!globals) {
        return SdfPath();
    }
    HdPathDataSourceHandle settingsPathDs = globals.GetActiveRenderSettingsPrim();
    if (!settingsPathDs) {
        return SdfPath();
    }
    const SdfPath settingsPath = settingsPathDs->GetTypedValue(0.0f);
    if (settingsPath.IsEmpty()) {
        return SdfPath();
    }

    HdSceneIndexPrim settings = scene->GetPrim(settingsPath);
    HdRenderSettingsSchema rs =
        HdRenderSettingsSchema::GetFromParent(settings.dataSource);
    if (!rs) {
        return SdfPath();
    }
    if (HdPathDataSourceHandle camera = rs.GetCamera()) {
        const SdfPath path = camera->GetTypedValue(0.0f);
        if (!path.IsEmpty()) {
            return path;
        }
    }

    // The settings-level camera is optional; a product may name its own, and
    // that product is also where `resolution` lives — which a real
    // screen-space error needs. Taking the first is a genuine limitation: with
    // several products at different resolutions there is no single right
    // answer, and the fix is to author `tuile:cameras` explicitly.
    HdRenderProductVectorSchema products = rs.GetRenderProducts();
    if (!products) {
        return SdfPath();
    }
    for (size_t i = 0; i < products.GetNumElements(); ++i) {
        HdRenderProductSchema product = products.GetElement(i);
        if (!product) {
            continue;
        }
        if (HdPathDataSourceHandle camera = product.GetCameraPrim()) {
            const SdfPath path = camera->GetTypedValue(0.0f);
            if (!path.IsEmpty()) {
                return path;
            }
        }
    }
    return SdfPath();
}

/// Where a camera is, in world space.
GfVec3d
_CameraPosition(const HdSceneIndexBaseRefPtr &scene, const SdfPath &cameraPath)
{
    HdSceneIndexPrim camera = scene->GetPrim(cameraPath);
    HdXformSchema xform = HdXformSchema::GetFromParent(camera.dataSource);
    if (!xform) {
        return GfVec3d(0.0);
    }
    HdMatrixDataSourceHandle matrixDs = xform.GetMatrix();
    if (!matrixDs) {
        return GfVec3d(0.0);
    }
    return matrixDs->GetTypedValue(0.0f).ExtractTranslation();
}

}  // namespace

/// How finely to grid the patch at a given camera distance.
///
/// Shared by Update, which needs it to name the prim, and GetChildPrim, which
/// needs it to build the geometry. Computing it twice from one function is what
/// keeps the name and the contents describing the same thing.
int
TuileGlobeProcedural::_Divisions(double distance)
{
    return static_cast<int>(std::clamp(200.0 / std::max(distance, 1.0), 1.0, 64.0));
}

double
TuileGlobeProcedural::_CameraDistance(const HdSceneIndexBaseRefPtr &scene) const
{
    // No camera resolves: a fixed distance, which is the view-independent mode
    // rather than a failure.
    const GfVec3d eye = _cameraPath.IsEmpty()
        ? GfVec3d(0.0, 0.0, 100.0)
        : _CameraPosition(scene, _cameraPath);
    return std::max(eye.GetLength(), 1.0);
}

TuileGlobeProcedural::TuileGlobeProcedural(const SdfPath &proceduralPrimPath)
    : HdGpGenerativeProcedural(proceduralPrimPath)
    , _primPath(proceduralPrimPath)
{
}

TuileGlobeProcedural::~TuileGlobeProcedural() = default;

SdfPath
TuileGlobeProcedural::_ResolveCamera(const HdSceneIndexBaseRefPtr &inputScene) const
{
    // 1. Authored on this prim: the only choice a pipeline can pin.
    if (SdfPath explicitCamera = _PathArg(inputScene, _primPath, _tokens->cameras);
        !explicitCamera.IsEmpty()) {
        return explicitCamera;
    }

    // 2. The camera this render product renders through.
    if (SdfPath fromSettings = _RenderSettingsCamera(inputScene);
        !fromSettings.IsEmpty()) {
        return fromSettings;
    }

    // 3. Whatever the host calls primary — a viewport's free camera, in an
    //    interactive session, so useful but not reproducible.
    if (HdSceneGlobalsSchema globals =
            HdSceneGlobalsSchema::GetFromSceneIndex(inputScene)) {
        if (HdPathDataSourceHandle primary = globals.GetPrimaryCameraPrim()) {
            return primary->GetTypedValue(0.0f);
        }
    }

    // 4. None. The caller falls back to a fixed geometric error, which is the
    //    view-independent mode rather than a failure.
    return SdfPath();
}

HdGpGenerativeProcedural::DependencyMap
TuileGlobeProcedural::UpdateDependencies(const HdSceneIndexBaseRefPtr &inputScene)
{
    DependencyMap result;
    _cameraPath = _ResolveCamera(inputScene);

    if (!_cameraPath.IsEmpty()) {
        // The transform is what changes as the shot moves; the camera schema
        // covers a lens change. Declaring both is what makes Hydra re-cook us
        // instead of leaving stale geometry on screen.
        result[_cameraPath] = {
            HdXformSchema::GetDefaultLocator(),
            HdCameraSchema::GetDefaultLocator(),
        };
    }

    // The frame number, so a scrub re-cooks even when the camera is static.
    // Without this a stage whose camera never moves would refine once and then
    // ignore the timeline entirely.
    result[HdSceneGlobalsSchema::GetDefaultPrimPath()] = {
        HdSceneGlobalsSchema::GetCurrentFrameLocator(),
    };

    return result;
}

HdGpGenerativeProcedural::ChildPrimTypeMap
TuileGlobeProcedural::Update(
    const HdSceneIndexBaseRefPtr &inputScene,
    const ChildPrimTypeMap &previousResult,
    const DependencyMap &dirtiedDependencies,
    HdSceneIndexObserver::DirtiedPrimEntries *outputDirtiedPrims)
{
    (void)dirtiedDependencies;

    ChildPrimTypeMap result;
    // The child's PATH encodes its refinement, so a change of refinement is a
    // different prim rather than the same prim mutated.
    //
    // This is not a stylistic choice, it is what works. Dirtying a mesh whose
    // *vertex count* changed is not enough: traced against hdEmbree, the
    // procedural re-cooked correctly and produced the finer grid, the renderer
    // re-read the topology, and the frame came out empty — a 289-vertex
    // topology indexing a 4-vertex point buffer it never re-read. Removing one
    // prim and adding another leaves no stale buffer to disagree with.
    //
    // It is also what the real thing does. Tiles appear and disappear as the
    // view moves; they do not mutate in place, so the path carrying the tile's
    // identity is the honest model rather than a workaround.
    _childPath = _primPath.AppendChild(_tokens->tileRoot)
                     .AppendChild(TfToken(TfStringPrintf("grid_%d", _Divisions(
                         _CameraDistance(inputScene)))));
    result[_childPath] = HdPrimTypeTokens->mesh;

    if (TfDebug::IsEnabled(TUILE_HYDRA_PROCEDURAL)) {
        TF_DEBUG(TUILE_HYDRA_PROCEDURAL).Msg(
            "[tuile] Update: camera=%s previous=%zu dirtied=%zu\n",
            _cameraPath.GetText(), previousResult.size(),
            dirtiedDependencies.size());
    }

    // Nothing is dirtied here on purpose. hdGp reads the returned map and does
    // the work itself: a path absent from it is removed, a path new to it is
    // added. Dirtying would only matter for a prim that kept its path, and by
    // construction none does.
    (void)previousResult;
    (void)outputDirtiedPrims;
    return result;
}

HdSceneIndexPrim
TuileGlobeProcedural::GetChildPrim(
    const HdSceneIndexBaseRefPtr &inputScene,
    const SdfPath &childPrimPath)
{
    if (childPrimPath != _childPath) {
        return HdSceneIndexPrim();
    }

    // The spike's stand-in for a tile pyramid: one patch whose subdivision
    // follows the camera's distance. It proves the only thing that is in doubt
    // — that we are re-cooked with a current camera — and nothing else.
    const double distance = _CameraDistance(inputScene);
    const int divisions = _Divisions(distance);
    TF_DEBUG(TUILE_HYDRA_PROCEDURAL).Msg(
        "[tuile] GetChildPrim: %s camera=%s distance=%.1f divisions=%d\n",
        childPrimPath.GetText(), _cameraPath.GetText(), distance, divisions);

    VtVec3fArray points;
    VtIntArray faceVertexCounts;
    VtIntArray faceVertexIndices;
    points.reserve((divisions + 1) * (divisions + 1));

    // A dome, not a plane, and that choice is the test rather than decoration.
    // Subdividing a flat quad changes the triangle count and nothing a renderer
    // can show — the silhouette is identical at 1 division and at 64, so an
    // image comparison would pass on a couple of anti-aliased pixels whether or
    // not the procedural was ever re-cooked. Displacing the grid makes the
    // refinement visible: one division is a flat quad through the corners,
    // sixteen is a recognisable curved surface.
    for (int row = 0; row <= divisions; ++row) {
        for (int col = 0; col <= divisions; ++col) {
            const float u = static_cast<float>(col) / divisions;
            const float v = static_cast<float>(row) / divisions;
            const float x = u * 2.0f - 1.0f;
            const float y = v * 2.0f - 1.0f;
            const float r2 = x * x + y * y;
            const float z = r2 < 1.0f ? std::sqrt(1.0f - r2) : 0.0f;
            points.push_back(GfVec3f(x, y, z));
        }
    }
    for (int row = 0; row < divisions; ++row) {
        for (int col = 0; col < divisions; ++col) {
            const int base = row * (divisions + 1) + col;
            faceVertexCounts.push_back(4);
            faceVertexIndices.push_back(base);
            faceVertexIndices.push_back(base + 1);
            faceVertexIndices.push_back(base + divisions + 2);
            faceVertexIndices.push_back(base + divisions + 1);
        }
    }

    HdSceneIndexPrim prim;
    prim.primType = HdPrimTypeTokens->mesh;
    prim.dataSource = HdRetainedContainerDataSource::New(
        HdMeshSchemaTokens->mesh,
        HdMeshSchema::Builder()
            .SetTopology(
                HdMeshTopologySchema::Builder()
                    .SetFaceVertexCounts(
                        HdRetainedTypedSampledDataSource<VtIntArray>::New(
                            faceVertexCounts))
                    .SetFaceVertexIndices(
                        HdRetainedTypedSampledDataSource<VtIntArray>::New(
                            faceVertexIndices))
                    .Build())
            // Polygonal, not subdivided. Hydra's default is catmullClark,
            // which would smooth the grid into the same shape at every
            // division count — hiding the very thing this spike measures.
            .SetSubdivisionScheme(
                HdRetainedTypedSampledDataSource<TfToken>::New(
                    PxOsdOpenSubdivTokens->none))
            .Build(),
        HdPrimvarsSchemaTokens->primvars,
        HdRetainedContainerDataSource::New(
            HdPrimvarsSchemaTokens->points,
            HdPrimvarSchema::Builder()
                .SetPrimvarValue(
                    HdRetainedTypedSampledDataSource<VtVec3fArray>::New(points))
                .SetInterpolation(
                    HdPrimvarSchema::BuildInterpolationDataSource(
                        HdPrimvarSchemaTokens->vertex))
                .SetRole(
                    HdPrimvarSchema::BuildRoleDataSource(
                        HdPrimvarSchemaTokens->point))
                .Build(),
            // Without a colour the surface is rendered white on a white
            // background: present, correct, and invisible. A displayColor is
            // the cheapest material a mesh can carry — no material network, no
            // shader — and it is what makes the image worth comparing.
            _tokens->displayColor,
            HdPrimvarSchema::Builder()
                .SetPrimvarValue(
                    HdRetainedTypedSampledDataSource<VtVec3fArray>::New(
                        VtVec3fArray{GfVec3f(0.15f, 0.45f, 0.85f)}))
                .SetInterpolation(
                    HdPrimvarSchema::BuildInterpolationDataSource(
                        HdPrimvarSchemaTokens->constant))
                .SetRole(
                    HdPrimvarSchema::BuildRoleDataSource(
                        HdPrimvarSchemaTokens->color))
                .Build()));

    return prim;
}

PXR_NAMESPACE_CLOSE_SCOPE
