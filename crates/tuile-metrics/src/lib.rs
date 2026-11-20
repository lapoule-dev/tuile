// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

#[cfg(not(target_arch = "wasm32"))]
use std::net::SocketAddr;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tuile_core::metrics::{ByLevel, Metrics, Tally, Timer, EYES, LEVELS};

/// Where the exporter listens unless `TUILE_METRICS_ADDR` says otherwise.
///
/// Loopback, deliberately: these numbers describe a running session in detail
/// and nothing about them wants to be on a network by accident. 9464 is the
/// port OpenTelemetry's Prometheus exporter uses, so a default scrape config
/// finds it without being told.
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_ADDR: &str = "127.0.0.1:9464";

/// How often the atomics are copied into the recorder.
///
/// The endpoint answers from whatever was last published, so this is the real
/// resolution of every series — finer than a scrape interval, and coarse enough
/// that the copy never shows up in a frame.
#[cfg(not(target_arch = "wasm32"))]
const PUBLISH_EVERY: Duration = Duration::from_millis(500);

/// The buckets of `tuile_frame_time_seconds` (the work of one frame, from its
/// start to its present) and `tuile_frame_interval_seconds` (top of a frame
/// to the top of the next): two milliseconds to a second, dense around the
/// refresh intervals a display has (8.3, 11.1, 16.7, 33.3 ms) so that a p99
/// reads as "missed one vsync" or "missed three".
pub const FRAME_SECONDS_BUCKETS: [f64; 18] = [
    0.002, 0.004, 0.006, 0.0083, 0.0111, 0.0139, 0.0167, 0.020, 0.025, 0.0333, 0.040, 0.050,
    0.0667, 0.100, 0.150, 0.250, 0.500, 1.000,
];

/// The buckets of `tuile_frame_uploads`: tiles uploaded to the GPU by one
/// frame. Its upload budget is a few dozen; past it is a stall.
pub const FRAME_UPLOADS_BUCKETS: [f64; 9] = [0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0];

/// The buckets a histogram named `name` is recorded with, by every recorder
/// that buckets: the Prometheus endpoint here, the OTLP push of
/// `tuile-telemetry`. `None`: a histogram this crate does not know, which
/// such a recorder gives its default.
pub fn buckets(name: &str) -> Option<&'static [f64]> {
    match name {
        "tuile_tiles_fetch_seconds" => Some(&tuile_core::metrics::TILE_FETCH_SECONDS_BUCKETS),
        "tuile_frame_time_seconds" | "tuile_frame_interval_seconds" => Some(&FRAME_SECONDS_BUCKETS),
        "tuile_frame_uploads" => Some(&FRAME_UPLOADS_BUCKETS),
        _ => None,
    }
}

/// The histograms [`buckets`] knows, by name.
pub const HISTOGRAMS: [&str; 4] = [
    "tuile_tiles_fetch_seconds",
    "tuile_frame_time_seconds",
    "tuile_frame_interval_seconds",
    "tuile_frame_uploads",
];

/// Starts the exporter and the publishing loop. `TUILE_METRICS_ADDR=off`
/// disables both; anything else is parsed as an address.
///
/// Failure to bind is reported and ignored. A port already taken — a second
/// viewer, most likely — is not a reason to refuse to draw a globe.
#[cfg(not(target_arch = "wasm32"))]
pub fn serve(handle: &tokio::runtime::Handle) {
    serve_with(handle, true, None);
}

