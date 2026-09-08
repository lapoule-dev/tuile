// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "procedural.h"

#include "tiles.h"
#include "view.h"

#include <pxr/base/gf/matrix4d.h>
#include <pxr/base/gf/vec2d.h>
#include <pxr/base/gf/vec3d.h>
#include <pxr/base/gf/vec3f.h>
#include <pxr/base/tf/debug.h>
#include <pxr/base/tf/diagnostic.h>
#include <pxr/base/tf/getenv.h>
#include <pxr/base/tf/registryManager.h>
#include <pxr/base/tf/staticTokens.h>
#include <pxr/base/tf/stringUtils.h>
#include <pxr/base/vt/array.h>
#include <pxr/imaging/hd/cameraSchema.h>
#include <pxr/imaging/hd/materialBindingSchema.h>
#include <pxr/imaging/hd/materialBindingsSchema.h>
#include <pxr/imaging/hd/materialConnectionSchema.h>
#include <pxr/imaging/hd/materialNetworkSchema.h>
#include <pxr/imaging/hd/materialNodeParameterSchema.h>
#include <pxr/imaging/hd/materialNodeSchema.h>
#include <pxr/imaging/hd/materialSchema.h>
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
#include <pxr/usd/sdf/assetPath.h>

#include <algorithm>
#include <cmath>
#include <cstring>

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
    ((renderOrigin, "tuile:renderOrigin"))
    ((terrainAssetId, "tuile:terrainAssetId"))
    ((imageryAssetId, "tuile:imageryAssetId"))
    ((maxSse, "tuile:maxSse"))
    ((viewportPx, "tuile:viewportPx"))
    ((tileRoot, "tiles"))
    ((materialRoot, "materials"))
    (displayColor)
    (st)
    (surface)
    (rgb)
    (result)
    (file)
    (varname)
    (diffuseColor)
    (roughness)
    (wrapS)
    (wrapT)
    ((clampWrap, "clamp"))
    (UsdPreviewSurface)
    (UsdUVTexture)
    (UsdPrimvarReader_float2)
    (PreviewSurface)
    (Texture)
    (StReader)
);

