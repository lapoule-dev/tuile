// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The page is Rust (`tuile_film_web::page`). This loads it and lets it start.

import init, { start_page } from "./pkg/tuile_film_web.js";

await init();
start_page();
