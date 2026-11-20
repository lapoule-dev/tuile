// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A snapshot of the registry as an OTLP `ExportMetricsServiceRequest`, in
//! OTLP/JSON — the encoding every OTLP/HTTP receiver accepts beside
//! protobuf, and the one a browser can send without a codec.
//!
//! One encoder for every platform, so a desktop and a page send byte for
//! byte the same shapes: counters as cumulative monotonic sums, gauges as
//! gauges, histograms as cumulative explicit-bucket histograms, all from the
//! process's start. Integers that are 64-bit in the protocol are strings, as
//! its JSON mapping says.
//!
//! The allow-lists are applied here, at the last step before the wire:
//! a series with a label outside [`crate::resource::ALLOWED_LABELS`] is left
//! out whole, and so is a `tuile_camera_*` series that is not one of the
//! camera's ([`crate::camera::CAMERA_METRICS`]).

use serde_json::{json, Value as Json};

use crate::registry::{Point, Value};
use crate::resource::{allowed_label, Resource};

/// The instrumentation scope every series is filed under.
pub const SCOPE: &str = "tuile";

/// OTLP's `AGGREGATION_TEMPORALITY_CUMULATIVE`.
const CUMULATIVE: u8 = 2;

/// What was encoded: the body, and how many series were left out by the
/// label allow-list.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoded {
    pub body: String,
    pub series: usize,
    pub withheld: usize,
}

/// Whether `point` may leave the process: every label key on the list,
/// and, for a camera series, a name on the camera's
/// ([`crate::camera::CAMERA_METRICS`]).
pub fn sendable(point: &Point) -> bool {
    crate::camera::allowed_name(&point.name) && point.labels.iter().all(|(k, _)| allowed_label(k))
}

/// `points`, as one request: `start_ns` the process's start, `now_ns` the
/// instant of the values, both Unix nanoseconds.
pub fn encode(resource: &Resource, points: &[Point], start_ns: u64, now_ns: u64) -> Encoded {
    let (start, now) = (start_ns.to_string(), now_ns.to_string());
    let mut withheld = 0;
    let mut metrics: Vec<Json> = Vec::new();
    // Points of one name together, as one metric with several data points.
    let mut i = 0;
    while i < points.len() {
        let name = &points[i].name;
        let mut j = i;
        while j < points.len() && points[j].name == *name {
            j += 1;
        }
        let group: Vec<&Point> = points[i..j]
            .iter()
            .filter(|p| {
                let ok = sendable(p);
                if !ok {
                    withheld += 1;
                }
                ok
            })
            .collect();
        i = j;
        if let Some(metric) = metric(name, &group, &start, &now) {
            metrics.push(metric);
        }
    }
    let series = metrics
        .iter()
        .map(|m| {
            ["sum", "gauge", "histogram"]
                .iter()
                .filter_map(|k| m.get(*k))
                .map(|d| d["dataPoints"].as_array().map_or(0, Vec::len))
                .sum::<usize>()
        })
        .sum();
    let body = json!({
        "resourceMetrics": [{
            "resource": { "attributes": attributes(resource.iter()) },
            "scopeMetrics": [{
                "scope": { "name": SCOPE, "version": env!("CARGO_PKG_VERSION") },
                "metrics": metrics,
            }],
        }],
    });
    Encoded {
        body: body.to_string(),
        series,
        withheld,
    }
}

fn attributes<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> Json {
    Json::Array(
        pairs
            .map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } }))
            .collect(),
    )
}