namespace {

/// A primvar's value off a prim, at shutter 0 — procedural arguments arrive as
/// primvars, and none of them vary within a frame.
VtValue
_PrimvarValue(const HdSceneIndexBaseRefPtr &scene,
              const SdfPath &primPath,
              const TfToken &name)
{
    HdSceneIndexPrim prim = scene->GetPrim(primPath);
    HdPrimvarsSchema primvars = HdPrimvarsSchema::GetFromParent(prim.dataSource);
    if (!primvars) {
        return VtValue();
    }
    HdPrimvarSchema primvar = primvars.GetPrimvar(name);
    if (!primvar) {
        return VtValue();
    }
    HdSampledDataSourceHandle value = primvar.GetPrimvarValue();
    if (!value) {
        return VtValue();
    }
    return value->GetValue(0.0f);
}

/// Reads a relationship-like path argument off our own prim: a "relationship"
/// is a path-valued primvar, because that is how hdGp passes configuration.
SdfPath
_PathArg(const HdSceneIndexBaseRefPtr &scene,
         const SdfPath &primPath,
         const TfToken &name)
{
    const VtValue v = _PrimvarValue(scene, primPath, name);
    if (v.IsHolding<SdfPath>()) {
        return v.UncheckedGet<SdfPath>();
    }
    if (v.IsHolding<VtArray<SdfPath>>()) {
        const auto &paths = v.UncheckedGet<VtArray<SdfPath>>();
        return paths.empty() ? SdfPath() : paths[0];
    }
    return SdfPath();
}

/// Numeric primvars arrive as whatever the stage authored (int, double, or a
/// one-element array of either); read them all as double.
double
_DoubleArg(const HdSceneIndexBaseRefPtr &scene,
           const SdfPath &primPath,
           const TfToken &name,
           double fallback)
{
    const VtValue v = _PrimvarValue(scene, primPath, name);
    if (v.IsEmpty()) {
        return fallback;
    }
    if (v.CanCast<double>()) {
        return VtValue::Cast<double>(v).UncheckedGet<double>();
    }
    if (v.IsHolding<VtDoubleArray>() && !v.UncheckedGet<VtDoubleArray>().empty()) {
        return v.UncheckedGet<VtDoubleArray>()[0];
    }
    if (v.IsHolding<VtIntArray>() && !v.UncheckedGet<VtIntArray>().empty()) {
        return v.UncheckedGet<VtIntArray>()[0];
    }
    return fallback;
}

GfVec3d
_Vec3dArg(const HdSceneIndexBaseRefPtr &scene,
          const SdfPath &primPath,
          const TfToken &name,
          const GfVec3d &fallback)
{
    const VtValue v = _PrimvarValue(scene, primPath, name);
    if (v.IsHolding<GfVec3d>()) {
        return v.UncheckedGet<GfVec3d>();
    }
    if (v.IsHolding<VtVec3dArray>() && !v.UncheckedGet<VtVec3dArray>().empty()) {
        return v.UncheckedGet<VtVec3dArray>()[0];
    }
    return fallback;
}

GfVec2d
_Vec2dArg(const HdSceneIndexBaseRefPtr &scene,
          const SdfPath &primPath,
          const TfToken &name,
          const GfVec2d &fallback)
{
    const VtValue v = _PrimvarValue(scene, primPath, name);
    if (v.IsHolding<GfVec2d>()) {
        return v.UncheckedGet<GfVec2d>();
    }
    if (v.IsHolding<VtVec2dArray>() && !v.UncheckedGet<VtVec2dArray>().empty()) {
        return v.UncheckedGet<VtVec2dArray>()[0];
    }
    return fallback;
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
#if PXR_VERSION >= 2608
    // The settings-level camera only exists from 26.08; on 25.08 (the
    // Blender fork's USD) rung 2 reads the per-product camera below.
    if (HdPathDataSourceHandle camera = rs.GetCamera()) {
        const SdfPath path = camera->GetTypedValue(0.0f);
        if (!path.IsEmpty()) {
            return path;
        }
    }
#endif

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

/// A float-buffer borrow as a typed VtArray copy. Copying is the point: the
/// borrow dies with the frame, the VtArray lives inside a retained data source
/// for as long as the renderer holds the prim.
template <typename Vec>
VtArray<Vec>
_CopyBuffer(const TuileBuffer &buffer, size_t count)
{
    VtArray<Vec> out(count);
    if (count > 0 && buffer.data && buffer.len == count * sizeof(Vec)) {
        std::memcpy(out.data(), buffer.data, buffer.len);
    }
    return out;
}

/// The standard preview-material triple: PrimvarReader(st) -> UsdUVTexture ->
/// UsdPreviewSurface, under the universal render context. Every Hydra renderer
/// that can texture at all resolves this network.
HdDataSourceBaseHandle
_MaterialNetwork(const std::string &textureUri)
{
    HdContainerDataSourceHandle stReader = HdMaterialNodeSchema::Builder()
        .SetNodeIdentifier(HdRetainedTypedSampledDataSource<TfToken>::New(
            _tokens->UsdPrimvarReader_float2))
        .SetParameters(HdRetainedContainerDataSource::New(
            _tokens->varname,
            HdMaterialNodeParameterSchema::Builder()
                .SetValue(HdRetainedTypedSampledDataSource<TfToken>::New(
                    _tokens->st))
                .Build()))
        .Build();

    HdContainerDataSourceHandle texture = HdMaterialNodeSchema::Builder()
        .SetNodeIdentifier(HdRetainedTypedSampledDataSource<TfToken>::New(
            _tokens->UsdUVTexture))
        .SetParameters(HdRetainedContainerDataSource::New(
            _tokens->file,
            HdMaterialNodeParameterSchema::Builder()
                .SetValue(HdRetainedTypedSampledDataSource<SdfAssetPath>::New(
                    SdfAssetPath(textureUri, textureUri)))
                .Build(),
            _tokens->wrapS,
            HdMaterialNodeParameterSchema::Builder()
                .SetValue(HdRetainedTypedSampledDataSource<TfToken>::New(
                    _tokens->clampWrap))
                .Build(),
            _tokens->wrapT,
            HdMaterialNodeParameterSchema::Builder()
                .SetValue(HdRetainedTypedSampledDataSource<TfToken>::New(
                    _tokens->clampWrap))
                .Build()))
        .SetInputConnections([] {
            const HdDataSourceBaseHandle connection =
                HdMaterialConnectionSchema::Builder()
                    .SetUpstreamNodePath(
                        HdRetainedTypedSampledDataSource<TfToken>::New(
                            _tokens->StReader))
                    .SetUpstreamNodeOutputName(
                        HdRetainedTypedSampledDataSource<TfToken>::New(
                            _tokens->result))
                    .Build();
            return HdRetainedContainerDataSource::New(
                _tokens->st,
                HdRetainedSmallVectorDataSource::New(1, &connection));
        }())
        .Build();

    HdContainerDataSourceHandle previewSurface = HdMaterialNodeSchema::Builder()
        .SetNodeIdentifier(HdRetainedTypedSampledDataSource<TfToken>::New(
            _tokens->UsdPreviewSurface))
        .SetParameters(HdRetainedContainerDataSource::New(
            _tokens->roughness,
            HdMaterialNodeParameterSchema::Builder()
                .SetValue(HdRetainedTypedSampledDataSource<float>::New(1.0f))
                .Build()))
        .SetInputConnections([] {
            const HdDataSourceBaseHandle connection =
                HdMaterialConnectionSchema::Builder()
                    .SetUpstreamNodePath(
                        HdRetainedTypedSampledDataSource<TfToken>::New(
                            _tokens->Texture))
                    .SetUpstreamNodeOutputName(
                        HdRetainedTypedSampledDataSource<TfToken>::New(
                            _tokens->rgb))
                    .Build();
            return HdRetainedContainerDataSource::New(
                _tokens->diffuseColor,
                HdRetainedSmallVectorDataSource::New(1, &connection));
        }())
        .Build();

    HdContainerDataSourceHandle network = HdMaterialNetworkSchema::Builder()
        .SetNodes(HdRetainedContainerDataSource::New(
            _tokens->StReader, stReader,
            _tokens->Texture, texture,
            _tokens->PreviewSurface, previewSurface))
        .SetTerminals(HdRetainedContainerDataSource::New(
            _tokens->surface,
            HdMaterialConnectionSchema::Builder()
                .SetUpstreamNodePath(
                    HdRetainedTypedSampledDataSource<TfToken>::New(
                        _tokens->PreviewSurface))
                .SetUpstreamNodeOutputName(
                    HdRetainedTypedSampledDataSource<TfToken>::New(
                        _tokens->surface))
                .Build()))
        .Build();

    return HdRetainedContainerDataSource::New(
        HdMaterialSchemaTokens->universalRenderContext, network);
}

}  // namespace

TuileGlobeProcedural::TuileGlobeProcedural(const SdfPath &proceduralPrimPath)
    : HdGpGenerativeProcedural(proceduralPrimPath)
    , _primPath(proceduralPrimPath)
{
}

TuileGlobeProcedural::~TuileGlobeProcedural()
{
    if (_frame) {
        tuile_frame_free(_frame);
    }
    if (_session) {
        tuile_session_free(_session);
    }
}

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

