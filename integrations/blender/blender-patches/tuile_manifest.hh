/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

/* A scene an embedder composes into the render, without a stage export.
 *
 * Blender's Hydra engine has two ways to hand a scene to a render delegate. The
 * fast path builds a native scene index and pushes only the delta each frame;
 * the other exports the whole scene to a USD file and rebuilds the imaging
 * chain from scratch, every frame — Blender's own source calls it "Slow USD
 * export for reference".
 *
 * Only the second one offers a seam for outside content: a `USDHook.on_export`
 * can compose a reference into the exported stage. An application whose content
 * cannot be represented as Blender data — a generative procedural, a streamed
 * scene, anything a `bpy` object does not map to — is therefore pinned to the
 * slow path, and pays a full teardown and rebuild per frame for the privilege.
 *
 * This adds one seam to the fast path. A shared library named by
 * `TUILE_HYDRA_LIB` may insert one scene index into the render index, once,
 * when the engine builds it:
 *
 *     int tuile_insert_manifest(void *render_index);   // 1 = inserted
 *
 * The library receives the `pxr::HdRenderIndex *` and decides everything else
 * for itself — what to open, where to root it, whether to act at all. Nothing
 * here knows about the content.
 *
 * An unset variable, a library that cannot be loaded, and a library that
 * answers 0 are all the same thing: the engine proceeds exactly as before. */

#pragma once

namespace blender::render::hydra {

/* Give an external library a chance to insert a scene index. Safe to call more
 * than once; only the first call per process can do anything.
 *
 * The render index travels as `void *`, deliberately. This header knows nothing
 * about the content being inserted and has no reason to know its types either —
 * and forward-declaring one would be wrong here anyway: in this build `pxr` is
 * an alias for a versioned namespace, so `namespace pxr { class HdRenderIndex; }`
 * defines a second, real `pxr` that shadows the alias and breaks every use of it
 * in the translation unit. */
void tuile_insert_manifest(void *render_index);

}  // namespace blender::render::hydra
