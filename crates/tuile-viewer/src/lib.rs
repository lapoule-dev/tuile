// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod app;
mod backdrop;
#[cfg(not(target_arch = "wasm32"))]
mod config_file;
mod desktop;
mod device;
pub mod embed;
pub mod frame_host;
mod gate;
mod globe;
mod host;
#[cfg(not(target_arch = "wasm32"))]
mod import;
mod journal;
mod launch;
mod location;
mod menu;
mod open;
mod profile;
mod recording;
#[cfg(not(target_arch = "wasm32"))]
mod secrets;
mod session;
mod signals;
// Only a script asks; without the desktop glue the answers have no reader.
#[cfg_attr(
    not(all(target_os = "macos", feature = "application")),
    allow(dead_code)
)]
mod snapshot;
mod start;
mod steer;
mod surface;

#[cfg(feature = "control")]
pub mod control;

// The control API is native: a page is driven through its own debug
// namespace, and has no thread a socket could be served from.
#[cfg(all(feature = "control", target_arch = "wasm32"))]
compile_error!("the `control` feature is native; a browser's viewer is driven from its page");

// Never in an application that ships to a phone: a socket that moves the view
// is a debug build's, and an iOS release build that has one is refused here
// rather than found later.
#[cfg(all(feature = "control", target_os = "ios", not(debug_assertions)))]
compile_error!("the `control` feature is for tests and debug builds; an iOS release has none");

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
use native as shell;
#[cfg(not(target_arch = "wasm32"))]
pub use native::main;

/// The Android entry: a host's `android_main` hands its activity and itself
/// to [`android::main`]. See `android/README.md`.
#[cfg(target_os = "android")]
pub mod android {
    pub use crate::native::android::main;
    pub use winit::platform::android::activity::AndroidApp;
}

// The browser's options are read and checked on the host too, by its tests.
#[cfg(any(target_arch = "wasm32", test))]
mod web;
#[cfg(target_arch = "wasm32")]
use web as shell;

/// The door a Rust host comes in by on the web: the viewer on a canvas, with
/// an application owning each frame ([`frame_host`]).
///
/// The function rather than the module: what a host is promised is this call,
/// and the shell around it is the viewer's own business.
#[cfg(target_arch = "wasm32")]
pub use web::page::start_with_host;

/// The worker entries a host's own module must re-export.
///
/// A host that starts the viewer with [`start_with_host`] *is* the wasm
/// module the page loads, so the package's `worker.js` and
/// `decode-worker.js` import **its** name, not the viewer's — and they call
/// these. Re-exporting them is how a host's module carries them; they are
/// `#[wasm_bindgen]` already, so naming them here is enough for the bindings
/// to appear in the host's own JS.
#[cfg(target_arch = "wasm32")]
pub use web::decode::run_decode_worker;
#[cfg(target_arch = "wasm32")]
pub use web::worker::run_worker;

pub use embed::{
    credential, renamed_by_environment, Identity, ImageryChoice, MissingCredential, Sources,
    TerrainChoice, TilesetChoice, ViewerHost,
};