    // 4. None. The caller falls back to a fixed view above the render origin,
    //    which is the view-independent mode rather than a failure.
    return SdfPath();
}

bool
TuileGlobeProcedural::_EnsureSession(const HdSceneIndexBaseRefPtr &inputScene)
{
    if (_session) {
        return true;
    }
    if (_sessionFailed) {
        return false;
    }

    const std::string token = TfGetenv("TUILE_ION_TOKEN");
    if (token.empty()) {
        _sessionFailed = true;
        TF_RUNTIME_ERROR(
            "tuile: TUILE_ION_TOKEN is not set — the globe cannot stream. "
            "The token deliberately never lives in a stage.");
        return false;
    }
    const std::string cacheDir = TfGetenv("TUILE_CACHE_DIR");

    TuileGlobeConfig config = {};
    config.ion_token = {reinterpret_cast<const uint8_t *>(token.data()),
                        token.size()};
    config.cache_dir = {reinterpret_cast<const uint8_t *>(cacheDir.data()),
                        cacheDir.size()};
    config.terrain_asset_id = static_cast<int64_t>(
        _DoubleArg(inputScene, _primPath, _tokens->terrainAssetId, 0.0));
    config.imagery_asset_id = static_cast<int64_t>(
        _DoubleArg(inputScene, _primPath, _tokens->imageryAssetId, 0.0));
    config.maximum_screen_space_error =
        _DoubleArg(inputScene, _primPath, _tokens->maxSse, 0.0);
    // A farm frame under the uniform-detail decree legitimately fetches
    // thousands of tiles; the session's 120 s default is an interactive
    // reflex. Thirty minutes still catches a genuine hang loudly, and
    // TUILE_FRAME_TIMEOUT (seconds) overrides it per job.
    config.frame_timeout_seconds =
        TfGetenvDouble("TUILE_FRAME_TIMEOUT", 1800.0);
    config.fail_on_tile_errors = true;  // eager-fatal, docs/15

    const TuileStatus status = tuile_session_new(&config, &_session);
    if (status != TuileStatus_Ok || !_session) {
        _sessionFailed = true;
        TF_RUNTIME_ERROR(
            "tuile: opening the globe failed (status %d) — check the token, "
            "the asset ids and the network. Nothing will be emitted.",
            static_cast<int>(status));
        return false;
    }
    return true;
}

