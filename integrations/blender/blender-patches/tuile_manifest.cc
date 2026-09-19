/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: GPL-2.0-or-later */

#include "tuile_manifest.hh"

#include <cstdio>
#include <cstdlib>
#include <mutex>

#ifndef _WIN32
#  include <dlfcn.h>
#endif

namespace blender::render::hydra {

namespace {

using InsertFn = int (*)(void *);

/* Resolved once, on first use.
 *
 * Once, because the answer cannot change and because a missing library must be
 * reported once rather than per render index. */
InsertFn the_insert_fn()
{
  static InsertFn fn = nullptr;
  static std::once_flag once;
  std::call_once(once, []() {
#ifndef _WIN32
    const char *path = getenv("TUILE_HYDRA_LIB");
    if (path == nullptr || path[0] == '\0') {
      return;
    }
    /* Kept open deliberately: the scene index it inserts stays in the render
     * index, and closing the library would unmap the code behind it. */
    void *lib = dlopen(path, RTLD_LAZY | RTLD_LOCAL);
    if (lib == nullptr) {
      fprintf(stderr, "TUILE_HYDRA_LIB=%s did not load: %s\n", path, dlerror());
      fflush(stderr);
      return;
    }
    fn = (InsertFn)dlsym(lib, "tuile_insert_manifest");
    if (fn == nullptr) {
      fprintf(stderr, "%s has no tuile_insert_manifest; nothing is inserted\n", path);
      fflush(stderr);
    }
#endif
  });
  return fn;
}

}  // namespace

void tuile_insert_manifest(void *render_index)
{
  if (render_index == nullptr) {
    return;
  }
  /* Once per process. The engine builds its render index once and keeps it for
   * the whole sequence; inserting twice would put the same scene in twice,
   * which on a globe means two coplanar surfaces everywhere. */
  static bool done = false;
  if (done) {
    return;
  }
  const InsertFn fn = the_insert_fn();
  if (fn == nullptr) {
    return;
  }
  done = true;
  fn(render_index);
}

}  // namespace blender::render::hydra