/// [`serve`], with a second recorder `also` handed every series beside the
/// endpoint — the OTLP push of `tuile-telemetry` — and the endpoint itself
/// only when `endpoint` (a profile that serves one: not a phone's). With
/// neither, nothing is installed and nothing is published.
///
/// A process holds one global recorder; this is the one place it is
/// installed, so a series reaches the endpoint and the push alike.
#[cfg(not(target_arch = "wasm32"))]
pub fn serve_with(
    handle: &tokio::runtime::Handle,
    endpoint: bool,
    also: Option<Box<dyn metrics::Recorder + Send + Sync>>,
) {
    let addr = if endpoint { listen_address() } else { None };
    let _guard = handle.enter();
    let prometheus = match addr {
        Some(addr) => match prometheus(addr) {
            Ok(built) => Some((addr, built)),
            Err(e) => {
                tracing::warn!("no metrics endpoint on {addr}: {e}");
                None
            }
        },
        None => None,
    };
    let installed = match (prometheus, also) {
        (None, None) => return,
        (Some((addr, (recorder, exporter))), None) => {
            handle.spawn(exporter);
            metrics::set_global_recorder(recorder)
                .map(|()| Some(addr))
                .map_err(|e| e.to_string())
        }
        (None, Some(also)) => metrics::set_global_recorder(also)
            .map(|()| None)
            .map_err(|e| e.to_string()),
        (Some((addr, (recorder, exporter))), Some(also)) => {
            handle.spawn(exporter);
            let both = metrics_util::layers::FanoutBuilder::default()
                .add_recorder(recorder)
                .add_recorder(also)
                .build();
            metrics::set_global_recorder(both)
                .map(|()| Some(addr))
                .map_err(|e| e.to_string())
        }
    };
    match installed {
        Ok(Some(addr)) => tracing::info!("metrics on http://{addr}/metrics"),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("the process already has a metrics recorder ({e}); none installed");
            return;
        }
    }
    publish_every(handle, PUBLISH_EVERY);
}

/// The address `TUILE_METRICS_ADDR` names, [`DEFAULT_ADDR`] without it;
/// `None` for `off`, or for what is not an address (said).
#[cfg(not(target_arch = "wasm32"))]
fn listen_address() -> Option<SocketAddr> {
    let setting = std::env::var("TUILE_METRICS_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    if setting.trim().eq_ignore_ascii_case("off") {
        return None;
    }
    match setting.parse() {
        Ok(addr) => Some(addr),
        Err(e) => {
            tracing::warn!("TUILE_METRICS_ADDR={setting:?} is not an address ({e}); no metrics");
            None
        }
    }
}

/// The endpoint's recorder and the future that serves it, with the buckets
/// of every histogram [`buckets`] knows. Must run inside the runtime.
#[cfg(not(target_arch = "wasm32"))]
fn prometheus(
    addr: SocketAddr,
) -> Result<
    (
        metrics_exporter_prometheus::PrometheusRecorder,
        metrics_exporter_prometheus::ExporterFuture,
    ),
    metrics_exporter_prometheus::BuildError,
> {
    let mut builder = PrometheusBuilder::new();
    for name in HISTOGRAMS {
        if let Some(bounds) = buckets(name) {
            builder = builder.set_buckets_for_metric(Matcher::Full(name.to_owned()), bounds)?;
        }
    }
    builder.with_http_listener(addr).build()
}

/// Publishes the engine's numbers into whatever recorder the process already
/// has, every `period`, on `handle`.
///
/// For a host that owns its own endpoint. A process may install one global
/// recorder, so [`serve`] — which installs one — fails in a host that already
/// exports metrics of its own; this only describes the series and starts the
/// copy, and the numbers come out wherever the host's recorder sends them.
///
/// Call it after the recorder is installed: a series described or written
/// before there is one goes nowhere. Called twice it starts two loops, which
/// write the same values and cost a second copy, nothing more.
#[cfg(not(target_arch = "wasm32"))]
pub fn publish_every(handle: &tokio::runtime::Handle, period: Duration) {
    describe();
    handle.spawn(async move {
        let mut tick = tokio::time::interval(period);
        loop {
            tick.tick().await;
            publish(tuile_core::metrics::metrics());
        }
    });
}

