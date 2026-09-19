// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "manifest.h"

#include <pxr/base/tf/getenv.h>
#include <pxr/imaging/hd/mergingSceneIndex.h>
#include <pxr/imaging/hd/renderIndex.h>
#include <pxr/usd/usd/stage.h>
#include <pxr/usdImaging/usdImaging/sceneIndices.h>

#include <cstdio>
#include <mutex>
#include <string>
#include <unordered_map>

PXR_NAMESPACE_OPEN_SCOPE

namespace {

/// One scene index per manifest path, for the life of the process.
///
/// It is built at most once per file: a session creates more than one render
/// index, and re-opening the stage each time would re-read and re-compose the
/// same file for an answer that cannot differ — and would hand out prims with
/// different identities for the same paths.
///
/// **Alloué et jamais détruit, délibérément.** Une table `static` ordinaire
/// serait détruite à la fin du processus, et elle tient les dernières
/// références d'une chaîne UsdImaging. Son destructeur démonterait donc cette
/// chaîne *après* que les statiques de la bibliothèque USD sont partis — et
/// depuis le code d'un greffon que l'éditeur de liens dynamique peut avoir déjà
/// démappé. Mesuré le 17 septembre 2026 dans l'image de rendu : SIGSEGV à la
/// sortie, dans `HdMergingSceneIndex::RemoveInputScenes`, appelé depuis le
/// destructeur de cette table.
///
/// Ce que ça coûte : rien. Le processus se termine ; la mémoire rendue à la
/// sortie n'est pas de la mémoire récupérée. Ce que ça évite : un plantage
/// après la dernière frame, c'est-à-dire au pire moment — le travail est fait,
/// le code de sortie dit le contraire, et l'archive peut sortir tronquée.
HdSceneIndexBaseRefPtr
_ManifestScene(const std::string &path)
{
    static std::mutex &mutex = *new std::mutex();
    static std::unordered_map<std::string, HdSceneIndexBaseRefPtr> &open =
        *new std::unordered_map<std::string, HdSceneIndexBaseRefPtr>();

    std::lock_guard<std::mutex> lock(mutex);
    auto it = open.find(path);
    if (it != open.end()) {
        return it->second;
    }

    UsdStageRefPtr stage = UsdStage::Open(path);
    if (!stage) {
        TF_RUNTIME_ERROR(
            "tuile: TUILE_MANIFEST=%s did not open — the globe will be absent "
            "from this render, and nothing else will say so.",
            path.c_str());
        open[path] = nullptr;
        return nullptr;
    }

    // No `SetTime`. The Globe prim's arguments are all uniform — the manifest
    // authors them without time samples (`tuile-usd/src/stage.rs`) — and the
    // procedural selects tiles from the camera's pose, not from a time code.
    // The manifest's own camera does carry samples, and is deliberately left at
    // the default time: nothing reads it since the camera rung took
    // `/freeCamera`, which the host moves.
    UsdImagingCreateSceneIndicesInfo info;
    info.stage = stage;
    const UsdImagingSceneIndices indices = UsdImagingCreateSceneIndices(info);
    open[path] = indices.finalSceneIndex;
    return indices.finalSceneIndex;
}

/// L'hôte a-t-il déjà inséré le manifeste lui-même ?
///
/// Les deux voies ci-dessous mènent au même endroit, et prendre les deux
/// mettrait le globe deux fois dans la scène — deux surfaces coplanaires,
/// c'est-à-dire du z-fighting sur toute la Terre.
bool &
_InsertedDirectly()
{
    static bool inserted = false;
    return inserted;
}

}  // namespace

TF_REGISTRY_FUNCTION(TfType)
{
    HdSceneIndexPluginRegistry::Define<TuileManifestSceneIndexPlugin>();
}

/// La voie normale : la registry des greffons de scene index.
///
/// Elle marche pour tout hôte qui charge notre bibliothèque avant de construire
/// son render index — `usdrecord`, un test, un outil à nous. Elle ne marche PAS
/// depuis Blender, et c'est pour ça que le pont C plus bas existe ; voir son
/// commentaire. Les deux sont gardés : celui-ci est testé
/// (`tests/manifestRungTest.cpp`) et reste la route standard.
TF_REGISTRY_FUNCTION(HdSceneIndexPlugin)
{
    // Pour TOUS les renderers : la chaîne vide, que la registry lit comme
    // « n'importe lequel ».
    HdSceneIndexPluginRegistry::GetInstance().RegisterSceneIndexForRenderer(
        std::string(),
        TfToken("TuileManifestSceneIndexPlugin"),
        nullptr,
        TuileManifestSceneIndexPlugin::GetInsertionPhase(),
        HdSceneIndexPluginRegistry::InsertionOrderAtStart);
}

