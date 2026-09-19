// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// Le patch Blender qui laisse entrer le globe, testé hors de Blender.
//
// `blender-patches/tuile_manifest.cc` est petit, mais il porte trois décisions
// dont chacune a une façon silencieuse de mal tourner :
//
//   - sans `TUILE_HYDRA_LIB`, il ne doit RIEN faire. C'est la promesse sur
//     laquelle un patch dans le moteur de rendu de Blender est acceptable ;
//   - il ne doit insérer qu'UNE fois par processus. Deux insertions mettent le
//     globe deux fois dans la scène, c'est-à-dire deux surfaces coplanaires sur
//     toute la Terre — du z-fighting qu'aucun compteur ne voit ;
//   - un pointeur nul ne doit pas l'atteindre.
//
// Rien de tout ça n'a besoin d'USD : le pont ne fait que passer un pointeur
// opaque. Le test se compile donc avec le seul fichier sous test et une
// bibliothèque bouchon qui compte ses appels — pas de Blender, pas de GPU, pas
// de render index.
//
// Chaque cas tourne dans SON processus, par `fork`. La résolution de la
// bibliothèque est mémorisée au premier appel — c'est voulu, un `dlopen` par
// texture serait absurde — donc deux cas dans le même processus mesureraient la
// mémoire du premier au lieu de ce qu'ils croient mesurer.

#include "tuile_manifest.hh"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

#include <dlfcn.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

const char *lib_path = nullptr;

/// Combien de fois le bouchon a-t-il été appelé ?
///
/// Lu DANS la bibliothèque bouchon plutôt que compté ici : c'est la seule
/// façon de savoir si l'appel a réellement traversé `dlsym`, et non si notre
/// propre code croit l'avoir fait.
int stub_calls()
{
  void *h = dlopen(lib_path, RTLD_LAZY | RTLD_LOCAL);
  if (h == nullptr) {
    fprintf(stderr, "le bouchon %s ne se charge pas: %s\n", lib_path, dlerror());
    return -1;
  }
  auto counter = (int (*)())dlsym(h, "tuile_stub_calls");
  return counter ? counter() : -1;
}

/// Un pointeur quelconque : le pont ne le déréférence jamais, il le passe.
void *fake_render_index()
{
  static int marker = 0;
  return &marker;
}

using Case = int (*)();

/// Rend 0 si le cas passe, 1 sinon — c'est le code de sortie de l'enfant.
int run_in_child(const Case body, const char *what)
{
  const pid_t pid = fork();
  if (pid == 0) {
    _exit(body());
  }
  int status = 0;
  waitpid(pid, &status, 0);
  const bool ok = WIFEXITED(status) && WEXITSTATUS(status) == 0;
  printf("%s  %s\n", ok ? "ok  " : "RATÉ", what);
  return ok ? 0 : 1;
}

int case_no_variable()
{
  unsetenv("TUILE_HYDRA_LIB");
  blender::render::hydra::tuile_insert_manifest(fake_render_index());
  return stub_calls() == 0 ? 0 : 1;
}

int case_null_render_index()
{
  setenv("TUILE_HYDRA_LIB", lib_path, 1);
  blender::render::hydra::tuile_insert_manifest(nullptr);
  return stub_calls() == 0 ? 0 : 1;
}

int case_inserts_once()
{
  setenv("TUILE_HYDRA_LIB", lib_path, 1);
  blender::render::hydra::tuile_insert_manifest(fake_render_index());
  return stub_calls() == 1 ? 0 : 1;
}

int case_never_twice()
{
  setenv("TUILE_HYDRA_LIB", lib_path, 1);
  for (int i = 0; i < 5; ++i) {
    blender::render::hydra::tuile_insert_manifest(fake_render_index());
  }
  return stub_calls() == 1 ? 0 : 1;
}

int case_missing_library()
{
  setenv("TUILE_HYDRA_LIB", "/nonexistent/pas-une-bibliotheque.so", 1);
  // Ne doit pas planter, et ne doit rien appeler. Un moteur de rendu qui meurt
  // parce qu'une variable pointe à côté est pire que pas de greffe du tout.
  blender::render::hydra::tuile_insert_manifest(fake_render_index());
  return stub_calls() == 0 ? 0 : 1;
}

}  // namespace

int main(int argc, char **argv)
{
  setvbuf(stdout, nullptr, _IOLBF, 0);
  if (argc < 2) {
    fprintf(stderr, "usage: %s <chemin du bouchon>\n", argv[0]);
    return 2;
  }
  lib_path = argv[1];

  int failures = 0;
  failures += run_in_child(case_no_variable,
                           "sans TUILE_HYDRA_LIB, la bibliothèque n'est pas appelée");
  failures += run_in_child(case_null_render_index, "un render index nul est refusé ici");
  failures += run_in_child(case_inserts_once, "avec la variable, la bibliothèque est appelée");
  failures += run_in_child(case_never_twice, "cinq appels n'insèrent qu'une fois");
  failures += run_in_child(case_missing_library,
                           "une bibliothèque absente est sans effet, pas fatale");

  printf(failures ? "\n%d test(s) en échec\n" : "\ntous les tests passent\n", failures);
  return failures ? 1 : 0;
}