/// The graphics budget's signals as the session last published them, under
/// `tuile_budget_*` (spec §16.3, §16.6): names and closed label sets from
/// [`tuile_core::budget::BudgetSignals::series`], counters for the totals,
/// gauges for the rest. Nothing until a session has published.
fn publish_budget() {
    let Some(signals) = tuile_core::metrics::budget() else {
        return;
    };
    for (name, labels, value) in signals.series() {
        let labels: Vec<metrics::Label> = labels
            .into_iter()
            .map(|(k, v)| metrics::Label::new(k, v))
            .collect();
        let full = format!("tuile_budget_{name}");
        if name.ends_with("_total") || name.ends_with("_count") {
            metrics::counter!(full, labels).absolute(value.max(0.0) as u64);
        } else {
            metrics::gauge!(full, labels).set(value);
        }
    }
}

/// The help text, stated once, into the installed recorder. A series nobody
/// can interpret is a series nobody reads.
pub fn describe() {
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
    describe_counter!(
        "tuile_drapes_withheld",
        "Drapes the consumer already held, so their layers were never requested."
    );
    describe_counter!("tuile_frames", "Frames presented.");
    describe_counter!("tuile_uploads", "Tiles uploaded to the GPU.");
    describe_counter!("tuile_stitch_plans", "Stitching plans worked out.");
    describe_counter!("tuile_restitched", "Tiles given new stitching strips.");
    describe_counter!(
        "tuile_stitch_overflows",
        "Tiles left unstitched: their plan had more knots than the consumer holds."
    );
    describe_counter!(
        "tuile_height_queries",
        "Height queries against a frame's ground."
    );
    describe_counter!(
        "tuile_ground_tiles",
        "Tiles cut into cells for a ground's heights: once a tile, again when its stitching moves it."
    );

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
        "tuile_ground_reads_pending",
        "Grounds built and not read yet, every bake of the process together."
    );
    describe_gauge!(
        "tuile_ground_reads_pending_bytes",
        "Their bytes, each ground counted whole: a ceiling, grounds share tiles."
    );
    describe_gauge!(
        "tuile_settled_tiles",
        "Distinct terrain tiles held by the selections settled so far."
    );
    describe_gauge!(
        "tuile_settled_bytes",
        "Their bytes, each tile once, with the selections' own lists."
    );
    describe_gauge!(
        "tuile_frame_exact",
        "Selected tiles the last frame drew with their own surface."
    );
    describe_gauge!(
        "tuile_frame_coarser",
        "Selected tiles the last frame drew with a coarser ancestor standing in."
    );
    describe_gauge!(
        "tuile_frame_lost",
        "Selected tiles the last frame drew with nothing at all: holes."
    );
    describe_gauge!(
        "tuile_frame_digest",
        "A digest of the tiles the last frame drew, its low 48 bits: constant while the picture is."
    );
    describe_gauge!(
        "tuile_eye_pixels",
        "Per eye of the last frame read back: ground pixels (series=ground), and those left the clear colour (series=black)."
    );
    describe_gauge!(
        "tuile_priming_total",
        "Coarse tiles the session primes before drawing."
    );
    describe_gauge!(
        "tuile_priming_pending",
        "Coarse tiles not answered yet, queued or in flight."
    );
    describe_gauge!(
        "tuile_priming_unavailable",
        "Coarse tiles given up on: none at the source, failed, or not geometry."
    );
    describe_gauge!(
        "tuile_priming_done",
        "One once the coarse pyramid has been asked for in full."
    );
    let m = tuile_core::metrics::metrics();
    for (name, help, _) in timers(m) {
        describe_counter!(
            format!("tuile_{name}_seconds_count"),
            format!("{help} Samples.")
        );
        describe_gauge!(
            format!("tuile_{name}_seconds_sum"),
            format!("{help} Total seconds; only goes up, take its rate.")
        );
        describe_gauge!(
            format!("tuile_{name}_seconds_peak"),
            format!("{help} Longest single sample, seconds.")
        );
    }
    for (name, help, _) in tallies(m) {
        describe_counter!(
            format!("tuile_{name}_count"),
            format!("{help} Occurrences.")
        );
        describe_counter!(
            format!("tuile_{name}_sum"),
            format!("{help} Total carried.")
        );
        describe_gauge!(
            format!("tuile_{name}_peak"),
            format!("{help} Most carried by one.")
        );
    }
    describe_gauge!(
        "tuile_by_level",
        "Per-quadtree-level series; the `series` label says which."
    );
    metrics::describe_histogram!(
        "tuile_frame_time_seconds",
        "The work of one frame, from its start to its present."
    );
    metrics::describe_histogram!(
        "tuile_frame_interval_seconds",
        "From the top of a frame to the top of the next: idle time included."
    );
    metrics::describe_histogram!(
        "tuile_frame_uploads",
        "Tiles one frame uploaded to the GPU."
    );
}

