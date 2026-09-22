/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

#include "tuile_wait.hh"

#include <chrono>
#include <cstdio>

#include <pxr/base/tf/getenv.h>
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

/* Chaque tour, dit à voix haute.
 *
 * Le 22 septembre 2026, un rendu 4K s'est immobilisé à zéro pour cent de CPU
 * ET zéro pour cent de GPU, 6,3 Go résidents sur la carte, sept minutes durant,
 * après la ligne « frame 1 : début » et avant toute autre. Rien dans le journal
 * ne permettait de dire si l'attente ci-dessous était entrée, ni si elle était
 * ressortie — et une attente dont on ne sait pas si elle rend la main est
 * indistinguable d'un interblocage.
 *
 * Donc : un compteur, le temps passé, et surtout une ligne AVANT la commande
 * autant qu'APRÈS. C'est la paire qui répond à la question ; une seule des deux
 * ne répond à rien. `TUILE_WAIT_QUIET=1` les fait taire quand un film long
 * n'en a plus besoin. */
static void wait_log(const char *what, int turn, double seconds)
{
  static const bool quiet = pxr::TfGetenvBool("TUILE_WAIT_QUIET", false);
  if (quiet) {
    return;
  }
  if (seconds < 0.0) {
    printf("WAIT[%d] %s\n", turn, what);
  }
  else {
    printf("WAIT[%d] %s %.3fs\n", turn, what, seconds);
  }
  fflush(stdout);
}

bool tuile_wait_for_frame(pxr::HdRenderIndex *index)
{
  static int turn = 0;
  ++turn;

  /* A viewport is watching: the refresh upstream does after this IS the
   * picture forming, and nothing here should change what it sees. */
  if (!G.background) {
    wait_log("viewport-visible, upstream refreshes", turn, -1.0);
    return false;
  }
  if (index == nullptr) {
    wait_log("no render index", turn, -1.0);
    return false;
  }
  pxr::HdRenderDelegate *delegate = index->GetRenderDelegate();
  if (delegate == nullptr) {
    wait_log("no render delegate", turn, -1.0);
    return false;
  }

  /* Le sondage par defaut, et le blocage sur demande.
   *
   * Bloquer est plus propre et c'est la cible. Mais `Session::wait()` n'est
   * correct que si la session se sait hors-ligne : hdCycles ecrit
   * `params.background = false` en dur, et dans ce mode `run_wait_for_work`
   * gare le fil de rendu dans `pause_cond_.wait()` en le laissant a l'etat
   * SESSION_THREAD_RENDER. L'attente porte alors sur une transition qui
   * n'arrivera jamais -- mesure le 22 septembre 2026, sept minutes a zero pour
   * cent de CPU et zero pour cent de GPU.
   *
   * D'ou le defaut : un reglage absent, ou mal mis, doit degrader vers le
   * sondage et non figer la ferme. `TUILE_WAIT_MODE=command` demande le
   * blocage, et n'a de sens qu'avec `CYCLES_BACKGROUND=1`. */
  static const bool blocking = pxr::TfGetenv("TUILE_WAIT_MODE", "poll") == "command";
  if (!blocking) {
    wait_log("poll mode, sleeping", turn, POLL_MS / 1000.0);
    BLI_time_sleep_ms(POLL_MS);
    return true;
  }

  /* Asked rather than assumed. The descriptor list is the delegate's own
   * statement about what it can do, and reading it costs a vector of tokens
   * once per frame - this loop turns once per frame when the wait works. */
  for (const pxr::HdCommandDescriptor &descriptor : delegate->GetCommandDescriptors()) {
    if (descriptor.commandName == wait_command()) {
      wait_log("entering cycles:waitForConvergence", turn, -1.0);
      const auto t0 = std::chrono::steady_clock::now();
      const bool ok = delegate->InvokeCommand(wait_command(), pxr::HdCommandArgs());
      const double elapsed = std::chrono::duration<double>(
          std::chrono::steady_clock::now() - t0).count();
      wait_log(ok ? "returned from wait, converged" : "wait refused", turn, elapsed);
      if (ok) {
        return true;
      }
      break;
    }
  }

  /* No blocking wait here. Still a batch render, so still no preview — just a
   * worse wait. */
  wait_log("no blocking wait advertised, sleeping", turn, POLL_MS / 1000.0);
  BLI_time_sleep_ms(POLL_MS);
  return true;
}

}  // namespace blender::render::hydra
