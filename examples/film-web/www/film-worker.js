// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// A worker is Rust (`tuile_film_web::slice`). This loads it and lets it wait
// for its order — which the page may send before the module is ready: a
// message that arrives early is kept and handed over.

import init, { start_slice } from "./pkg/tuile_film_web.js";

const early = [];
self.onmessage = (event) => early.push(event);
await init();
start_slice();
for (const event of early) self.onmessage(event);