/// Every [`Timer`], by the name it is published under and what it measures.
///
/// A timer is three series: `_seconds_count` (a counter), `_seconds_sum` and
/// `_seconds_peak` (gauges). The sum is a gauge only because the recorder's
/// counters are integers; it never goes down, and `rate(sum) / rate(count)` is
/// the mean over any window.
fn timers(m: &Metrics) -> [(&'static str, &'static str, &Timer); 17] {
    [
        ("traversal", "The traversal walk.", &m.traversal_seconds),
        (
            "select",
            "Building and sending the selection a pass ends with.",
            &m.select_seconds,
        ),
        (
            "arrival_wait",
            "From a load resolving to the loop handling it.",
            &m.arrival_wait_seconds,
        ),
        (
            "load_wake_wait",
            "From a load being woken to the loop polling it, per wake.",
            &m.load_wake_wait_seconds,
        ),
        (
            "load",
            "A tile load, from start to content.",
            &m.load_seconds,
        ),
        ("mesh_fetch", "A terrain fetch.", &m.mesh_fetch_seconds),
        (
            "texture_fetch",
            "An imagery fetch.",
            &m.texture_fetch_seconds,
        ),
        ("frame", "A frame, top to top.", &m.frame_seconds),
        (
            "rebase",
            "Rewriting model matrices on an origin move.",
            &m.rebase_seconds,
        ),
        ("present", "Waiting for a drawable.", &m.present_seconds),
        ("upload", "A tile upload to the GPU.", &m.upload_seconds),
        ("stitch", "Working out a stitching plan.", &m.stitch_seconds),
        (
            "bake_frame",
            "A bake frame resolved and written, its pose excluded.",
            &m.bake_frame_seconds,
        ),
        ("pose", "Deciding a bake frame's pose.", &m.pose_seconds),
        ("finish", "Finishing one tile, any mode.", &m.finish_seconds),
        (
            "ground",
            "Building the ground a frame's selection draws.",
            &m.ground_seconds,
        ),
        (
            "height_query",
            "A height query against a frame's ground.",
            &m.height_query_seconds,
        ),
    ]
}

/// Every [`Tally`], likewise: `_count`, `_sum` (counters) and `_peak` (a
/// gauge).
fn tallies(m: &Metrics) -> [(&'static str, &'static str, &Tally); 1] {
    [(
        "arrivals_per_pass",
        "Load completions handled by each pass an arrival provoked.",
        &m.arrivals_per_pass,
    )]
}

