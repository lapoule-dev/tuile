// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// L'aperçu que personne ne regarde, et ce qu'il coûte de le rafraîchir.
//
// `blender-patches/tuile_preview.cc` ne décide qu'une chose — faut-il recopier
// l'image maintenant — mais cette chose gouverne dix-huit minutes de L4 par
// frame en 8K. Les trois propriétés testées ici sont celles dont dépend le
// correctif :
//
//   - sans intervalle, le comportement d'amont est intact. C'est la promesse
//     sur laquelle un patch dans la boucle de rendu de Blender est acceptable ;
//   - avec un intervalle, les tours rapprochés sont sautés ;
//   - et le PREMIER tour rafraîchit quand même, sinon un intervalle long
//     laisserait l'image vide pendant tout ce temps.
//
// Aucune dépendance : ni USD, ni Blender, ni horloge réelle — le temps est un
// paramètre, ce qui est précisément ce qui rend la décision testable.
//
//   c++ -std=c++17 -I<patches> tuile_preview_test.cc <patches>/tuile_preview.cc \
//       -o tuile_preview_test && ./tuile_preview_test

#include "tuile_preview.hh"

#include <cstdio>
#include <cstdlib>

using blender::render::hydra::tuile_preview_due;
using blender::render::hydra::tuile_preview_interval;

namespace {

int failures = 0;

void check(const bool ok, const char *what)
{
  printf("%s  %s\n", ok ? "ok  " : "RATÉ", what);
  if (!ok) {
    ++failures;
  }
}

}  // namespace

int main()
{
  setvbuf(stdout, nullptr, _IOLBF, 0);

  // 1. Sans intervalle, chaque tour rafraîchit — exactement comme Blender.
  {
    double last = 0.0;
    bool every = true;
    for (int i = 0; i < 5; ++i) {
      every = every && tuile_preview_due(1000.0 + i, &last, 0.0);
    }
    check(every, "sans intervalle, tous les tours rafraîchissent");
  }

  // 2. Avec un intervalle, les tours rapprochés sont sautés.
  {
    double last = 0.0;
    tuile_preview_due(1000.0, &last, 1.0);  // le premier passe et pose la borne
    check(!tuile_preview_due(1000.1, &last, 1.0), "un tour trop tôt est sauté");
    check(!tuile_preview_due(1000.9, &last, 1.0), "juste avant l'échéance, sauté");
    check(tuile_preview_due(1001.0, &last, 1.0), "à l'échéance, rafraîchi");
  }

  // 3. Le premier tour rafraîchit, quel que soit l'intervalle. Sinon une valeur
  //    de mille secondes laisserait l'image vide mille secondes.
  {
    double last = 0.0;
    check(tuile_preview_due(1000.0, &last, 1000.0),
          "le premier tour rafraîchit même avec un intervalle long");
  }

  // 4. Et la borne avance vraiment : deux tours d'affilée ne passent pas tous
  //    les deux. Une borne qui n'avance pas rendrait le réglage inopérant tout
  //    en ayant l'air posé.
  {
    double last = 0.0;
    tuile_preview_due(1000.0, &last, 10.0);
    check(!tuile_preview_due(1001.0, &last, 10.0), "la borne avance après un rafraîchissement");
  }

  // 5. L'environnement est lu, et une valeur illisible ne change rien.
  {
    setenv("TUILE_PREVIEW_INTERVAL", "pas-un-nombre", 1);
    check(tuile_preview_interval() == 0.0,
          "une valeur illisible laisse le comportement d'amont");
  }

  printf(failures ? "\n%d test(s) en échec\n" : "\ntous les tests passent\n", failures);
  return failures ? 1 : 0;
}
