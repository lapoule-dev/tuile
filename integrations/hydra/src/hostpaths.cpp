// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "hostpaths.h"

#include <pxr/base/tf/getenv.h>
#include <pxr/imaging/hd/dataSource.h>
#include <pxr/imaging/hd/tokens.h>

PXR_NAMESPACE_OPEN_SCOPE

namespace {

/// Does a prim carrying data live at `path`?
///
/// Not `dataSource != nullptr`. A prefixing scene index answers every ancestor
/// of its prefix with an EMPTY retained container so the namespace stays
/// connected — `/` and `/usd_scene`'s parents are all "present" and all carry
/// nothing. Asking for names is what separates a prim from a placeholder.
bool
_Populated(const HdSceneIndexBaseRefPtr &scene, const SdfPath &path)
{
    if (path.IsEmpty() || !path.IsAbsolutePath()) {
        return false;
    }
    HdSceneIndexPrim prim = scene->GetPrim(path);
    if (!prim.dataSource) {
        return false;
    }
    return !prim.dataSource->GetNames().empty();
}

}  // namespace

SdfPath
TuileResolveHostPath(const HdSceneIndexBaseRefPtr &scene,
                     const SdfPath &authored,
                     const SdfPath &anchor)
{
    if (!scene || authored.IsEmpty() || !authored.IsAbsolutePath()) {
        return SdfPath();
    }
    if (_Populated(scene, authored)) {
        return authored;
    }

    // Longest prefix first: with an anchor at `/usd_scene/World/Globe` we try
    // `/usd_scene/World`, then `/usd_scene`, and the deepest host prefix that
    // actually holds the prim wins. A shallower one can only match by
    // coincidence, and trying it first is how you resolve to the wrong prim.
    const SdfPath relative = authored.MakeRelativePath(SdfPath::AbsoluteRootPath());
    for (SdfPath at = anchor.GetParentPath();
         !at.IsEmpty() && at != SdfPath::AbsoluteRootPath();
         at = at.GetParentPath())
    {
        const SdfPath candidate = at.AppendPath(relative);
        if (_Populated(scene, candidate)) {
            return candidate;
        }
    }
    return SdfPath();
}

const SdfPath &
TuileRenderCameraPath()
{
    // Résolu une fois : c'est une propriété de l'hôte, pas de la frame.
    static const SdfPath path = [] {
        const std::string named = TfGetenv("TUILE_RENDER_CAMERA");
        if (!named.empty() && SdfPath::IsValidPathString(named)) {
            return SdfPath(named);
        }
        return SdfPath("/freeCamera");
    }();
    return path;
}

SdfPath
TuileSelectCamera(const HdSceneIndexBaseRefPtr &scene,
                  const SdfPath &anchor,
                  const SdfPath &authored)
{
    if (!scene) {
        return SdfPath();
    }
    // Le prim doit être une caméra, pas seulement présent : le chemin est
    // devinable par défaut, et servir un Xform vide comme caméra donnerait une
    // vue à l'identité, c'est-à-dire une sélection au centre de la Terre.
    const SdfPath &render = TuileRenderCameraPath();
    if (scene->GetPrim(render).primType == HdPrimTypeTokens->camera) {
        return render;
    }
    return TuileResolveHostPath(scene, authored, anchor);
}

HdSceneGlobalsSchema
TuileSceneGlobals(const HdSceneIndexBaseRefPtr &scene, const SdfPath &anchor)
{
    if (!scene) {
        return HdSceneGlobalsSchema(nullptr);
    }
    if (HdSceneGlobalsSchema globals = HdSceneGlobalsSchema::GetFromSceneIndex(scene)) {
        return globals;
    }
    // Same climb as above, and for the same reason: the stage's root data
    // source is at the prefix, not at `/`.
    for (SdfPath at = anchor.GetParentPath();
         !at.IsEmpty() && at != SdfPath::AbsoluteRootPath();
         at = at.GetParentPath())
    {
        HdSceneIndexPrim prim = scene->GetPrim(at);
        if (HdSceneGlobalsSchema globals =
                HdSceneGlobalsSchema::GetFromParent(prim.dataSource)) {
            return globals;
        }
    }
    return HdSceneGlobalsSchema(nullptr);
}

PXR_NAMESPACE_CLOSE_SCOPE
