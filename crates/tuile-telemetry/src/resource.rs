// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a process says about itself, and the allow-lists that keep it to
//! that.
//!
//! Two lists, checked where the data leaves, not where it is made:
//!
//! - [`ALLOWED`], the resource attributes: which program, which build, which
//!   kind of machine, which GPU, and a random identifier for this launch.
//!   Anything else — whatever a configuration adds — is dropped with a line
//!   naming the key. No user, no host name, no address, no place.
//! - [`ALLOWED_LABELS`], the metric labels: the closed sets the engine labels
//!   its series with (a class of memory, a tier, a level). A series carrying
//!   any other label is not sent at all, so a label added one day with a
//!   tile's address or a layer's URL in it is stopped here rather than
//!   shipped.
//!
//! What is never sent, by construction: the device's location, a tile's
//! identity, a source's address, anything a person typed. The camera's pose
//! is sent as gauges of its own (`crate::camera`), never as a label or an
//! attribute.

use std::collections::BTreeMap;

/// The resource attributes a process may send.
pub const ALLOWED: [&str; 16] = [
    "service.name",
    "service.version",
    // A random identifier drawn at each launch: tells two sessions' counters
    // apart, says nothing about who ran them.
    "service.instance.id",
    // The build: its commit, and its `git describe` (the tag and the way
    // from it).
    "vcs.ref.head.revision",
    "vcs.ref.head.name",
    "deployment.environment",
    "os.type",
    "host.arch",
    // `desktop`, `handheld`, `browser`: the profile a session runs under.
    "device.class",
    // The adapter's name as the driver reports it (`Apple M2 Pro`) and the
    // graphics API (`metal`, `vulkan`, `webgpu`, `webgl`).
    "gpu.adapter",
    "gpu.backend",
    // Which part of a page sends: `page` (the frames) or `worker` (the
    // geometry server); `native` for a process that is both.
    "tuile.realm",
    "telemetry.sdk.name",
    "telemetry.sdk.language",
    "telemetry.sdk.version",
    // The embedding host's own name for itself (a game, an application),
    // distinct from the service the numbers are filed under.
    "tuile.host",
];

/// The metric label keys a series may carry.
pub const ALLOWED_LABELS: [&str; 18] = [
    // tuile_core::metrics: the per-level and per-eye series.
    "series",
    "level",
    "eye",
    // The tile reads by place (`tuile_tiles_fetch_*`).
    "source",
    "kind",
    "outcome",
    "tier",
    // The graphics budget's closed sets (`tuile_budget::observe::labels`).
    "class",
    "state",
    "role",
    "reason",
    "gap",
    "invariant",
    "phase",
    // The budget's specification in force (`tuile_budget::SpecKind`'s
    // labels: interactive, known_path, film, fpv, fpv_film, custom), with
    // `source` = context | override (`tuile_budget_spec_info`).
    "spec",
    "step",
    "quantile",
    // Histograms' bucket bound, where a recorder writes one as a label.
    "le",
];

/// Whether a resource attribute may be sent.
pub fn allowed(key: &str) -> bool {
    ALLOWED.contains(&key)
}

/// Whether a metric label may be sent.
pub fn allowed_label(key: &str) -> bool {
    ALLOWED_LABELS.contains(&key)
}

/// The longest attribute value kept: an adapter's name is a few dozen
/// characters, and nothing legitimate is longer.
const LONGEST_VALUE: usize = 128;

/// The attributes every series of this process carries. Only keys of
/// [`ALLOWED`] get in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resource(BTreeMap<String, String>);

impl Resource {
    /// Sets `key` to `value` if the key is allowed; says so and drops it
    /// otherwise — naming the key, never the value. Returns whether it was
    /// kept. Empty values are not kept.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        let key = key.trim();
        if !allowed(key) {
            tracing::warn!(
                "telemetry: resource attribute {key:?} is not on the allow-list; not sent"
            );
            return false;
        }
        let value: String = value
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(LONGEST_VALUE)
            .collect();
        if value.is_empty() {
            return false;
        }
        self.0.insert(key.to_owned(), value);
        true
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// A random identifier for this launch: 128 bits, hex.
///
/// Drawn from the standard library's per-process random hash keys on a
/// native target and from `Math.random` in a browser: an identifier only
/// has to differ between launches, it guards nothing.
pub fn session_id() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::hash::{BuildHasher, Hasher};
        let half = |salt: u64| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(salt);
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default(),
            );
            h.write_u32(std::process::id());
            h.finish()
        };
        format!("{:016x}{:016x}", half(1), half(2))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let half = || (js_sys::Math::random() * 2f64.powi(53)) as u64;
        format!("{:016x}{:016x}", half(), half())
    }
}

/// The operating system, under OpenTelemetry's `os.type` values.
pub fn os_type() -> &'static str {
    match std::env::consts::OS {
        "macos" | "ios" => "darwin",
        "windows" => "windows",
        "linux" | "android" => "linux",
        "" => "unknown",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The privacy guard: nothing outside the allow-list is kept, whatever
    /// a configuration adds.
    #[test]
    fn only_allowed_attributes_are_kept() {
        let mut r = Resource::default();
        for (k, v) in [
            ("service.name", "viewer"),
            ("user.id", "someone"),
            ("host.name", "laurents-mac"),
            ("geo.location.lat", "45.8"),
            ("enduser.id", "x"),
            ("camera.position", "1,2,3"),
            ("gpu.adapter", "Apple M2 Pro"),
            ("vcs.ref.head.revision", "abc123def456"),
            ("vcs.ref.head.name", "v0.17.1-12-gabc123def456"),
        ] {
            r.set(k, v);
        }
        let keys: Vec<&str> = r.iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            [
                "gpu.adapter",
                "service.name",
                "vcs.ref.head.name",
                "vcs.ref.head.revision"
            ]
        );
        assert!(r.iter().all(|(k, _)| allowed(k)));
    }

    #[test]
    fn values_are_bounded_and_clean() {
        let mut r = Resource::default();
        assert!(r.set("gpu.adapter", &"x".repeat(1000)));
        assert_eq!(r.get("gpu.adapter").map(str::len), Some(LONGEST_VALUE));
        assert!(r.set("os.type", "dar\nwin"));
        assert_eq!(r.get("os.type"), Some("darwin"));
        assert!(!r.set("service.version", "  "));
    }

    #[test]
    fn session_ids_differ() {
        let (a, b) = (session_id(), session_id());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
