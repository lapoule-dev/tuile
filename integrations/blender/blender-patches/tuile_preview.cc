/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

#include "tuile_preview.hh"

#include <cstdlib>
#include <mutex>

namespace blender::render::hydra {

double tuile_preview_interval()
{
  /* Read once: it describes the machine, not the frame. */
  static double interval = 0.0;
  static std::once_flag once;
  std::call_once(once, []() {
    const char *set = getenv("TUILE_PREVIEW_INTERVAL");
    if (set == nullptr || set[0] == '\0') {
      return;
    }
    char *end = nullptr;
    const double parsed = strtod(set, &end);
    /* A value that does not parse leaves the upstream behaviour in place rather
     * than guessing: silently rendering at some other cadence than the one
     * asked for is worse than ignoring a typo. */
    if (end != set) {
      interval = parsed;
    }
  });
  return interval;
}

bool tuile_preview_due(double now, double *last, double interval)
{
  if (last == nullptr) {
    return true;
  }
  if (interval <= 0.0) {
    return true;
  }
  /* The first turn refreshes: `*last` starts at zero and `now` is a clock
   * reading far from it, so the picture appears without waiting a full
   * interval — which matters when the interval is long. */
  if (now - *last < interval) {
    return false;
  }
  *last = now;
  return true;
}

}  // namespace blender::render::hydra
