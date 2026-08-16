// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Publishes the engine's counters to a Prometheus endpoint.
//!
//! Every number used to reach a human by being printed into one wide log line
//! per second, mixed with five unrelated subjects. That line is unreadable, and
//! worse it is the wrong shape: a cumulative counter like `uploads` or
//! `evictions` says almost nothing as an absolute value and everything as a
//! *rate*, which a log forces the reader to compute by subtracting two lines by
//! eye. So the numbers come here, where anything that speaks Prometheus takes
//! their derivative, and `stdout` keeps only what is genuinely an **event**:
//! ground drawn black, a server that stopped, imagery that never arrived.
//!
//! ```text
//! curl -s localhost:9464/metrics | grep tuile_store
//! ```
//!
//! # Why a bridge at all
//!
//! [`tuile_core::metrics`] is plain atomics: no allocation, no locks, nothing
//! behind a feature, and it compiles to `wasm32-unknown-unknown`, where a
//! registry and an HTTP listener cannot follow. The core may take no backend
//! dependency, so recording stays there and publishing happens here — a copy of
//! a few dozen integers, once a second.
//!
//! `metrics` + `metrics-exporter-prometheus` own the exposition format and the
//! HTTP listener. Both were briefly written by hand in this file, which was a
//! waste and worse than what the crates do.

use std::net::SocketAddr;
use std::time::Duration;

use metrics_exporter_prometheus::PrometheusBuilder;
use tuile_core::metrics::{ByLevel, Metrics, LEVELS};

/// Where the exporter listens unless `TUILE_METRICS_ADDR` says otherwise.
///
/// Loopback, deliberately: these numbers describe a running session in detail
/// and nothing about them wants to be on a network by accident. 9464 is the
/// port OpenTelemetry's Prometheus exporter uses, so a default scrape config
/// finds it without being told.
const DEFAULT_ADDR: &str = "127.0.0.1:9464";

/// How often the atomics are copied into the recorder.
///
/// The endpoint answers from whatever was last published, so this is the real
/// resolution of every series — finer than a scrape interval, and coarse enough
/// that the copy never shows up in a frame.
const PUBLISH_EVERY: Duration = Duration::from_millis(500);

/// Starts the exporter and the publishing loop. `TUILE_METRICS_ADDR=off`
/// disables both; anything else is parsed as an address.
///
/// Failure to bind is reported and ignored. A port already taken — a second
/// viewer, most likely — is not a reason to refuse to draw a globe.
pub fn serve(handle: &tokio::runtime::Handle) {
    let setting = std::env::var("TUILE_METRICS_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    if setting.trim().eq_ignore_ascii_case("off") {
        return;
    }
    let addr: SocketAddr = match setting.parse() {
        Ok(addr) => addr,
        Err(e) => {
            tracing::warn!("TUILE_METRICS_ADDR={setting:?} is not an address ({e}); no metrics");
            return;
        }
    };

    let _guard = handle.enter();
    if let Err(e) = PrometheusBuilder::new().with_http_listener(addr).install() {
        tracing::warn!("no metrics endpoint on {addr}: {e}");
        return;
    }
    describe();
    tracing::info!("metrics on http://{addr}/metrics");

    handle.spawn(async move {
        let mut tick = tokio::time::interval(PUBLISH_EVERY);
        loop {
            tick.tick().await;
            publish(tuile_core::metrics::metrics());
        }
    });
}

/// The help text, stated once. A series nobody can interpret is a series nobody
/// reads.
fn describe() {
    use metrics::{describe_counter, describe_gauge};
    describe_counter!("tuile_traversals", "Traversal passes run.");
    describe_counter!("tuile_loads_started", "Tile loads begun.");
    describe_counter!("tuile_loads_completed", "Tile loads that produced content.");
    describe_counter!("tuile_loads_failed", "Tile loads that errored.");
    describe_counter!(
        "tuile_loads_cancelled",
        "Tile loads killed before finishing."
    );
    describe_counter!("tuile_mesh_fetches", "Terrain fetches begun.");
    describe_counter!("tuile_texture_fetches", "Imagery fetches begun.");
    describe_counter!(
        "tuile_tiles_upsampled",
        "Tiles built from an ancestor's surface."
    );
    describe_counter!("tuile_tiles_evicted", "Tiles the budget reclaimed.");
    describe_counter!(
        "tuile_tiles_without_imagery",
        "Tiles drawn with no imagery covering them."
    );
    describe_counter!(
        "tuile_imagery_absent",
        "Imagery the provider does not serve at all."
    );
    describe_counter!(
        "tuile_terrain_absent",
        "Terrain the source does not serve at all."
    );
    describe_counter!(
        "tuile_black_textures",
        "Imagery tiles that decoded to opaque black."
    );
    describe_counter!(
        "tuile_store_hits",
        "Content served by the store, without touching the network."
    );
    describe_counter!(
        "tuile_store_misses",
        "Content the store lacked, so the origin was asked."
    );
    describe_counter!("tuile_store_bytes_served", "Bytes served by the store.");
    describe_counter!(
        "tuile_store_bytes_fetched",
        "Bytes fetched from the origin."
    );
    describe_counter!("tuile_frames", "Frames presented.");
    describe_counter!("tuile_uploads", "Tiles uploaded to the GPU.");

    describe_gauge!("tuile_tiles_visited", "Nodes the last pass walked.");
    describe_gauge!("tuile_tiles_culled", "Nodes no view could see.");
    describe_gauge!("tuile_tiles_selected", "Tiles the last pass chose to draw.");
    describe_gauge!("tuile_gaps", "Ground drawn by nothing. Zero on a globe.");
    describe_gauge!("tuile_loads_in_flight", "Loads outstanding.");
    describe_gauge!("tuile_meshes_in_flight", "Terrain fetches outstanding.");
    describe_gauge!("tuile_textures_in_flight", "Imagery fetches outstanding.");
    describe_gauge!("tuile_resident_bytes", "Decoded geometry held.");
    describe_gauge!(
        "tuile_imagery_bytes",
        "Imagery held, counted once per texture."
    );
    describe_gauge!("tuile_imagery_textures", "Distinct imagery textures held.");
    describe_gauge!(
        "tuile_pending_uploads",
        "Decoded tiles waiting for a frame's upload budget."
    );
    describe_gauge!("tuile_prepared_tiles", "Tiles resident on the GPU.");
    describe_gauge!(
        "tuile_tiles_filled",
        "Selected tiles drawn by a stand-in surface rather than their own geometry."
    );
    describe_gauge!(
        "tuile_by_level",
        "Per-quadtree-level series; the `series` label says which."
    );
}