bool
TuileGlobeProcedural::_ViewForCook(const HdSceneIndexBaseRefPtr &inputScene,
                                   TuileViewState *out) const
{
    const double origin[3] = {_renderOrigin[0], _renderOrigin[1],
                              _renderOrigin[2]};
    if (!_cameraPath.IsEmpty() &&
        TuileViewFromCamera::Read(inputScene, _cameraPath, origin,
                                  _fallbackViewportPx, out))
    {
        return true;
    }

    // View-independent fallback: straight down from the render origin, which a
    // manifest places on the trajectory — above the ground it flies over. With
    // no origin either there is nothing sane to look at, and saying so beats
    // selecting tiles at the centre of the Earth.
    const double length = _renderOrigin.GetLength();
    if (length < 1.0) {
        return false;
    }
    const GfVec3d down = -_renderOrigin / length;
    // Any horizontal completes the basis; east of the origin's meridian.
    GfVec3d east(-_renderOrigin[1], _renderOrigin[0], 0.0);
    const double eastLength = east.GetLength();
    east = eastLength > 1e-9 ? east / eastLength : GfVec3d(1.0, 0.0, 0.0);
    for (int i = 0; i < 3; ++i) {
        out->position[i] = _renderOrigin[i];
        out->direction[i] = down[i];
        out->up[i] = east[i];
    }
    out->viewport_px[0] = _fallbackViewportPx[0];
    out->viewport_px[1] = _fallbackViewportPx[1];
    out->fovy_rad = 45.0 * M_PI / 180.0;
    return true;
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
    (void)previousResult;
    (void)dirtiedDependencies;
    (void)outputDirtiedPrims;

    ChildPrimTypeMap result;

    // The previous cook's frame dies here — GetChildPrim copied everything it
    // served into retained data sources, and the texture bytes were copied
    // into the resolver's store, so nothing borrows from it any more.
    if (_frame) {
        tuile_frame_free(_frame);
        _frame = nullptr;
    }
    _tilesByPath.clear();
    _materialsByPath.clear();

    if (!_EnsureSession(inputScene)) {
        return result;
    }

    _renderOrigin =
        _Vec3dArg(inputScene, _primPath, _tokens->renderOrigin, GfVec3d(0.0));
    const GfVec2d viewport = _Vec2dArg(
        inputScene, _primPath, _tokens->viewportPx,
        GfVec2d(_fallbackViewportPx[0], _fallbackViewportPx[1]));
    _fallbackViewportPx[0] = viewport[0];
    _fallbackViewportPx[1] = viewport[1];

    TuileViewState view = {};
    if (!_ViewForCook(inputScene, &view)) {
        TF_RUNTIME_ERROR(
            "tuile: no camera resolves and no render origin is authored — "
            "there is no view to select tiles for.");
        return result;
    }

    // Eager and fatal: this blocks until every selected tile is resident, or
    // reports why not. A frame that lost tiles must be a loud failure, never a
    // plausible image at the wrong level of detail.
    const TuileStatus status = tuile_session_frame(_session, &view, 1, &_frame);
    if (status != TuileStatus_Ok || !_frame) {
        TF_RUNTIME_ERROR(
            "tuile: the frame did not converge (status %d) — nothing is "
            "emitted rather than a partial globe.",
            static_cast<int>(status));
        return result;
    }

    size_t count = 0;
    tuile_frame_tile_count(_frame, &count);

    const SdfPath tileRoot = _primPath.AppendChild(_tokens->tileRoot);
    const SdfPath materialRoot = _primPath.AppendChild(_tokens->materialRoot);

    size_t textured = 0;
    for (size_t i = 0; i < count; ++i) {
        TuileTile tile = {};
        if (tuile_frame_tile(_frame, i, &tile) != TuileStatus_Ok) {
            continue;
        }

        _Tile entry;
        entry.index = i;
        entry.id = tile.tile_id;

        // Push this tile's textures into the resolver's store, copying: the
        // store owns its bytes, so nothing ties an in-flight texture load to
        // this frame's lifetime. The URI is content-stable across cooks, so a
        // tile kept from the previous frame is already served.
        for (size_t t = 0;; ++t) {
            TuileTexture texture = {};
            const TuileStatus ts = tuile_frame_texture(_frame, i, t, &texture);
            if (ts == TuileStatus_NotFound) {
                break;
            }
            if (ts != TuileStatus_Ok) {
                TF_RUNTIME_ERROR(
                    "tuile: encoding texture %zu of tile %llu failed "
                    "(status %d)",
                    t, static_cast<unsigned long long>(tile.tile_id),
                    static_cast<int>(ts));
                break;
            }
            const std::string uri(
                reinterpret_cast<const char *>(texture.uri.data),
                texture.uri.len);
            if (t == 0) {
                entry.textureUri = uri;
            }
            if (!TuileSpikeTiles::Has(uri)) {
                TuileSpikeTiles::Put(
                    uri,
                    std::vector<uint8_t>(texture.png.data,
                                         texture.png.data + texture.png.len));
            }
        }

        // The tile's PATH is its identity: a refinement change selects
        // different tile ids, so it removes prims and adds prims rather than
        // mutating one in place — the invariant hdEmbree taught us (a dirtied
        // mesh whose vertex count changed rendered an empty frame).
        const TfToken tileName(TfStringPrintf(
            "t%llu", static_cast<unsigned long long>(tile.tile_id)));
        const SdfPath tilePath = tileRoot.AppendChild(tileName);
        result[tilePath] = HdPrimTypeTokens->mesh;

        if (tile.base_color_texture >= 0 && !entry.textureUri.empty()) {
            const SdfPath materialPath = materialRoot.AppendChild(TfToken(
                TfStringPrintf("m%llu",
                               static_cast<unsigned long long>(tile.tile_id))));
            result[materialPath] = HdPrimTypeTokens->material;
            _materialsByPath[materialPath] = entry;
            ++textured;
        }
        _tilesByPath[tilePath] = entry;
    }

    TF_DEBUG(TUILE_HYDRA_PROCEDURAL).Msg(
        "[tuile] Update: camera=%s tiles=%zu textured=%zu origin=(%g, %g, %g)\n",
        _cameraPath.GetText(), count, textured, _renderOrigin[0],
        _renderOrigin[1], _renderOrigin[2]);

    return result;
}