/// Copies every atomic into the installed recorder: what [`publish_every`]
/// does each period, and what a host without a runtime (a page) calls on its
/// own timer.
///
/// Counters are published with `absolute`, which is exactly right here: both
/// sides are monotonic, so there is no delta to track and a process restart
/// resets them together — which is what a counter reset is supposed to look
/// like to a scraper.
pub fn publish(m: &Metrics) {
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
    counter!("tuile_stitch_plans").absolute(m.stitch_plans.get());
    counter!("tuile_restitched").absolute(m.restitched.get());
    counter!("tuile_stitch_overflows").absolute(m.stitch_overflows.get());
    counter!("tuile_drapes_withheld").absolute(m.drapes_withheld.get());
    counter!("tuile_height_queries").absolute(m.height_queries.get());
    counter!("tuile_ground_tiles").absolute(m.ground_tiles.get());

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
    gauge!("tuile_ground_reads_pending").set(m.ground_reads_pending.get() as f64);
    gauge!("tuile_ground_reads_pending_bytes").set(m.ground_reads_pending_bytes.get() as f64);
    gauge!("tuile_settled_tiles").set(m.settled_tiles.get() as f64);
    gauge!("tuile_settled_bytes").set(m.settled_bytes.get() as f64);
    gauge!("tuile_frame_exact").set(m.frame_exact.get() as f64);
    gauge!("tuile_frame_coarser").set(m.frame_coarser.get() as f64);
    gauge!("tuile_frame_lost").set(m.frame_lost.get() as f64);
    // Forty-eight bits, which a float carries exactly: a fingerprint to see
    // the picture change by, not a number to do arithmetic on.
    gauge!("tuile_frame_digest").set((m.frame_digest.get() & ((1 << 48) - 1)) as f64);
    for eye in 0..EYES {
        let label = eye.to_string();
        gauge!("tuile_eye_pixels", "series" => "ground", "eye" => label.clone())
            .set(m.eye_ground_pixels.get(eye) as f64);
        gauge!("tuile_eye_pixels", "series" => "black", "eye" => label)
            .set(m.eye_black_pixels.get(eye) as f64);
    }
    publish_budget();
    gauge!("tuile_priming_total").set(m.priming_total.get() as f64);
    gauge!("tuile_priming_pending").set(m.priming_pending.get() as f64);
    gauge!("tuile_priming_unavailable").set(m.priming_unavailable.get() as f64);
    gauge!("tuile_priming_done").set(m.priming_done.get() as f64);

    // The peak is read in place, not taken: a scrape must not reset what a
    // status line in the same process reads as "the worst this window".
    for (name, _, t) in timers(m) {
        counter!(format!("tuile_{name}_seconds_count")).absolute(t.count());
        gauge!(format!("tuile_{name}_seconds_sum")).set(t.total_seconds());
        gauge!(format!("tuile_{name}_seconds_peak")).set(t.peak_millis() / 1000.0);
    }
    for (name, _, t) in tallies(m) {
        counter!(format!("tuile_{name}_count")).absolute(t.count());
        counter!(format!("tuile_{name}_sum")).absolute(t.sum());
        gauge!(format!("tuile_{name}_peak")).set(t.peak() as f64);
    }

    // One series name, two labels. The alternative — eight series names each
    // carrying a level label — makes `sum by (level)` across them impossible
    // without naming all eight.
    let series: [(&str, &ByLevel); 9] = [
        ("meshes", &m.meshes_by_level),
        ("mesh_bytes", &m.mesh_bytes_by_level),
        ("textures", &m.textures_by_level),
        ("texture_bytes", &m.texture_bytes_by_level),
        ("queued", &m.queued_by_level),
        ("loads", &m.loads_by_level),
        ("upsampled", &m.upsampled_by_level),
        ("selected", &m.selected_by_level),
        ("drawn", &m.drawn_by_level),
    ];
    for (name, values) in series {
        for level in 0..LEVELS as u32 {
            gauge!("tuile_by_level", "series" => name, "level" => level.to_string())
                .set(values.get(level) as f64);
        }
    }
    publish_layers(&m.layer);
}

