// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

/// How the globe enters a host that renders its own scene.
///
/// The globe is a `GenerativeProcedural` prim. No Blender object maps to it, so
/// it cannot be imported; it has to be composed into the scene the host hands
/// to Hydra. Until now that was done through a `USDHook.on_export`, which only
/// fires on Blender's USD export path — the one Blender's own source calls
/// "Slow USD export for reference", and which tears the whole scene down and
/// rebuilds it once per frame (`USDSceneIndex::populate`, verified at v5.2.2).
///
/// Blender's fast path never exports a stage, so that door does not exist
/// there. This is the other door, and it is the one OpenUSD ships for exactly
/// this purpose: a scene index plugin registered for every renderer, which
/// merges a stage of our own into the chain the render index builds.
///
/// **Ordering is the whole design.** `HdGpSceneIndexPlugin` — the resolver that
/// turns our procedural prim into geometry — sits at insertion phase 2, and its
/// header says so explicitly: "allow plugins to run before and after this
/// plugin (i.e., don't use 0)". We take phase 1. The manifest is therefore
/// merged in *upstream* of procedural resolution, which is what makes the
/// procedural visible to the thing that cooks it.
///
/// Being in the plugin chain also means we see the whole merged scene, the
/// host's prims included — `/freeCamera` among them. That is not incidental:
/// with no export there is no exported camera to read, and `/freeCamera` is
/// both the camera the image is actually made by and the only one the host
/// still moves. See `hostpaths.h` and `cameraRungTest.cpp`.
///
/// Inert without `TUILE_MANIFEST`: no variable, no stage, the input scene is
/// returned untouched.

#ifndef TUILE_MANIFEST_H
#define TUILE_MANIFEST_H

#include <pxr/pxr.h>

#include <pxr/imaging/hd/sceneIndexPlugin.h>
#include <pxr/imaging/hd/sceneIndexPluginRegistry.h>

PXR_NAMESPACE_OPEN_SCOPE

class TuileManifestSceneIndexPlugin : public HdSceneIndexPlugin
{
public:
    /// Strictly before hdGp's 2. A manifest merged in after the resolver would
    /// be a procedural prim nobody ever cooks: geometry silently absent, with
    /// no error anywhere — the failure mode this number exists to avoid.
    static const HdSceneIndexPluginRegistry::InsertionPhase GetInsertionPhase()
    {
        return 1;
    }

    TuileManifestSceneIndexPlugin();

protected:
    /// Pas de `_IsEnabled`, et c'est délibéré : ce point d'extension n'existe
    /// pas dans l'USD 26.03 contre lequel l'image de rendu est liée, il est
    /// arrivé plus tard. La décision se prend donc ici, où les deux versions
    /// s'accordent — sans `TUILE_MANIFEST`, on rend la scène d'entrée telle
    /// quelle. Voir `manifest.cpp`.
    HdSceneIndexBaseRefPtr _AppendSceneIndex(
        const HdSceneIndexBaseRefPtr &inputScene,
        const HdContainerDataSourceHandle &inputArgs) override;
};

PXR_NAMESPACE_CLOSE_SCOPE

#endif  // TUILE_MANIFEST_H