HdSceneIndexPrim
TuileGlobeProcedural::GetChildPrim(
    const HdSceneIndexBaseRefPtr &inputScene,
    const SdfPath &childPrimPath)
{
    (void)inputScene;
    HdSceneIndexPrim prim;

    if (const auto materialIt = _materialsByPath.find(childPrimPath);
        materialIt != _materialsByPath.end())
    {
        prim.primType = HdPrimTypeTokens->material;
        prim.dataSource = HdRetainedContainerDataSource::New(
            HdMaterialSchemaTokens->material,
            _MaterialNetwork(materialIt->second.textureUri));
        return prim;
    }

    const auto it = _tilesByPath.find(childPrimPath);
    if (it == _tilesByPath.end() || !_frame) {
        return prim;
    }

    TuileTile tile = {};
    if (tuile_frame_tile(_frame, it->second.index, &tile) != TuileStatus_Ok) {
        return prim;
    }

    const size_t vertexCount = tile.vertex_count;
    const size_t indexCount = tile.index_count;

    VtVec3fArray points = _CopyBuffer<GfVec3f>(tile.positions, vertexCount);
    VtIntArray indices(indexCount);
    if (indexCount > 0 && tile.indices.data &&
        tile.indices.len == indexCount * sizeof(uint32_t))
    {
        std::memcpy(indices.data(), tile.indices.data, tile.indices.len);
    }
    // glTF-shaped content: triangle lists.
    VtIntArray faceVertexCounts(indexCount / 3, 3);

    // Placement, and the whole precision protocol in two lines: positions are
    // f32 relative to the tile's ECEF origin, the child transform carries
    // `origin − renderOrigin` computed in double. Never narrowed on the way.
    GfMatrix4d xf(1.0);
    xf.SetTranslate(GfVec3d(tile.origin_ecef[0] - _renderOrigin[0],
                            tile.origin_ecef[1] - _renderOrigin[1],
                            tile.origin_ecef[2] - _renderOrigin[2]));

    const bool textured = tile.base_color_texture >= 0 &&
                          !it->second.textureUri.empty() &&
                          tile.uvs.len == vertexCount * sizeof(float) * 2;

    std::vector<TfToken> primvarNames;
    std::vector<HdDataSourceBaseHandle> primvarSources;
    primvarNames.push_back(HdPrimvarsSchemaTokens->points);
    primvarSources.push_back(
        HdPrimvarSchema::Builder()
            .SetPrimvarValue(
                HdRetainedTypedSampledDataSource<VtVec3fArray>::New(points))
            .SetInterpolation(HdPrimvarSchema::BuildInterpolationDataSource(
                HdPrimvarSchemaTokens->vertex))
            .SetRole(HdPrimvarSchema::BuildRoleDataSource(
                HdPrimvarSchemaTokens->point))
            .Build());

    if (tile.normals.len == vertexCount * sizeof(float) * 3) {
        // Authored normals give the smooth surface; without them Storm
        // flat-shades every triangle into facets. The price — vertical skirt
        // faces going black under a lone camera light (measured on the first
        // gate render) — is paid by lighting, not geometry: the manifest
        // authors a dome light, so walls are lit from everywhere.
        primvarNames.push_back(HdTokens->normals);
        primvarSources.push_back(
            HdPrimvarSchema::Builder()
                .SetPrimvarValue(
                    HdRetainedTypedSampledDataSource<VtVec3fArray>::New(
                        _CopyBuffer<GfVec3f>(tile.normals, vertexCount)))
                .SetInterpolation(HdPrimvarSchema::BuildInterpolationDataSource(
                    HdPrimvarSchemaTokens->vertex))
                .SetRole(HdPrimvarSchema::BuildRoleDataSource(
                    HdPrimvarSchemaTokens->normal))
                .Build());
    }

    if (textured) {
        primvarNames.push_back(_tokens->st);
        primvarSources.push_back(
            HdPrimvarSchema::Builder()
                .SetPrimvarValue([&] {
                    // The baked mosaic's rows follow the tile's v — which
                    // grows SOUTHWARD (the drape convention) — while a
                    // sampled texture's t grows from the image's bottom row
                    // up. Without this flip every tile wears its
                    // south-north-mirrored imagery: same biome, so it looks
                    // plausible, and the roads stop dead at every tile
                    // border (found by Laurent following a road).
                    VtVec2fArray st =
                        _CopyBuffer<GfVec2f>(tile.uvs, vertexCount);
                    for (GfVec2f &uv : st) {
                        uv[1] = 1.0f - uv[1];
                    }
                    return HdRetainedTypedSampledDataSource<
                        VtVec2fArray>::New(st);
                }())
                .SetInterpolation(HdPrimvarSchema::BuildInterpolationDataSource(
                    HdPrimvarSchemaTokens->vertex))
                .SetRole(HdPrimvarSchema::BuildRoleDataSource(
                    HdPrimvarSchemaTokens->textureCoordinate))
                .Build());
    } else {
        // Untextured (terrain-only, or a tile with no uvs): the factor is the
        // whole material. A constant displayColor needs no network.
        primvarNames.push_back(_tokens->displayColor);
        primvarSources.push_back(
            HdPrimvarSchema::Builder()
                .SetPrimvarValue(
                    HdRetainedTypedSampledDataSource<VtVec3fArray>::New(
                        VtVec3fArray{GfVec3f(tile.base_color_factor[0],
                                             tile.base_color_factor[1],
                                             tile.base_color_factor[2])}))
                .SetInterpolation(HdPrimvarSchema::BuildInterpolationDataSource(
                    HdPrimvarSchemaTokens->constant))
                .SetRole(HdPrimvarSchema::BuildRoleDataSource(
                    HdPrimvarSchemaTokens->color))
                .Build());
    }

    std::vector<TfToken> names;
    std::vector<HdDataSourceBaseHandle> sources;
    names.push_back(HdMeshSchemaTokens->mesh);
    sources.push_back(
        HdMeshSchema::Builder()
            .SetTopology(
                HdMeshTopologySchema::Builder()
                    .SetFaceVertexCounts(
                        HdRetainedTypedSampledDataSource<VtIntArray>::New(
                            faceVertexCounts))
                    .SetFaceVertexIndices(
                        HdRetainedTypedSampledDataSource<VtIntArray>::New(
                            indices))
                    .SetOrientation(HdRetainedTypedSampledDataSource<TfToken>::New(
                        HdTokens->rightHanded))
                    .Build())
            // Terrain is polygons, not a subdivision cage.
            .SetSubdivisionScheme(HdRetainedTypedSampledDataSource<TfToken>::New(
                PxOsdOpenSubdivTokens->none))
            .SetDoubleSided(HdRetainedTypedSampledDataSource<bool>::New(true))
            .Build());
    names.push_back(HdPrimvarsSchemaTokens->primvars);
    sources.push_back(HdRetainedContainerDataSource::New(
        primvarNames.size(), primvarNames.data(), primvarSources.data()));
    names.push_back(HdXformSchemaTokens->xform);
    sources.push_back(HdXformSchema::Builder()
                          .SetMatrix(HdRetainedTypedSampledDataSource<
                                     GfMatrix4d>::New(xf))
                          .SetResetXformStack(
                              HdRetainedTypedSampledDataSource<bool>::New(false))
                          .Build());
    if (textured) {
        const SdfPath materialPath =
            _primPath.AppendChild(_tokens->materialRoot)
                .AppendChild(TfToken(TfStringPrintf(
                    "m%llu", static_cast<unsigned long long>(tile.tile_id))));
        names.push_back(HdMaterialBindingsSchemaTokens->materialBindings);
        sources.push_back(HdRetainedContainerDataSource::New(
            HdMaterialBindingsSchemaTokens->allPurpose,
            HdMaterialBindingSchema::Builder()
                .SetPath(HdRetainedTypedSampledDataSource<SdfPath>::New(
                    materialPath))
                .Build()));
    }

    prim.primType = HdPrimTypeTokens->mesh;
    prim.dataSource =
        HdRetainedContainerDataSource::New(names.size(), names.data(),
                                           sources.data());
    return prim;
}

PXR_NAMESPACE_CLOSE_SCOPE
