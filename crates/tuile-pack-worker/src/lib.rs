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
//! **A byte is read from a bucket once.** Every object goes through
//! `Cached`: it is read in fixed chunks, each kept in the edge cache, and a
//! ranged request is answered from the chunks that cover it — a 206 cut from
//! what is already at the edge. Replies carry a validator and are immutable,
//! so a browser keeps the ranges it has read and asks for none of them
//! twice either.
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
