// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The adapters a native process reaches a bucket with.

mod disk;
mod http;
mod store;
mod tiles;

pub use disk::DiskChunks;
pub use http::{HttpGet, Kept};
