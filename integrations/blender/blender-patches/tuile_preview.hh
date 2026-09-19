/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

/* How often a final render refreshes the picture nobody is looking at.
 *
 * `FinalEngine::render()` polls in a loop with no pause, and every turn of that
 * loop calls `update_render_result()`: it allocates a `RenderResult` the size of
 * the frame, reads every AOV back out of the Hydra render buffers, copies them
 * in, and frees it again.
 *
 * At 1920x1440 that is 2.7 Mpixels — some 55 MB a turn, and nobody notices. At
 * 7680x5760 it is 44.2 Mpixels: roughly 880 MB allocated, copied and freed per
 * turn, on one thread. The core saturates, the path tracer is starved, and the
 * render does not advance. Measured on an L4 the 18th of September 2026: CPU
 * flat at 13.1% (one core of eight), GPU compute at 0%, VRAM unchanged at
 * 1.5 GiB, eighteen minutes for the first frame of a single-sample render that
 * takes eleven seconds at 1920.
 *
 * The cost also happens to be what paces the loop, so a fix that only removes
 * the copy turns the poll into a hot spin. Hence both halves here: refresh at
 * most every `TUILE_PREVIEW_INTERVAL` seconds, and sleep briefly on the turns
 * that skip it.
 *
 * A batch render has no viewer at all, and the call after the loop is what
 * actually produces the image — so on a farm the right interval is "never".
 *
 * Unset, zero or negative keeps Blender's behaviour exactly: refresh every
 * turn, no sleep. */

#pragma once

namespace blender::render::hydra {

/* Seconds between two in-loop refreshes; <= 0 means every turn, as upstream. */
double tuile_preview_interval();

/* Is a refresh due at `now`? Advances `*last` when it says yes. */
bool tuile_preview_due(double now, double *last, double interval);

}  // namespace blender::render::hydra
