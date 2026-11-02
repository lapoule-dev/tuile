<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Recorded responses

Answers recorded from the live services on 2026-10-31, with every credential
replaced by a placeholder (`FAKE_KEY`, `FAKE_SESSION`) and the trees cut down
to the few tiles a test walks:

- `external-3dtiles-endpoint.json` — `/v1/assets/2275207/endpoint`: a 3D Tiles
  asset ion does not host itself. No `url`, no `accessToken`; the tileset is
  at `options.url`, under a key carried as a query parameter.
- `external-3dtiles-root.json` — the root document that URL answers with: its
  content URIs carry a session parameter and not the key, and the external
  tilesets below them carry neither.
