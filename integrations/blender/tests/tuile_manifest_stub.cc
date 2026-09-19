// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// La bibliothèque que `TUILE_HYDRA_LIB` désigne, réduite à ce que le pont en
// attend : un symbole du bon nom, et un compteur pour dire s'il a été atteint.

#include <cstdio>

namespace {
int calls = 0;
}

extern "C" int tuile_insert_manifest(void *render_index)
{
  (void)render_index;
  ++calls;
  return 1;
}

extern "C" int tuile_stub_calls()
{
  return calls;
}