TuileManifestSceneIndexPlugin::TuileManifestSceneIndexPlugin() = default;

HdSceneIndexBaseRefPtr
TuileManifestSceneIndexPlugin::_AppendSceneIndex(
    const HdSceneIndexBaseRefPtr &inputScene,
    const HdContainerDataSourceHandle & /*inputArgs*/)
{
    if (_InsertedDirectly()) {
        return inputScene;
    }
    // Lu à chaque appel plutôt que mis en cache : l'hôte pose la variable avant
    // de construire son render index, et un `static` figerait ce qu'elle valait
    // au premier passage.
    const std::string path = TfGetenv("TUILE_MANIFEST");
    if (path.empty()) {
        return inputScene;
    }
    const HdSceneIndexBaseRefPtr manifest = _ManifestScene(path);
    if (!manifest) {
        return inputScene;
    }

    // Les deux à la racine. La scène de l'hôte est elle aussi insérée sans
    // préfixe, et les chemins du manifeste (`/World/...`) ne rencontrent pas
    // ceux que Blender émet (`/scene/...`).
    HdMergingSceneIndexRefPtr merged = HdMergingSceneIndex::New();
    merged->AddInputScene(inputScene, SdfPath::AbsoluteRootPath());
    merged->AddInputScene(manifest, SdfPath::AbsoluteRootPath());
    fprintf(stderr, "MANIFEST-MERGED %s\n", path.c_str());
    fflush(stderr);
    return merged;
}

PXR_NAMESPACE_CLOSE_SCOPE

/// Le pont : l'hôte insère le manifeste dans SON render index.
///
/// Hors du namespace versionné d'USD et sans décoration C++, pour être résolu
/// par un simple `dlsym` depuis le fork de Blender — même pont que celui des
/// images externes, et pour la même raison : la logique reste ici, l'hôte ne
/// porte que l'appel.
///
/// **Pourquoi pas la registry des greffons**, qui est juste au-dessus et qui
/// est la route normale. Parce qu'elle exige que notre bibliothèque soit
/// inscrite AVANT que le render index existe, et que depuis Blender ce moment
/// n'est pas atteignable. Trois tentatives, trois murs, tous mesurés le
/// 17 septembre 2026 :
///
///   - `TF_REGISTRY_FUNCTION(HdSceneIndexPlugin)` ne part qu'à l'abonnement au
///     tag, que fait le constructeur de `HdSceneIndexPluginRegistry` — c'est-à-
///     dire à la construction du render index, trop tard pour celui-là. Tant
///     que hdGp chargeait notre bibliothèque pour y résoudre le type d'un
///     procédural trouvé dans la stage exportée, l'abonnement avait déjà eu
///     lieu et la fonction partait aussitôt ; sans export, plus personne ne la
///     réclame. Symptôme : bibliothèque chargée, `_AppendSceneIndex` jamais
///     appelé, huit frames d'un bleu uniforme en 0,74 s chacune.
///   - un initialiseur statique qui s'inscrit au chargement INTERBLOQUE :
///     `Plug.Load()` tient le mutex de `PlugRegistry`, que construire la
///     registry des scene index redemande.
///   - un appel explicite depuis le Python de Blender interbloque aussi —
///     vingt minutes de L4 à 3 % de GPU.
///
/// Un scene index inséré, lui, est un frère de `/freeCamera` en entrée du
/// merging scene index, donc en AMONT de toute la chaîne de greffons : hdGp le
/// voit entier et résout le procédural. L'ordre d'insertion ne se négocie plus.
///
/// Rend 1 si le manifeste est entré, 0 s'il n'y avait rien à faire, -1 sur
/// erreur. L'hôte n'a pas à les distinguer : chacun est déjà dit ici.
extern "C" int
tuile_insert_manifest(void *renderIndex)
{
    if (renderIndex == nullptr) {
        return -1;
    }
    const std::string path = PXR_NS::TfGetenv("TUILE_MANIFEST");
    if (path.empty()) {
        fprintf(stderr, "MANIFEST-IDLE — TUILE_MANIFEST n'est pas posée\n");
        fflush(stderr);
        return 0;
    }
    const PXR_NS::HdSceneIndexBaseRefPtr manifest = PXR_NS::_ManifestScene(path);
    if (!manifest) {
        return -1;
    }
    static_cast<PXR_NS::HdRenderIndex *>(renderIndex)->InsertSceneIndex(
        manifest, PXR_NS::SdfPath::AbsoluteRootPath(), /* needsPrefixing */ false);
    PXR_NS::_InsertedDirectly() = true;
    fprintf(stderr, "MANIFEST-INSERTED %s\n", path.c_str());
    fflush(stderr);
    return 1;
}
