// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#ifndef TUILE_HYDRA_HOSTPATHS_H
#define TUILE_HYDRA_HOSTPATHS_H

#include <pxr/imaging/hd/sceneIndex.h>
#include <pxr/imaging/hd/sceneGlobalsSchema.h>
#include <pxr/usd/sdf/path.h>

PXR_NAMESPACE_OPEN_SCOPE

/// Reading a stage's own paths from inside a host that moved them.
///
/// A path authored on a stage — `/World/ShotCam` in a `tuile:cameras` primvar
/// — names a prim in the stage's namespace. The scene a procedural is handed
/// is not always that namespace. `HdRenderIndex::InsertSceneIndex` prefixes by
/// default, and Blender inserts its USD scene under `/usd_scene`
/// (`render/hydra/engine.cc`), so the prim is at `/usd_scene/World/ShotCam`.
///
/// `HdPrefixingSceneIndex` rewrites path-VALUED data sources as it goes
/// (`prefixingSceneIndex.cpp`, `Hd_PrefixingSceneIndexPathDataSource`), so
/// anything Hydra hands us as an `SdfPath` is already correct and must be left
/// alone. What it cannot rewrite is a path carried as **text**: USD has no
/// path-valued attribute type, so `tuile:cameras` is a `string`, and a string
/// is opaque to prefixing. It arrives exactly as authored, pointing at a prim
/// that is no longer there.
///
/// Whether we are prefixed depends on where the resolver sits: inserted into
/// the UsdImaging chain we are upstream of prefixing and the authored path is
/// literal; resolved by OpenUSD's own `HdGpSceneIndexPlugin` we are at the end
/// of the render index and everything is prefixed. Both are legitimate, so
/// neither is assumed — these functions ask the scene.
///
/// The anchor is our own prim path, the one path we know the host's opinion
/// of, and therefore the only place a prefix can be read from.

/// Where `authored` actually lives in `scene`, or an empty path if nowhere.
///
/// Tries the authored path, then the same path re-rooted under each ancestor
/// of `anchor`, longest first. Empty when nothing matches, which the caller
/// must treat as "not found" rather than as "root".
SdfPath
TuileResolveHostPath(const HdSceneIndexBaseRefPtr &scene,
                     const SdfPath &authored,
                     const SdfPath &anchor);

/// Le chemin de la caméra par laquelle l'hôte rend réellement.
///
/// `TUILE_RENDER_CAMERA` sinon `/freeCamera`, qui est celui de Blender :
/// `render/hydra/engine.cc` crée une caméra à ce chemin dans un scene index
/// retenu inséré à la racine sans préfixage, puis fait
/// `render_task_delegate_->set_camera(free_camera_delegate_->GetCameraId())`.
/// C'est donc littéralement la caméra de la tâche de rendu — et elle est mise
/// à jour d'un `DirtyPrims` par frame, sans que rien soit reconstruit.
const SdfPath &TuileRenderCameraPath();

/// La caméra pour laquelle une cuisson doit sélectionner.
///
/// La caméra du rendu d'abord, le chemin écrit sur le prim ensuite.
///
/// Cet ordre est un choix de JUSTESSE avant d'être un choix de vitesse. Le
/// chemin écrit — `primvars:tuile:cameras` — nomme une caméra *dans la stage*,
/// et un hôte peut n'en tenir qu'une copie : Blender exporte la sienne à
/// chaque frame sous `/usd_scene/...` et rend, lui, par `/freeCamera`. Cuire
/// pour la copie, c'est sélectionner les tuiles pour une caméra qui n'est pas
/// celle de l'image ; tant qu'elles coïncident personne ne le voit, et le jour
/// où elles divergent le défaut est invisible dans tous les compteurs.
///
/// Et c'est ce qui rend la cuisson incrémentale possible : la caméra du rendu
/// bouge sans que la stage change, donc l'hôte n'a plus de raison de la
/// reconstruire entre deux frames.
///
/// Rend un chemin vide si aucune des deux ne désigne un prim caméra présent —
/// l'appelant a d'autres barreaux après celui-ci.
SdfPath TuileSelectCamera(const HdSceneIndexBaseRefPtr &scene,
                          const SdfPath &anchor,
                          const SdfPath &authored);

/// The scene globals, wherever the host's prefixing left the root prim.
///
/// `HdSceneGlobalsSchema::GetDefaultPrimPath()` is `/`, and under a prefixing
/// scene index the stage's root data source moves to the prefix — `/` then
/// answers with an empty container (`prefixingSceneIndex.cpp`, the
/// `_prefix.HasPrefix(primPath)` branch). Reading only `/` therefore finds
/// nothing at all in a prefixed host.
HdSceneGlobalsSchema
TuileSceneGlobals(const HdSceneIndexBaseRefPtr &scene, const SdfPath &anchor);

PXR_NAMESPACE_CLOSE_SCOPE

#endif  // TUILE_HYDRA_HOSTPATHS_H
