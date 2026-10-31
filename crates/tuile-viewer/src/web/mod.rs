// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The browser shell: the shared application on a page's canvas, its geometry
//! server in a Web Worker.
//!
//! ```text
//!   page (this module, `page`)                 worker (`worker`)
//!   App ── ContentPump ── tuile-wgpu           GeometryServer, planetary loader
//!    │        ▲                                   ▲
//!    │   PageStream ◀── ServerMessage bytes ──── serve()
//!    └── proxies ── detail, slots, layer, pause ──▶ the loader's own handles
//! ```
//!
//! The page and the worker load the same wasm module and call different
//! exports: [`page`] creates the viewer, [`worker`] runs the server, and
//! [`decode`] runs the decode workers the server's worker starts. The
//! renderer, the frame step, the gestures, the indicator and the lifecycle are
//! the desktop's own; what is here is only what a browser does differently —
//! where the session comes from ([`session`]), what the host page is told
//! ([`system`]), and how the page drives the event loop it shares with
//! whatever else is on it.
//!
//! There is no tile store yet: the worker keeps its residency in memory, and
//! the browser store comes with the graphics budget.

pub(crate) mod options;

#[cfg(target_arch = "wasm32")]
mod control;
#[cfg(target_arch = "wasm32")]
pub mod decode;
#[cfg(target_arch = "wasm32")]
mod log;
#[cfg(target_arch = "wasm32")]
pub(crate) mod menu;
#[cfg(target_arch = "wasm32")]
pub mod page;
// The decode pool's bookkeeping, on every target so its tests run on the host.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) mod imagery;
#[cfg(target_arch = "wasm32")]
mod screen;
#[cfg(target_arch = "wasm32")]
mod session;
#[cfg(target_arch = "wasm32")]
mod sources;
#[cfg(target_arch = "wasm32")]
pub(crate) mod system;
#[cfg(target_arch = "wasm32")]
pub mod worker;

#[cfg(target_arch = "wasm32")]
pub(crate) use session::{Budget, Gate, Started};
#[cfg(target_arch = "wasm32")]
pub(crate) use system::{keep_the_view, keep_tilesets_on};
