/* SPDX-FileCopyrightText: 2023 Blender Authors
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

/* lapoule.dev build-fork patch: feed Hydra through the USD stage scene
 * index (Hydra 2.0) instead of the legacy UsdImagingDelegate, and resolve
 * hdGp generative procedurals on the way. Upstream Blender's own Hydra 2.0
 * migration will make this patch obsolete. */

#pragma once

#include <string>

#include <pxr/imaging/hd/renderIndex.h>
#include <pxr/imaging/hd/sceneIndex.h>
#include <pxr/usd/usd/stage.h>
#include <pxr/usdImaging/usdImaging/stageSceneIndex.h>

namespace blender {

struct Depsgraph;

namespace io::hydra {

/* Populate Hydra render index using USD file export, for testing. */
class USDSceneDelegate {
 private:
  pxr::HdRenderIndex *render_index_;
  pxr::SdfPath const delegate_id_;
  pxr::UsdStageRefPtr stage_;
  pxr::UsdImagingStageSceneIndexRefPtr stage_index_;
  pxr::HdSceneIndexBaseRefPtr terminal_index_;

  std::string temp_dir_;
  std::string temp_file_;

  bool use_materialx = true;

 public:
  USDSceneDelegate(pxr::HdRenderIndex *render_index,
                   pxr::SdfPath const &delegate_id,
                   bool use_materialx);
  ~USDSceneDelegate();

  void populate(Depsgraph *depsgraph);
};

}  // namespace io::hydra
}  // namespace blender
