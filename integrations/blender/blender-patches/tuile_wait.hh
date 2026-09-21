/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

/* Waiting for a frame instead of asking whether it is done yet.
 *
 * `FinalEngine::render()` polls: every turn it calls `engine_->Execute()`,
 * reads the progress, asks `is_converged()`, and then calls
 * `update_render_result()` — which allocates a `RenderResult` the size of the
 * frame, reads every AOV out of the Hydra buffers, copies them in, and frees
 * it again. At 1920x1440 that is some 55 MB a turn and nobody notices. At
 * 3840x2160 it is about 166 MB, and at 7680x5760 about 880 MB, on one thread,
 * while the path tracer starves. Measured on an L4: CPU flat at 13.1 percent,
 * GPU compute at 0, and twenty-four minutes without finishing one frame that
 * takes seconds at a tenth of the resolution.
 *
 * The obvious cure — drop the copy — turns the poll into a hot spin, because
 * that copy is what paces the loop. `HdCyclesRenderPass::_Execute` does not
 * block: it calls `session->start()` (idempotent) and `session->draw()`.
 *
 * So the cure is not to remove the pacing but to stop needing it. Cycles has
 * the right mechanism and its own Blender path uses it: `Session::wait()`
 * blocks on a condition variable that the render thread notifies when the
 * frame is finished. What is missing is a way to reach it from here, since
 * `FinalEngine` speaks to Hydra and the delegate might be Storm.
 *
 * Hydra has the extension point for exactly that: `InvokeCommand`. The Cycles
 * delegate advertises `cycles:waitForConvergence` and answers it with
 * `session->wait()`. This asks whether the delegate offers it, and blocks if
 * it does.
 *
 * # What this does NOT do
 *
 * It never runs when a human is watching. `G.background` is the same
 * distinction Cycles draws throughout `BlenderSession` (`background &&
 * print_render_stats`, `!background` for interactive sync): with a viewport
 * open, the in-loop refresh IS the picture forming, and upstream behaviour is
 * preserved turn for turn.
 *
 * # The delegate that cannot block
 *
 * Storm advertises no such command. A batch render through it still must not
 * pay for a preview nobody will see, so this sleeps briefly instead and
 * reports that it waited. The copy is skipped either way; only the quality of
 * the wait differs. */

#pragma once

namespace pxr {
class HdRenderIndex;
}

namespace blender::render::hydra {

/* Waits for the frame, and says whether it did.
 *
 * True means the caller must NOT refresh the picture: this is a batch render
 * and the wait has already happened. False means a viewer is watching and
 * upstream's refresh is what should follow. */
bool tuile_wait_for_frame(pxr::HdRenderIndex *index);

}  // namespace blender::render::hydra