/// Imagery layer substitution: per layer role (`layer_role` = `source` |
/// `substitute`, a closed set), its fetches, their answers, bytes and
/// latency, the drapes drawn by level; the fallback specification's
/// decisions by gap, its back-offs, whether it is in force. See
/// `docs/explorations/imagery-layer-substitution.md` §7.
fn publish_layers(l: &tuile_core::metrics::LayerMetrics) {
    use metrics::{counter, gauge};
    use tuile_core::metrics::LayerRole;
    for role in LayerRole::ALL {
        let layer = role.label();
        let f = l.of(role);
        counter!("tuile_layer_requests_total", "layer_role" => layer).absolute(f.requests.get());
        for (outcome, c) in [
            ("hit", &f.hits),
            ("absent", &f.absents),
            ("error", &f.errors),
        ] {
            counter!("tuile_layer_responses_total", "layer_role" => layer, "outcome" => outcome)
                .absolute(c.get());
        }
        counter!("tuile_layer_bytes_total", "layer_role" => layer).absolute(f.bytes.get());
        counter!("tuile_layer_fetch_seconds_count", "layer_role" => layer)
            .absolute(f.seconds.count());
        gauge!("tuile_layer_fetch_seconds_sum", "layer_role" => layer)
            .set(f.seconds.total_seconds());
        gauge!("tuile_layer_fetch_seconds_peak", "layer_role" => layer)
            .set(f.seconds.peak_millis() / 1000.0);
        counter!("tuile_layer_ancestor_fallbacks_total", "layer_role" => layer)
            .absolute(f.ancestor_fallbacks.get());
    }
    for (name, values) in [
        ("drapes_source", &l.drawn_source),
        ("drapes_substitute", &l.drawn_substitute),
    ] {
        for level in 0..LEVELS as u32 {
            gauge!("tuile_by_level", "series" => name, "level" => level.to_string())
                .set(values.get(level) as f64);
        }
    }
    const GAP: [&str; tuile_core::metrics::GAPS] = [
        "-8", "-7", "-6", "-5", "-4", "-3", "-2", "-1", "0", "1", "2", "3", "4", "5", "6", "7", "8",
    ];
    for (reason, counts) in [
        ("to_substitute", &l.to_substitute),
        ("to_source", &l.to_source),
    ] {
        for (gap, c) in GAP.iter().zip(counts.iter()) {
            counter!("tuile_layer_switches_total", "reason" => reason, "gap" => *gap)
                .absolute(c.get());
        }
    }
    counter!("tuile_layer_backoffs_total").absolute(l.backoffs.get());
    gauge!("tuile_layer_fallback_suspended").set(l.suspended.get() as f64);
    gauge!("tuile_layer_fallback_active").set(l.fallback_active.get() as f64);
    gauge!("tuile_layer_reference_level").set(l.reference_level.get() as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop's instruments reach the endpoint, timers as three series and
    /// tallies as three more.
    #[test]
    fn timers_and_tallies_are_published() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let m = tuile_core::metrics::metrics();
        m.arrival_wait_seconds.record(Duration::from_millis(1500));
        m.arrivals_per_pass.record(3);
        metrics::with_local_recorder(&recorder, || publish(m));
        let text = handle.render();
        for series in [
            "tuile_arrival_wait_seconds_count",
            "tuile_arrival_wait_seconds_sum",
            "tuile_arrival_wait_seconds_peak",
            "tuile_select_seconds_count",
            "tuile_load_wake_wait_seconds_count",
            "tuile_traversal_seconds_sum",
            "tuile_arrivals_per_pass_count",
            "tuile_arrivals_per_pass_sum",
            "tuile_arrivals_per_pass_peak 3",
        ] {
            assert!(text.contains(series), "{series} missing from:\n{text}");
        }
        // The peak is in seconds, like the sum.
        assert!(
            text.contains("tuile_arrival_wait_seconds_peak 1.5"),
            "peak not in seconds:\n{text}"
        );
    }

    /// What a frame drew, by level, is one more `series` of `tuile_by_level`,
    /// beside what the pass selected.
    #[test]
    fn the_drawn_tiles_are_published_by_level() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let m = tuile_core::metrics::metrics();
        m.drawn_by_level.set(17, 41);
        m.selected_by_level.set(17, 43);
        metrics::with_local_recorder(&recorder, || publish(m));
        let text = handle.render();
        for line in [
            "tuile_by_level{series=\"drawn\",level=\"17\"} 41",
            "tuile_by_level{series=\"selected\",level=\"17\"} 43",
        ] {
            assert!(text.contains(line), "{line} missing from:\n{text}");
        }
    }

    /// A host's recorder — installed by the host, not by [`serve`] — gets the
    /// bake's timers and the series that were recorded but never published,
    /// from [`publish_every`]'s own loop.
    ///
    /// The runtime is current-thread so the loop runs on this thread, where
    /// the local recorder is the one its writes reach.
    #[test]
    fn publish_every_publishes_into_the_host_s_recorder() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let m = tuile_core::metrics::metrics();
        m.bake_frame_seconds.record(Duration::from_millis(2));
        m.pose_seconds.record(Duration::from_millis(30));
        m.height_queries.inc();
        m.ground_tiles.inc();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime");
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                publish_every(&tokio::runtime::Handle::current(), Duration::from_millis(5));
                tokio::time::sleep(Duration::from_millis(50)).await;
            });
        });
        let text = handle.render();
        for series in [
            "tuile_bake_frame_seconds_count",
            "tuile_bake_frame_seconds_sum",
            "tuile_bake_frame_seconds_peak",
            "tuile_pose_seconds_count",
            "tuile_finish_seconds_count",
            "tuile_ground_seconds_count",
            "tuile_height_query_seconds_count",
            "tuile_height_queries",
            "tuile_ground_tiles",
            "tuile_drapes_withheld",
            "tuile_priming_total",
            "tuile_priming_pending",
            "tuile_priming_unavailable",
            "tuile_priming_done",
            "tuile_ground_reads_pending",
            "tuile_ground_reads_pending_bytes",
            "tuile_settled_tiles",
            "tuile_settled_bytes",
            "tuile_stitch_plans",
            "# HELP tuile_stitch_overflows",
            "# HELP tuile_pose_seconds_sum",
        ] {
            assert!(text.contains(series), "{series} missing from:\n{text}");
        }
    }

    /// The budget's signals, once a session has published them, reach the
    /// endpoint under `tuile_budget_*`, with their closed labels.
    #[test]
    fn the_budget_s_signals_are_published() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut signals = tuile_core::budget::BudgetSignals::default();
        signals.lost_tile_steps_total = 3;
        signals.sse_scale_milli = 1500;
        tuile_core::metrics::publish_budget(signals);
        metrics::with_local_recorder(&recorder, || publish(tuile_core::metrics::metrics()));
        let text = handle.render();
        for series in [
            "tuile_budget_lost_tile_steps_total 3",
            "tuile_budget_sse_scale 1.5",
            "tuile_budget_bytes{class=\"terrain_mesh\",state=\"drawable\"}",
        ] {
            assert!(text.contains(series), "{series} missing from:\n{text}");
        }
    }

    /// What a frame drew, and what its eyes saw, reach the endpoint — and
    /// move when the numbers do.
    #[test]
    fn the_frame_and_its_eyes_are_published() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let m = tuile_core::metrics::metrics();
        m.frame_lost.set(7);
        m.eye_black_pixels.set(1, 42);
        metrics::with_local_recorder(&recorder, || publish(m));
        let text = handle.render();
        for series in [
            "tuile_frame_lost 7",
            "tuile_frame_exact",
            "tuile_frame_coarser",
            "tuile_frame_digest",
            "tuile_eye_pixels{series=\"black\",eye=\"1\"} 42",
            "tuile_eye_pixels{series=\"ground\",eye=\"0\"}",
        ] {
            assert!(text.contains(series), "{series} missing from:\n{text}");
        }
        m.frame_lost.set(0);
        metrics::with_local_recorder(&recorder, || publish(m));
        assert!(handle.render().contains("tuile_frame_lost 0"));
    }
}
