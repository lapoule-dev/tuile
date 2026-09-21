/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

#include "tuile_wait.hh"

#include <pxr/imaging/hd/command.h>
#include <pxr/imaging/hd/renderDelegate.h>
#include <pxr/imaging/hd/renderIndex.h>

#include "BKE_global.hh"
#include "BLI_time.h"

namespace blender::render::hydra {

/* The command the Cycles delegate advertises. Spelled the same in both, and
 * nowhere else: a delegate that does not know it simply does not offer it. */
static const pxr::TfToken &wait_command()
{
  static const pxr::TfToken token("cycles:waitForConvergence", pxr::TfToken::Immortal);
  return token;
}

/* How long a delegate with no blocking wait is left alone between questions.
 *
 * It bounds one thing only: how late a finished frame is noticed. A delegate
 * that CAN block notices it immediately and never reaches this. Fifty
 * milliseconds against a frame measured in seconds is under a percent, and the
 * alternative — asking as fast as the CPU allows — is a core burnt to learn
 * nothing. */
static const int POLL_MS = 50;

bool tuile_wait_for_frame(pxr::HdRenderIndex *index)
{
  /* A viewport is watching: the refresh upstream does after this IS the
   * picture forming, and nothing here should change what it sees. */
  if (!G.background) {
    return false;
  }
  if (index == nullptr) {
    return false;
  }
  pxr::HdRenderDelegate *delegate = index->GetRenderDelegate();
  if (delegate == nullptr) {
    return false;
  }

  /* Asked rather than assumed. The descriptor list is the delegate's own
   * statement about what it can do, and reading it costs a vector of tokens
   * once per frame — this loop turns once per frame when the wait works. */
  for (const pxr::HdCommandDescriptor &descriptor : delegate->GetCommandDescriptors()) {
    if (descriptor.commandName == wait_command()) {
      if (delegate->InvokeCommand(wait_command(), pxr::HdCommandArgs())) {
        return true;
      }
      break;
    }
  }

  /* No blocking wait here. Still a batch render, so still no preview — just a
   * worse wait. */
  BLI_time_sleep_ms(POLL_MS);
  return true;
}

}  // namespace blender::render::hydra