/// One metric of `name` from its points, all of one kind — the first's;
/// a point of another kind under the same name is left out.
fn metric(name: &str, points: &[&Point], start: &str, now: &str) -> Option<Json> {
    let first = points.first()?;
    let labels = |p: &Point| attributes(p.labels.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    match &first.value {
        Value::Counter(_) => {
            let data: Vec<Json> = points
                .iter()
                .filter_map(|p| match p.value {
                    Value::Counter(v) => Some(json!({
                        "attributes": labels(p),
                        "startTimeUnixNano": start,
                        "timeUnixNano": now,
                        "asInt": v.to_string(),
                    })),
                    _ => None,
                })
                .collect();
            Some(json!({
                "name": name,
                "sum": {
                    "dataPoints": data,
                    "aggregationTemporality": CUMULATIVE,
                    "isMonotonic": true,
                },
            }))
        }
        Value::Gauge(_) => {
            let data: Vec<Json> = points
                .iter()
                .filter_map(|p| match p.value {
                    Value::Gauge(v) if v.is_finite() => Some(json!({
                        "attributes": labels(p),
                        "timeUnixNano": now,
                        "asDouble": v,
                    })),
                    _ => None,
                })
                .collect();
            Some(json!({ "name": name, "gauge": { "dataPoints": data } }))
        }
        Value::Histogram { .. } => {
            let data: Vec<Json> = points
                .iter()
                .filter_map(|p| match &p.value {
                    Value::Histogram {
                        bounds,
                        counts,
                        count,
                        sum,
                    } => Some(json!({
                        "attributes": labels(p),
                        "startTimeUnixNano": start,
                        "timeUnixNano": now,
                        "count": count.to_string(),
                        "sum": sum,
                        "bucketCounts": counts.iter().map(u64::to_string).collect::<Vec<_>>(),
                        "explicitBounds": bounds,
                    })),
                    _ => None,
                })
                .collect();
            Some(json!({
                "name": name,
                "histogram": {
                    "dataPoints": data,
                    "aggregationTemporality": CUMULATIVE,
                },
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    fn resource() -> Resource {
        let mut r = Resource::default();
        r.set("service.name", "viewer");
        r.set("service.instance.id", "0123");
        r
    }

    /// The eviction/prefetch decider's series (its spec §7.1) reach the
    /// collector, every label on the allow-list: the quality deficit and the
    /// specification in force, with whether a diagnostic override set it.
    #[test]
    fn the_budget_decider_s_series_reach_the_collector() {
        let registry = Registry::new();
        let mut s = tuile_core::budget::BudgetSignals::default();
        s.under_need_tiles = [1, 2];
        s.deficit_levels = [1, 3];
        s.deficit_level_micros_total = [1_000_000, 2_000_000];
        s.spec = tuile_core::budget::SpecKind::Fpv;
        s.spec_overridden = true;
        s.spec_changes_total = 2;
        tuile_core::metrics::publish_budget(s);
        metrics::with_local_recorder(&registry, || {
            tuile_metrics::publish(tuile_core::metrics::metrics())
        });
        let out = encode(&resource(), &registry.snapshot(), 1_000, 2_000);
        assert_eq!(out.withheld, 0, "every label is allowed");
        let v: Json = serde_json::from_str(&out.body).expect("JSON");
        let metrics = v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics")
            .clone();
        let point = |name: &str, labels: &[(&str, &str)]| -> Option<Json> {
            let m = metrics.iter().find(|m| m["name"] == name)?;
            let data = m.get("gauge").or_else(|| m.get("sum"))?;
            data["dataPoints"]
                .as_array()?
                .iter()
                .find(|p| {
                    labels.iter().all(|(k, val)| {
                        p["attributes"].as_array().is_some_and(|a| {
                            a.contains(&json!({"key": k, "value": {"stringValue": val}}))
                        })
                    })
                })
                .cloned()
        };
        let value = |p: Option<Json>| {
            p.map(|p| {
                p["asDouble"]
                    .as_f64()
                    .or_else(|| p["asInt"].as_str().and_then(|x| x.parse().ok()))
                    .unwrap_or(f64::NAN)
            })
        };
        assert_eq!(
            value(point("tuile_budget_under_need_tiles", &[("kind", "imagery")])),
            Some(2.0)
        );
        assert_eq!(
            value(point("tuile_budget_deficit_levels", &[("kind", "imagery")])),
            Some(3.0)
        );
        assert_eq!(
            value(point("tuile_budget_deficit_level_seconds_total", &[("kind", "mesh")])),
            Some(1.0)
        );
        assert_eq!(
            value(point(
                "tuile_budget_spec_info",
                &[("spec", "fpv"), ("source", "override")]
            )),
            Some(1.0)
        );
        assert_eq!(
            value(point(
                "tuile_budget_spec_info",
                &[("spec", "interactive"), ("source", "override")]
            )),
            Some(0.0)
        );
        assert_eq!(value(point("tuile_budget_spec_changes_total", &[])), Some(2.0));
    }

    /// The mapping: names, kinds, temporality, labels, the 64-bit integers
    /// as strings, the buckets as the protocol counts them.
    #[test]
    fn each_kind_maps_to_its_otlp_shape() {
        let registry = Registry::new();
        metrics::with_local_recorder(&registry, || {
            metrics::counter!("tuile_frames").absolute(42);
            metrics::gauge!("tuile_budget_bytes", "class" => "terrain_mesh", "state" => "drawable")
                .set(1024.0);
            metrics::histogram!("tuile_tiles_fetch_seconds", "source" => "cache", "kind" => "mesh")
                .record(0.003);
        });
        let out = encode(&resource(), &registry.snapshot(), 1_000, 2_000);
        assert_eq!((out.series, out.withheld), (3, 0));
        let v: Json = serde_json::from_str(&out.body).expect("JSON");
        let rm = &v["resourceMetrics"][0];
        let attrs = rm["resource"]["attributes"].as_array().expect("attributes");
        assert!(attrs.contains(&json!({"key": "service.name", "value": {"stringValue": "viewer"}})));
        let metrics = rm["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics");
        let by = |n: &str| metrics.iter().find(|m| m["name"] == n).expect(n);

        let c = &by("tuile_frames")["sum"];
        assert_eq!(c["aggregationTemporality"], 2);
        assert_eq!(c["isMonotonic"], true);
        assert_eq!(c["dataPoints"][0]["asInt"], "42");
        assert_eq!(c["dataPoints"][0]["startTimeUnixNano"], "1000");
        assert_eq!(c["dataPoints"][0]["timeUnixNano"], "2000");

        let g = &by("tuile_budget_bytes")["gauge"]["dataPoints"][0];
        assert_eq!(g["asDouble"], 1024.0);
        assert_eq!(
            g["attributes"],
            json!([
                {"key": "class", "value": {"stringValue": "terrain_mesh"}},
                {"key": "state", "value": {"stringValue": "drawable"}},
            ])
        );

        let h = &by("tuile_tiles_fetch_seconds")["histogram"];
        assert_eq!(h["aggregationTemporality"], 2);
        let p = &h["dataPoints"][0];
        assert_eq!(p["count"], "1");
        assert_eq!(p["sum"], 0.003);
        let bounds = p["explicitBounds"].as_array().expect("bounds");
        let counts = p["bucketCounts"].as_array().expect("counts");
        assert_eq!(counts.len(), bounds.len() + 1);
        assert_eq!(
            counts[3], "1",
            "0.003 is in the bucket up to 0.004: {counts:?}"
        );
    }

    /// The privacy guard on labels: a series with a label outside the list
    /// is not sent; its siblings are.
    #[test]
    fn a_label_outside_the_list_withholds_the_series() {
        let registry = Registry::new();
        metrics::with_local_recorder(&registry, || {
            metrics::counter!("tuile_tiles_fetch_total", "source" => "cache", "kind" => "mesh", "outcome" => "hit").increment(1);
            metrics::counter!("tuile_tiles_fetch_total", "url" => "https://tiles.example/1/2/3.terrain").increment(1);
            metrics::histogram!("tuile_tiles_zone_listing_seconds", "layer" => "a-layer")
                .record(1.0);
        });
        let out = encode(&resource(), &registry.snapshot(), 0, 1);
        assert_eq!((out.series, out.withheld), (1, 2));
        assert!(!out.body.contains("tiles.example"), "{}", out.body);
        assert!(!out.body.contains("a-layer"), "{}", out.body);
        assert!(
            !out.body.contains("tuile_tiles_zone_listing_seconds"),
            "{}",
            out.body
        );
    }

    /// The camera's pose goes out as plain gauges, its values as they are;
    /// a series under the camera's prefix that is not on its list does not.
    #[test]
    fn the_camera_s_gauges_go_and_nothing_else_of_its_kind() {
        let pose = crate::camera::CameraPose {
            altitude_m: 812.75,
            height_ellipsoid_m: 2950.5,
            heading_deg: 271.0,
            pitch_deg: 18.0,
            roll_deg: 1.5,
            fov_deg: 45.0,
            latitude_deg: 45.8326,
            longitude_deg: 6.8652,
        };
        let registry = Registry::new();
        metrics::with_local_recorder(&registry, || {
            metrics::gauge!("tuile_camera_device_latitude_degrees").set(48.1234);
        });
        let mut points = registry.snapshot();
        points.extend(pose.points());
        let out = encode(&resource(), &points, 0, 1);
        assert_eq!((out.series, out.withheld), (8, 1));
        assert!(!out.body.contains("device_latitude"), "{}", out.body);
        assert!(!out.body.contains("48.1234"), "{}", out.body);
        let v: Json = serde_json::from_str(&out.body).expect("JSON");
        let metrics = v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics");
        let value = |n: &str| {
            let m = metrics.iter().find(|m| m["name"] == n).expect(n);
            let p = &m["gauge"]["dataPoints"][0];
            assert_eq!(p["attributes"], json!([]), "{n} carries labels");
            p["asDouble"].as_f64().expect("a double")
        };
        assert_eq!(value("tuile_camera_altitude_meters"), 812.75);
        assert_eq!(value("tuile_camera_height_ellipsoid_meters"), 2950.5);
        assert_eq!(value("tuile_camera_heading_degrees"), 271.0);
        assert_eq!(value("tuile_camera_pitch_degrees"), 18.0);
        assert_eq!(value("tuile_camera_roll_degrees"), 1.5);
        assert_eq!(value("tuile_camera_fov_degrees"), 45.0);
        assert_eq!(value("tuile_camera_latitude_degrees"), 45.8326);
        assert_eq!(value("tuile_camera_longitude_degrees"), 6.8652);
    }

    /// Every series the engine publishes passes the label guard: a label
    /// added to the engine without a thought for the list fails here, not
    /// silently in production.
    #[test]
    fn every_engine_series_passes_the_guard() {
        let registry = Registry::new();
        let mut signals = tuile_core::budget::BudgetSignals::default();
        signals.lost_tile_steps_total = 1;
        tuile_core::metrics::publish_budget(signals);
        metrics::with_local_recorder(&registry, || {
            tuile_metrics::publish(tuile_core::metrics::metrics());
        });
        let points = registry.snapshot();
        assert!(points.len() > 100, "only {} series", points.len());
        let refused: Vec<&Point> = points.iter().filter(|p| !sendable(p)).collect();
        assert!(refused.is_empty(), "withheld: {refused:?}");
    }
}
