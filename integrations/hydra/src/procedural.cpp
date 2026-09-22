// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "procedural.h"

#include "hostpaths.h"
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
#include <pxr/usd/ar/asset.h>
#include <pxr/usd/ar/resolvedPath.h>
#include <pxr/usd/ar/resolver.h>
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
#include <limits>
#include <memory>
#include <mutex>

PXR_NAMESPACE_OPEN_SCOPE

// The debug code itself is declared in the header (the plugin counts
// constructions with it). Kept in the shipped plugin rather than removed after
// the fact: the failure it was written to diagnose — a procedural that cooks
// once and then silently stops — produces a plausible image and no error at
// all, so the next person to hit it needs a way in.
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
    (extension)
    (UsdPreviewSurface)
    (UsdUVTexture)
    ((cyclesImageTexture, "cycles_image_texture"))
    (filename)
    (vector)
    (color)
    (UsdPrimvarReader_float2)
    (PreviewSurface)
    ((Texture, "TuileTexture"))
    (StReader)
);

/// A session, shared by every procedural that reads the same globe.
///
/// A session is not a cheap thing to own: it holds the resident tile set, the
/// baked-texture memo, a tokio runtime, a server thread and a warm HTTP pool.
/// It is also not a cheap thing to *lose* — the server sends a tile's content
/// once per residency, so a fresh session starts from nothing every time.
///
/// hdGp would keep one procedural instance alive for a whole render, and with
/// it the session; Blender does not, because it rebuilds its scene index on
/// every frame (`Engine::sync`, the export path Blender itself calls "slow").
/// So the session outlives the instance instead — process-lifetime, keyed by
/// what it actually serves.
struct _SharedSession
{
    TuileSession *session = nullptr;
    /// Set once if opening failed, so a broken configuration is reported
    /// once rather than retried on every cook of every frame.
    bool failed = false;
    /// `tuile_session_frame` takes the session mutably. One prim cooks at a
    /// time today; this makes that a fact rather than an assumption.
    std::mutex frameLock;
};

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
    // A path carried as text, which is how a stage can carry one at all.
    //
    // USD has no `SdfPath`-valued attribute, and hdGp hands its arguments over
    // as primvars — which are attributes. A manifest that wanted to name the
    // camera therefore had only one honest option, `string`, and this function
    // did not accept it: it authored `rel primvars:tuile:cameras` instead, a
    // relationship wearing a primvar's prefix, which never reached here.
    //
    // The cost was silent and total. `_ResolveCamera` fell through all four
    // rungs to the fixed view above the render origin — the centre of the
    // orbit — and every frame of a 1440-frame pack was then equidistant from
    // it: "the nearest is frame 1108, 8000.000 m away", 8000 m being the
    // orbit's radius to the millimetre.
    if (v.IsHolding<std::string>()) {
        const std::string &text = v.UncheckedGet<std::string>();
        if (!text.empty() && SdfPath::IsValidPathString(text)) {
            return SdfPath(text);
        }
    }
    if (v.IsHolding<TfToken>()) {
        const std::string text = v.UncheckedGet<TfToken>().GetString();
        if (!text.empty() && SdfPath::IsValidPathString(text)) {
            return SdfPath(text);
        }
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
_RenderSettingsCamera(const HdSceneIndexBaseRefPtr &scene, const SdfPath &anchor)
{
    HdSceneGlobalsSchema globals = TuileSceneGlobals(scene, anchor);
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
    // The settings-level camera only exists from 26.08; on 26.03 (the
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

    // Le nœud natif Cycles, et pourquoi pas `UsdUVTexture`.
    //
    // `UsdUVTexture` est la forme portable, et c'est celle qui était écrite
    // ici. Le hdCycles livré avec ce Blender la refuse : « Could not create
    // node 'Texture' », suivi de « Ignoring connection from 'Texture.rgb' to
    // 'PreviewSurface.diffuseColor' ». Le terrain sortait alors en aplat rose
    // — la couleur d'un matériau dont la texture n'a pas été branchée — avec
    // un relief parfaitement correct par ailleurs.
    //
    // Ce n'est pas nous : relu sur le schéma lui-même au moment de l'écrire,
    // l'identifiant était bien `UsdUVTexture` et le chemin pointait un PNG
    // existant. Les deux autres nœuds du réseau, bâtis par le même Builder,
    // passent sans un mot.
    //
    // `PopulateShaderGraph` traite à part tout identifiant préfixé `cycles_` :
    // il court-circuite la table de correspondance USD et va directement à
    // `NodeType::find`. On emprunte cette porte-là. Le coût est la portabilité
    // — un délégué qui n'est pas Cycles ne texturera pas — et il est assumé
    // tant que la cible est Cycles ; le jour où Storm compte, le choix devra
    // dépendre du délégué et non être figé ici.
    //
    // Les noms changent avec le nœud : `filename` au lieu de `file`, l'entrée
    // `vector` au lieu de `st`, la sortie `color` au lieu de `rgb`. Et
    // `extension` au lieu de wrapS/wrapT — un seul socket pour les deux axes.
    HdContainerDataSourceHandle texture = HdMaterialNodeSchema::Builder()
        .SetNodeIdentifier(HdRetainedTypedSampledDataSource<TfToken>::New(
            _tokens->cyclesImageTexture))
        // Pas de `colorspace` : `auto` est mesuré juste.
        //
        // Le socket vaut `u_colorspace_auto` par défaut — une ustring vide
        // (`intern/cycles/util/colorspace.cpp`) — qui laisse OIIO déduire
        // l'espace du fichier. On a soupçonné cette déduction d'être fausse,
        // et on l'a testée le 17 septembre 2026 en écrivant
        // `colorspace = "__builtin_srgb"` : l'image est passée de 22 % de
        // pixels écrêtés à 99,84 %, un blanc uniforme. Un double décodage
        // ASSOMBRIT ; celui-ci blanchit, donc la valeur n'est pas reçue comme
        // on le croyait. `auto` reste, et la sur-exposition qu'on chassait
        // venait de l'éclairage.
        .SetParameters(HdRetainedContainerDataSource::New(
            // `clamp`, et surtout pas le défaut.
            //
            // Le défaut de Cycles est `EXTENSION_REPEAT`
            // (`intern/cycles/scene/shader_nodes.cpp:275`), avec un filtre
            // bilinéaire. Nos UV vont exactement de 0 à 1
            // (`tuile-terrain/src/mesh.rs:98-104`) et le drapage n'a AUCUNE
            // gouttière — `tuile-core/src/raster.rs:919-934` documente ce choix
            // et note qu'aucune des deux implémentations de référence n'en a.
            // Échantillonner en `u=0` demande donc le texel −0,5, que REPEAT
            // fait boucler sur la colonne opposée de la même tuile : une bande
            // d'un demi-texel de la mauvaise couleur, sur chaque bord, en
            // grille.
            //
            // Les jupes transforment ce sous-pixel en ligne franche : leur UV
            // entier est épinglé au bord (`mesh.rs:224`, délibérément, pour
            // qu'un mur visible porte la texture de son bord), donc le mur
            // censé masquer les fissures se peint de la couleur du bord d'en
            // face.
            //
            // `UsdUVTexture` portait `wrapS`/`wrapT = "clamp"`, que hdCycles
            // remappait vers ce socket (`hydra/material.cpp:129-152`). Ce
            // remappage ne s'applique pas à un identifiant préfixé `cycles_`
            // (`material.cpp:440-462` puis `findCycles()` qui rend `nullptr`),
            // donc le socket s'écrit ici, directement, avec le vocabulaire de
            // Cycles — où « répéter » se dit `periodic` et « borner »,
            // `clamp`.
            _tokens->extension,
            HdMaterialNodeParameterSchema::Builder()
                .SetValue(HdRetainedTypedSampledDataSource<TfToken>::New(
                    _tokens->clampWrap))
                .Build(),
            _tokens->filename,
            HdMaterialNodeParameterSchema::Builder()
                // Une chaîne, pas un `SdfAssetPath`.
                //
                // Le socket Cycles est `SOCKET_STRING(filename, ...)`, et la
                // conversion depuis un asset path n'a pas lieu : le rendu
                // s'est plaint de « Image file  does not exist » — deux
                // espaces, parce que le nom était vide — pendant que le
                // fichier, lui, existait bel et bien sur le disque (vérifié
                // par `TfIsFile` au moment même de l'écrire).
                .SetValue(HdRetainedTypedSampledDataSource<std::string>::New(
                    textureUri))
                .Build()))
        // Pas de connexion entrante, et pas de lecteur de primvar.
        //
        // Le socket Cycles est déclaré
        // `SOCKET_IN_POINT(vector, "Vector", zero_float3(), LINK_TEXTURE_UV)`,
        // et ce drapeau dit à Cycles de brancher lui-même les UV par défaut
        // quand rien n'est connecté. Le `UsdPrimvarReader_float2` ne servait
        // donc qu'à répéter ce que le moteur fait seul.
        //
        // Il servait surtout à autre chose sans le vouloir : il faisait de ce
        // nœud le SEUL du réseau à être à la fois cible et source d'une
        // connexion — `StReader → Texture → PreviewSurface` — et c'est
        // exactement le seul nœud que hdCycles refusait de créer, quel que
        // soit son identifiant (`UsdUVTexture` comme `cycles_image_texture`,
        // les deux essayés, le même « Could not create node »). Les deux
        // autres, chacun d'un seul côté d'une connexion, passaient sans un
        // mot.
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
                            _tokens->color))
                    .Build();
            return HdRetainedContainerDataSource::New(
                _tokens->diffuseColor,
                HdRetainedSmallVectorDataSource::New(1, &connection));
        }())
        .Build();

    HdContainerDataSourceHandle nodes = HdRetainedContainerDataSource::New(
        _tokens->StReader, stReader,
        _tokens->Texture, texture,
        _tokens->PreviewSurface, previewSurface);

    // Le réseau tel qu'il part, relu une fois comme hdCycles le lira.
    //
    // hdCycles refuse le nœud de texture — « Could not create node » — et son
    // code ne peut le faire que si `NodeType::find` échoue, c'est-à-dire si
    // l'identifiant qu'il lit est vide ou inconnu. Or la table de ce fork
    // contient bien `UsdUVTexture`, et son `PopulateShaderGraph` accepte le
    // préfixe `cycles_` : les deux formes ont été essayées, les deux ont
    // échoué, pendant que les deux autres nœuds du même réseau passaient.
    //
    // Alors on regarde les trois ensemble. Si les trois portent leur
    // identifiant ici, la perte est en aval de nous.
    static bool announced = false;
    if (!announced) {
        announced = true;
        for (const TfToken &name : {_tokens->StReader, _tokens->Texture,
                                    _tokens->PreviewSurface}) {
            HdMaterialNodeSchema node(
                HdContainerDataSource::Cast(nodes->Get(name)));
            const TfToken identifier =
                node.GetNodeIdentifier()
                    ? node.GetNodeIdentifier()->GetTypedValue(0.0f)
                    : TfToken();
            printf("MATERIAL-NODE %-14s identifier='%s'\n",
                   name.GetText(), identifier.GetText());
        }
        // Et l'URI, demandee au resolveur et non au systeme de fichiers.
        //
        // `TfIsFile` etait ici, et il mentait a tous les coups : c'est un
        // `stat`, il prend un chemin, et `tuile://pack-.../texture/0.....png`
        // n'en est pas un. Il repondait donc « MISSING » quoi que le resolveur
        // sache faire — et c'est le greffon `tuile://` qui va chercher la
        // texture au rendu, pas lui. Un diagnostic qui crie au loup a chaque
        // rendu reussi coute plus cher que pas de diagnostic : on a cherche
        // une texture absente deux fois avant de lire cette ligne.
        //
        // Resoudre PUIS ouvrir, parce que les deux echouent differemment :
        // une URI que le resolveur ne reconnait pas n'a pas de chemin resolu,
        // tandis qu'une URI bien formee dont l'octet ne vient pas se resout
        // et ne s'ouvre pas. La seconde est la panne qu'on veut voir.
        ArResolver &resolver = ArGetResolver();
        const ArResolvedPath resolved = resolver.Resolve(textureUri);
        const char *verdict = "UNRESOLVED";
        if (resolved) {
            verdict = resolver.OpenAsset(resolved) ? "READABLE"
                                                   : "RESOLVED-BUT-UNREADABLE";
        }
        printf("MATERIAL-URI %s (%s)\n", textureUri.c_str(), verdict);
        fflush(stdout);
    }

    HdContainerDataSourceHandle network = HdMaterialNetworkSchema::Builder()
        .SetNodes(nodes)
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

