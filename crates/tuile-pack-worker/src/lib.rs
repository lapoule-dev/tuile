// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The bench's API as a Cloudflare Worker.
//!
//! The routes, and what each means, are `tuile_repository::bench`'s — the
//! same `Bench` the native server answers from. A Worker differs in one
//! thing: how it reaches a bucket. It has `fetch` and no S3 client, and a
//! bucket in another account cannot be bound to it, so it asks over HTTP
//! with requests `tuile_repository::s3` signs. This file is that `fetch`,
//! the edge cache as a place to keep chunks, and the translation of a
//! request and a reply.
//!
//! **A byte is read from a bucket once.** A pack's reader asks for its
//! fixed blocks, each a URL of its own answered whole and immutable. The
//! reply to a block is kept in the edge cache under that URL and served
//! from there to whoever asks next, before any of this crate's logic runs;
//! a browser keeps it too, as it keeps any file. Behind that, every object
//! goes through `Cached`, so the tables the repositories read and the odd
//! ranged request are cut from chunks the edge already holds.
//!
//! Configuration, all of it from the environment:
//!
//! - `BENCH_CONFIG` — the bench's TOML, as `film-bench.example.toml`
//!   describes it. Buckets only: a Worker has no directory to read.
//! - `TUILE_STORE_ENDPOINT`, `TUILE_STORE_ACCESS_KEY_ID`,
//!   `TUILE_STORE_SECRET_ACCESS_KEY` — the buckets' endpoint and a key that
//!   can read them. Secrets; never in `wrangler.toml`.
//!
//! Source tiles are not served from here yet: the tile store's reader is a
//! native one, and `/api/tiles` answers that there is none.
//!
//! Anything that is not `/api/…` is the page, from the `ASSETS` binding.

#[cfg(target_arch = "wasm32")]
mod bench;
