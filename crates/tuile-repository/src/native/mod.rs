// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The adapters a native process reaches a bucket with.

mod cached;
mod store;
mod tiles;

pub use cached::{Cached, CHUNK};