struct _SessionRegistry
{
    std::mutex mutex;
    std::map<std::string, std::unique_ptr<_SharedSession>> sessions;
};

/// Function-local static: constructed on first use, never destroyed — the
/// same shape as the tile store next door, and for the same reason (a plugin
/// has no controlled shutdown).
_SessionRegistry &
_TheSessions()
{
    static _SessionRegistry registry;
    return registry;
}

/// The ABI's status code, spelled out — a bare "status 4" in a farm log costs
/// a trip to the header every time.
const char *
_StatusName(TuileStatus status)
{
    switch (status) {
    case TuileStatus_Ok: return "ok";
    case TuileStatus_BadArgument: return "bad argument";
    case TuileStatus_TimedOut: return "timed out";
    case TuileStatus_ServerGone: return "the streaming session ended";
    case TuileStatus_TilesFailed: return "a tile could not be loaded";
    case TuileStatus_InternalError: return "internal error";
    case TuileStatus_EncodeFailed: return "texture encoding failed";
    case TuileStatus_NotFound: return "not found";
    }
    return "unknown status";
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
    // The session is deliberately NOT freed: it belongs to the process
    // registry and outlives this instance, which Blender destroys per frame.
}

SdfPath
TuileGlobeProcedural::_ResolveCamera(const HdSceneIndexBaseRefPtr &inputScene) const
{
    // 1. The camera the host renders THROUGH, then the one authored on this
    //    prim. `TuileSelectCamera` holds the order between the two; see
    //    `cameraRungTest.cpp` for what that order is and why.
    //
    // The authored path is the only choice a pipeline can pin, but it reaches
    // us through the host's namespace, not the stage's — it is the rung that
    // carries its path as text, so it is the one prefixing cannot rewrite (see
    // `hostpaths.h`).
    //
    // The render camera comes first because it is the one the image is made
    // by. Blender re-exports its camera into the stage every frame as
    // `/usd_scene/.../shot`, but renders through `/freeCamera`, which it moves
    // without rebuilding anything. Selecting tiles for the exported copy means
    // selecting for a camera that is only usually the right one — and it is
    // the re-export we are about to stop doing.
    const SdfPath authoredCamera = _PathArg(inputScene, _primPath, _tokens->cameras);
    if (const SdfPath chosen = TuileSelectCamera(inputScene, _primPath, authoredCamera);
        !chosen.IsEmpty()) {
        if (!authoredCamera.IsEmpty() && chosen != authoredCamera) {
            printf("CAMERA-REROOTED authored=%s host=%s\n",
                   authoredCamera.GetText(), chosen.GetText());
            fflush(stdout);
        }
        return chosen;
    }
    if (!authoredCamera.IsEmpty()) {
        // Authored and absent. Saying so beats falling through in silence to a
        // rung that answers a different question.
        printf("CAMERA-AUTHORED-MISSING %s — no prim there, nor under any "
               "ancestor of %s\n",
               authoredCamera.GetText(), _primPath.GetText());
        fflush(stdout);
    }

    // 2. The camera this render product renders through.
    if (SdfPath fromSettings = _RenderSettingsCamera(inputScene, _primPath);
        !fromSettings.IsEmpty()) {
        return fromSettings;
    }

    // 3. Whatever the host calls primary — a viewport's free camera, in an
    //    interactive session, so useful but not reproducible.
    if (HdSceneGlobalsSchema globals = TuileSceneGlobals(inputScene, _primPath)) {
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
    if (_shared && _shared->session) {
        return true;
    }
    if (_shared && _shared->failed) {
        return false;
    }

    // A pre-baked pack, if the job was given one. It is the whole scene —
    // selection, geometry and imagery, decided once by `tuile-bake` — so with
    // one in hand this process never opens a socket and needs no token at all.
    //
    // From the environment rather than from the stage, and for the same reason
    // the token is: a manifest describes a shot, not the machine rendering it.
    // Two nodes may legitimately read the same stage with the pack in
    // different places, or one of them from the network.
    const std::string packPath = TfGetenv("TUILE_PACK");
    const std::string sceneDigest = TfGetenv("TUILE_SCENE");

    const std::string token = TfGetenv("TUILE_ION_TOKEN");
    if (packPath.empty() && token.empty()) {
        TF_RUNTIME_ERROR(
            "tuile: neither TUILE_PACK nor TUILE_ION_TOKEN is set — there is "
            "nothing to read the globe from. The token deliberately never "
            "lives in a stage, and neither does the pack's path.");
        return false;
    }
    const std::string cacheDir = TfGetenv("TUILE_CACHE_DIR");

    TuileGlobeConfig config = {};
    // The key is what the session SERVES, never who is allowed to read it:
    // the token selects permission, not data, and keeping it out means it can
    // never surface in a diagnostic that prints a key.
    const std::string key = TfStringPrintf(
        "%s|%lld|%lld|%g|%s",
        packPath.c_str(),
        static_cast<long long>(
            _DoubleArg(inputScene, _primPath, _tokens->terrainAssetId, 0.0)),
        static_cast<long long>(
            _DoubleArg(inputScene, _primPath, _tokens->imageryAssetId, 0.0)),
        _DoubleArg(inputScene, _primPath, _tokens->maxSse, 0.0),
        cacheDir.c_str());

    _SessionRegistry &registry = _TheSessions();
    std::lock_guard<std::mutex> registryLock(registry.mutex);
    std::unique_ptr<_SharedSession> &entry = registry.sessions[key];
    if (!entry) {
        entry = std::make_unique<_SharedSession>();
    }
    _shared = entry.get();
    if (_shared->session) {
        return true;
    }
    if (_shared->failed) {
        return false;
    }

    config.pack_path = {reinterpret_cast<const uint8_t *>(packPath.data()),
                        packPath.size()};
    config.scene_digest = {
        reinterpret_cast<const uint8_t *>(sceneDigest.data()),
        sceneDigest.size()};
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

    const TuileStatus status = tuile_session_new(&config, &_shared->session);
    if (status != TuileStatus_Ok || !_shared->session) {
        _shared->failed = true;
        if (!packPath.empty()) {
            TF_RUNTIME_ERROR(
                "tuile: opening the pack %s failed (status %d) — it is "
                "missing, truncated, or a bake of another scene. Nothing will "
                "be emitted.",
                packPath.c_str(), static_cast<int>(status));
        } else {
            TF_RUNTIME_ERROR(
                "tuile: opening the globe failed (status %d) — check the "
                "token, the asset ids and the network. Nothing will be "
                "emitted.",
                static_cast<int>(status));
        }
        return false;
    }
    // Which of the two this process is reading, said once and unmistakably.
    // A pod that was meant to read a pack and quietly fell back to the network
    // would still render — slower, and possibly a different picture — and the
    // only symptom would be the bill.
    const std::string source =
        packPath.empty() ? std::string("network") : "pack " + packPath;
    printf("SOURCE %s\n", source.c_str());
    fflush(stdout);
    return true;
}

bool
TuileGlobeProcedural::_ViewForCook(const HdSceneIndexBaseRefPtr &inputScene,
                                   TuileViewState *out) const
{
    const double origin[3] = {_renderOrigin[0], _renderOrigin[1],
                              _renderOrigin[2]};
    if (!_cameraPath.IsEmpty() &&
        TuileViewFromCamera::Read(inputScene, _cameraPath, _primPath, origin,
                                  _fallbackViewportPx, out))
    {
        return true;
    }

    // Said out loud, because the fallback below is indistinguishable from a
    // working render until something downstream refuses the view it produces.
    //
    // With a pack in hand it is never right: a baked frame answers a camera
    // that was on the trajectory, and this one is at its centre. The error a
    // reader then gets — "no baked frame answers this camera, the nearest is
    // 8000 m away" — describes the symptom and hides the cause, which is that
    // the camera never arrived.
    printf("CAMERA-FALLBACK path=%s read=%s — view-independent, straight down "
           "from the render origin\n",
           _cameraPath.IsEmpty() ? "(none)" : _cameraPath.GetText(),
           _cameraPath.IsEmpty() ? "n/a" : "failed");
    fflush(stdout);

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
    const SdfPath previous = _cameraPath;
    _cameraPath = _ResolveCamera(inputScene);
    if (_cameraPath != previous) {
        printf("CAMERA-RESOLVED %s\n",
               _cameraPath.IsEmpty() ? "(none — all four rungs empty)"
                                     : _cameraPath.GetText());
        fflush(stdout);
    }

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

    ChildPrimTypeMap result;

    if (!_EnsureSession(inputScene)) {
        return result;
    }

    _renderOrigin =
        _Vec3dArg(inputScene, _primPath, _tokens->renderOrigin, GfVec3d(0.0));
    // Every child's transform is `origin − renderOrigin`. If the manifest
    // moved the render origin, every prim built against the old one is wrong,
    // and none of them can be kept.
    if (_haveBuilt && _renderOrigin != _builtOrigin) {
        _tilesByPath.clear();
        _materialsByPath.clear();
    }
    _builtOrigin = _renderOrigin;
    _haveBuilt = true;
    const GfVec2d viewport = _Vec2dArg(
        inputScene, _primPath, _tokens->viewportPx,
        GfVec2d(_fallbackViewportPx[0], _fallbackViewportPx[1]));
    _fallbackViewportPx[0] = viewport[0];
    _fallbackViewportPx[1] = viewport[1];

    TuileViewState view = {};
    if (!_ViewForCook(inputScene, &view)) {
        // Fatal, not a warning with an empty globe behind it: see below.
        TF_FATAL_ERROR(
            "tuile: no camera resolves and no render origin is authored — "
            "there is no view to select tiles for.");
    }

    // Eager and fatal: this blocks until every selected tile is resident, or
    // ends the process.
    //
    // FATAL, and deliberately so. This used to post a runtime error and return
    // an empty map, which reads like caution and is the opposite: hdGp emits no
    // children, the renderer draws nothing, `usdrecord` writes a transparent
    // frame and exits 0, and ffmpeg encodes it as black. Nobody downstream can
    // tell that apart from a frame that legitimately had nothing in it — so a
    // farm job shipped a 48-frame video, every frame black, and reported
    // success. The Rust side has already retried the transport and has already
    // logged which tile and why; there is nothing left to recover, and the only
    // useful thing left to do is to stop where the fault is, with a non-zero
    // exit, so the segment fails instead of being delivered.
    TuileStatus status;
    {
        std::lock_guard<std::mutex> frameLock(_shared->frameLock);
        status = tuile_session_frame(_shared->session, &view, 1, &_frame);
    }
    if (status != TuileStatus_Ok || !_frame) {
        TF_FATAL_ERROR(
            "tuile: the frame did not converge (status %d, %s) — see the "
            "error logged above for the tile and the reason.",
            static_cast<int>(status), _StatusName(status));
    }

    size_t count = 0;
    tuile_frame_tile_count(_frame, &count);

    const SdfPath tileRoot = _primPath.AppendChild(_tokens->tileRoot);
    const SdfPath materialRoot = _primPath.AppendChild(_tokens->materialRoot);

    // What this cook actually had to do. `kept` is the number that matters:
    // hdGp emits nothing for a child re-declared unchanged, so those tiles
    // cost nothing at all — no notice, no GetChildPrim, no data source.
    size_t textured = 0;
    size_t kept = 0;
    size_t built = 0;
    size_t redraped = 0;

    for (size_t i = 0; i < count; ++i) {
        TuileTile tile = {};
        if (tuile_frame_tile(_frame, i, &tile) != TuileStatus_Ok) {
            continue;
        }

        // The tile's PATH is its identity: a refinement change selects
        // different tile ids, so it removes prims and adds prims rather than
        // mutating one in place — the invariant hdEmbree taught us (a dirtied
        // mesh whose vertex count changed rendered an empty frame).
        const SdfPath tilePath = tileRoot.AppendChild(TfToken(TfStringPrintf(
            "t%llu", static_cast<unsigned long long>(tile.tile_id))));
        const SdfPath materialPath = materialRoot.AppendChild(TfToken(
            TfStringPrintf("m%llu",
                           static_cast<unsigned long long>(tile.tile_id))));

        const auto known = _tilesByPath.find(tilePath);
        const bool unchanged =
            known != _tilesByPath.end() && known->second.drape == tile.drape;

        if (unchanged) {
            // Re-declared exactly as it was. The resolver compares this map
            // against the previous one and stays silent — which is the whole
            // point, since a PrimsAdded on a prim that already exists is a
            // resync and makes the renderer re-fetch everything.
            result[tilePath] = HdPrimTypeTokens->mesh;
            if (known->second.textured) {
                result[materialPath] = HdPrimTypeTokens->material;
                ++textured;
            }
            ++kept;
            continue;
        }

        // New, or the same ground re-draped at another imagery level. Either
        // way its pixels have to be fetched, and its prim rebuilt.
        std::string textureUri;
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
                textureUri = uri;
            }
            // La table ne sert que les URI `tuile://`, donc on ne remplit
            // que celles-là.
            //
            // Elle était remplie pour TOUTE URI. Sur le chemin pack, où le
            // `materialise` du lecteur écrit un vrai fichier et rend son
            // chemin, Ar résout par le système de fichiers et n'ouvre jamais
            // cette table : chaque texture y était donc recopiée pour
            // personne. Et `TuileSpikeTiles` n'a aucune éviction — seulement
            // `Clear` — si bien que cette copie inutile grandissait jusqu'à
            // contenir l'intégralité des textures du pack, dans la mémoire
            // d'un job qui en a 32 Gio et écrit déjà les mêmes octets dans un
            // `/tmp` qui est lui aussi de la RAM.
            //
            // Le préfixe est le bon test parce que c'est exactement celui que
            // le resolver déclare : un URI resolver ne voit que son schéma
            // (`resolver.h`), et tout le reste appartient au resolver de
            // fichiers.
            //
            // The URI carries the drape, so a re-drape is a different asset
            // and this never serves stale pixels under a name it already has.
            if (uri.rfind("tuile://", 0) == 0 && !TuileSpikeTiles::Has(uri)) {
                TuileSpikeTiles::Put(
                    uri,
                    std::vector<uint8_t>(texture.png.data,
                                         texture.png.data + texture.png.len));
            }
        }

        _Tile entry;
        entry.id = tile.tile_id;
        entry.drape = tile.drape;
        entry.originEcef = GfVec3d(tile.origin_ecef[0], tile.origin_ecef[1],
                                   tile.origin_ecef[2]);
        entry.textureUri = textureUri;
        entry.textured = tile.base_color_texture >= 0 && !textureUri.empty() &&
                         tile.uvs.len ==
                             static_cast<size_t>(tile.vertex_count) *
                                 sizeof(float) * 2;

        const bool wasHere = known != _tilesByPath.end();
        const bool wasTextured = wasHere && known->second.textured;
        // A re-drape changes the imagery, never the ground: same tile id, same
        // vertices, same transform. So the mesh is kept and only its material
        // is rebuilt — unless the tile gained or lost its texture, which
        // rewrites the mesh's own primvars and binding.
        if (wasHere && wasTextured == entry.textured) {
            entry.prim = known->second.prim;
            ++redraped;
        } else {
            entry.prim = _BuildTilePrim(tile, entry.textured, materialPath);
            if (wasHere) {
                ++redraped;
            } else {
                ++built;
            }
        }

        result[tilePath] = HdPrimTypeTokens->mesh;
        _tilesByPath[tilePath] = std::move(entry);

        if (_tilesByPath[tilePath].textured) {
            _Tile material;
            material.id = tile.tile_id;
            material.drape = tile.drape;
            material.textured = true;
            material.textureUri = textureUri;
            material.prim.primType = HdPrimTypeTokens->material;
            material.prim.dataSource = HdRetainedContainerDataSource::New(
                HdMaterialSchemaTokens->material, _MaterialNetwork(textureUri));
            _materialsByPath[materialPath] = std::move(material);
            result[materialPath] = HdPrimTypeTokens->material;
            ++textured;
        } else {
            _materialsByPath.erase(materialPath);
        }

        // A kept path with new data is the one case the resolver cannot see:
        // same path, same type, so it says nothing. Saying it here is the only
        // way the renderer learns the pixels moved.
        if (wasHere && outputDirtiedPrims) {
            if (_tilesByPath[tilePath].textured) {
                outputDirtiedPrims->emplace_back(
                    materialPath, HdMaterialSchema::GetDefaultLocator());
            }
            // Gaining or losing a texture rewrites the mesh itself — its
            // primvars (st against displayColor) and its binding — so the
            // whole prim is dirty, not just its material.
            if (wasTextured != _tilesByPath[tilePath].textured) {
                outputDirtiedPrims->emplace_back(
                    tilePath, HdDataSourceLocator::EmptyLocator());
            }
        }
    }

    // Everything the selection dropped. Absent from `result`, the resolver
    // emits PrimsRemoved for it; dropping the entry here is what stops the
    // maps growing for the length of a shot.
    size_t dropped = 0;
    for (auto it = _tilesByPath.begin(); it != _tilesByPath.end();) {
        if (result.find(it->first) == result.end()) {
            it = _tilesByPath.erase(it);
            ++dropped;
        } else {
            ++it;
        }
    }
    for (auto it = _materialsByPath.begin(); it != _materialsByPath.end();) {
        if (result.find(it->first) == result.end()) {
            it = _materialsByPath.erase(it);
        } else {
            ++it;
        }
    }

    // The frame's borrows are all copied by now — into the prims above and
    // into the resolver's texture store — so it dies here rather than at the
    // start of the next cook. That is what lets GetChildPrim be a map lookup
    // with nothing to outlive.
    tuile_frame_free(_frame);
    _frame = nullptr;

    // How far the selection actually reaches, in kilometres from the eye.
    //
    // The question this answers is "is that black band sky, or ground nobody
    // selected?" — and it is not answerable from a picture: a ridge occludes,
    // and missing ground looks exactly like a horizon. The top ray of a frame
    // meets the ground at a distance geometry can state exactly; if the
    // selection stops short of it, the black is ours.
    //
    // Reported next to the count of tiles PAST THE HORIZON, because the reach
    // on its own is a maximum and a maximum lies about volume: one coarse tile
    // on the far side of the planet reads the same as six hundred of them. It
    // was read that way once, and the wrong conclusion followed.
    double reach = 0.0;
    size_t beyondHorizon = 0;
    // <5, <10, <20, <50, <100, <500, >=500 km from the eye.
    size_t bands[7] = {0, 0, 0, 0, 0, 0, 0};
    {
        const GfVec3d eye(view.position[0], view.position[1], view.position[2]);
        // Distance from the eye to the horizon of a sphere inscribed in the
        // ellipsoid: sqrt(|eye|^2 - r^2). Ground further off than this is
        // behind the planet, whatever the frustum says about it.
        constexpr double kInscribedRadius = 6356752.0;
        const double eyeLengthSq = eye.GetLengthSq();
        const double horizon =
            eyeLengthSq > kInscribedRadius * kInscribedRadius
                ? std::sqrt(eyeLengthSq - kInscribedRadius * kInscribedRadius)
                : std::numeric_limits<double>::infinity();
        for (const auto &entry : _tilesByPath) {
            const GfVec3d origin = entry.second.originEcef;
            const double distance = (origin - eye).GetLength();
            reach = std::max(reach, distance);
            if (distance > horizon) {
                ++beyondHorizon;
            }
            // Which ground was selected, by distance from the eye.
            //
            // The one question a count of tiles cannot answer: when a band of
            // the frame is black, is that ground missing from the SELECTION,
            // or selected and not drawn? Everything else — reach, tile counts,
            // gaps — is blind to it, and three wrong causes were argued for
            // before anyone measured this.
            const double km = distance / 1000.0;
            size_t band = 0;
            for (const double edge : {5.0, 10.0, 20.0, 50.0, 100.0, 500.0}) {
                if (km < edge) {
                    break;
                }
                ++band;
            }
            ++bands[band];
        }
    }

    // Cooks on THIS instance. Read next to the plugin's construction count:
    // one construction and N cooks is hdGp working as designed; N of each is
    // the host rebuilding its scene index every frame.
    ++_cooks;
    TF_DEBUG(TUILE_HYDRA_PROCEDURAL).Msg(
        "[tuile] Update: cook #%llu on this instance, camera=%s tiles=%zu "
        "kept=%zu built=%zu redraped=%zu dropped=%zu textured=%zu "
        "reach=%.1fkm beyondHorizon=%zu "
        "km<5=%zu <10=%zu <20=%zu <50=%zu <100=%zu <500=%zu >=500=%zu\n",
        static_cast<unsigned long long>(_cooks), _cameraPath.GetText(), count,
        kept, built, redraped, dropped, textured, reach / 1000.0, beyondHorizon,
        bands[0], bands[1], bands[2], bands[3], bands[4], bands[5], bands[6]);

    return result;
}

/// Hands back a child built during a cook.
///
/// A map lookup and nothing else, on purpose. It is called from several
/// threads, and it used to read the frame that the *next* cook frees — which
/// only ever worked because the whole procedural was thrown away every frame.
/// Now that a prim can outlive the frame it came from, it has to.
HdSceneIndexPrim
TuileGlobeProcedural::GetChildPrim(
    const HdSceneIndexBaseRefPtr &inputScene,
    const SdfPath &childPrimPath)
{
    (void)inputScene;

    if (const auto it = _materialsByPath.find(childPrimPath);
        it != _materialsByPath.end())
    {
        return it->second.prim;
    }
    if (const auto it = _tilesByPath.find(childPrimPath);
        it != _tilesByPath.end())
    {
        return it->second.prim;
    }
    return HdSceneIndexPrim();
}

HdSceneIndexPrim
TuileGlobeProcedural::_BuildTilePrim(const TuileTile &tile,
                                     bool textured,
                                     const SdfPath &materialPath) const
{
    HdSceneIndexPrim prim;
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