/// Copies every atomic into the recorder.
///
/// Counters are published with `absolute`, which is exactly right here: both
/// sides are monotonic, so there is no delta to track and a process restart
/// resets them together — which is what a counter reset is supposed to look
/// like to a scraper.
fn publish(m: &Metrics) {
    use metrics::{counter, gauge};

    counter!("tuile_traversals").absolute(m.traversals.get());
    counter!("tuile_loads_started").absolute(m.loads_started.get());
    counter!("tuile_loads_completed").absolute(m.loads_completed.get());
    counter!("tuile_loads_failed").absolute(m.loads_failed.get());
    counter!("tuile_loads_cancelled").absolute(m.loads_cancelled.get());
    counter!("tuile_mesh_fetches").absolute(m.mesh_fetches.get());
    counter!("tuile_texture_fetches").absolute(m.texture_fetches.get());
    counter!("tuile_tiles_upsampled").absolute(m.tiles_upsampled.get());
    counter!("tuile_tiles_evicted").absolute(m.tiles_evicted.get());
    counter!("tuile_tiles_without_imagery").absolute(m.tiles_without_imagery.get());
    counter!("tuile_imagery_absent").absolute(m.imagery_absent.get());
    counter!("tuile_terrain_absent").absolute(m.terrain_absent.get());
    counter!("tuile_black_textures").absolute(m.black_textures.get());
    counter!("tuile_store_hits").absolute(m.store_hits.get());
    counter!("tuile_store_misses").absolute(m.store_misses.get());
    counter!("tuile_store_bytes_served").absolute(m.store_bytes_served.get());
    counter!("tuile_store_bytes_fetched").absolute(m.store_bytes_fetched.get());
    counter!("tuile_frames").absolute(m.frames.get());
    counter!("tuile_uploads").absolute(m.uploads.get());

    gauge!("tuile_tiles_visited").set(m.tiles_visited.get() as f64);
    gauge!("tuile_tiles_culled").set(m.tiles_culled.get() as f64);
    gauge!("tuile_tiles_selected").set(m.tiles_selected.get() as f64);
    gauge!("tuile_gaps").set(m.gaps.get() as f64);
    gauge!("tuile_loads_in_flight").set(m.loads_in_flight.get() as f64);
    gauge!("tuile_meshes_in_flight").set(m.meshes_in_flight.get() as f64);
    gauge!("tuile_textures_in_flight").set(m.textures_in_flight.get() as f64);
    gauge!("tuile_resident_bytes").set(m.resident_bytes.get() as f64);
    gauge!("tuile_imagery_bytes").set(m.imagery_bytes.get() as f64);
    gauge!("tuile_imagery_textures").set(m.imagery_textures.get() as f64);
    gauge!("tuile_pending_uploads").set(m.pending_uploads.get() as f64);
    gauge!("tuile_prepared_tiles").set(m.prepared_tiles.get() as f64);
    gauge!("tuile_tiles_filled").set(m.tiles_filled.get() as f64);

    // One series name, two labels. The alternative — eight series names each
    // carrying a level label — makes `sum by (level)` across them impossible
    // without naming all eight.
    let series: [(&str, &ByLevel); 8] = [
        ("meshes", &m.meshes_by_level),
        ("mesh_bytes", &m.mesh_bytes_by_level),
        ("textures", &m.textures_by_level),
        ("texture_bytes", &m.texture_bytes_by_level),
        ("queued", &m.queued_by_level),
        ("loads", &m.loads_by_level),
        ("upsampled", &m.upsampled_by_level),
        ("selected", &m.selected_by_level),
    ];
    for (name, values) in series {
        for level in 0..LEVELS as u32 {
            gauge!("tuile_by_level", "series" => name, "level" => level.to_string())
                .set(values.get(level) as f64);
        }
    }
}
