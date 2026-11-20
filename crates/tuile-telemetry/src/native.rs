// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The push from a native process: a task on the host's runtime that, every
//! interval, copies the registry out, encodes it and posts it.

use std::time::Duration;

use crate::{Config, Identity, Registry};

/// How long one push may take before it is given up: the next interval
/// sends the same counters, cumulative, so nothing is lost but resolution.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Starts what `config` asks for, on `handle`:
///
/// - with a metrics endpoint, the push, every `config.interval`, of what
///   the returned [`Registry`] records — the host installs it as (or beside)
///   its recorder: `tuile_metrics::serve_with`;
/// - with a profiling endpoint, the CPU profiles (macOS and Linux, the
///   `profiling` feature), from [`crate::ready`] on.
///
/// `None` without a metrics endpoint: nothing to record, nothing sent. The
/// switches ([`crate::set_metrics`], [`crate::set_profiling`]) act on both
/// at any time.
pub fn start_native(
    handle: &tokio::runtime::Handle,
    config: &Config,
    identity: Identity<'_>,
) -> Option<Registry> {
    let resource = crate::describe(config, &identity);
    crate::adopt(config, resource);
    #[cfg(all(feature = "profiling", any(target_os = "macos", target_os = "linux")))]
    if config.profiling.endpoint.is_some() {
        crate::profiling::configure(config.profiling.clone());
    }
    if config.profiling.endpoint.is_some() && !crate::profiling_supported() {
        tracing::info!("telemetry: CPU profiles are not available on this platform; metrics only");
    }
    let endpoint = config.endpoint.clone()?;
    let url = format!("{endpoint}/v1/metrics");
    let registry = Registry::new();
    let client = match reqwest::Client::builder().timeout(PUSH_TIMEOUT).build() {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("telemetry: no HTTP client ({e}); nothing sent");
            return None;
        }
    };
    let headers = config.headers.clone();
    let interval = config.interval;
    let pushed = registry.clone();
    let start_ns = crate::unix_nanos();
    tracing::info!(
        "telemetry: metrics to {endpoint} every {} s{}",
        interval.as_secs_f64(),
        if headers.is_empty() {
            ""
        } else {
            ", with credentials"
        }
    );
    handle.spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // The first tick is immediate: skip it, there is nothing to say yet.
        tick.tick().await;
        let mut failing = false;
        loop {
            tick.tick().await;
            if !crate::metrics_on() {
                continue;
            }
            // The engine's atomics into the facade first, so the push is of
            // now and not of the last publish period.
            tuile_metrics::publish(tuile_core::metrics::metrics());
            let encoded = crate::otlp::encode(
                &crate::resource(),
                &crate::points(&pushed),
                start_ns,
                crate::unix_nanos(),
            );
            let outcome = push(&client, &url, &headers, encoded.body).await;
            match (outcome, failing) {
                (Ok(()), true) => {
                    tracing::info!("telemetry: metrics sent again");
                    failing = false;
                }
                (Err(why), false) => {
                    tracing::warn!("telemetry: metrics not sent ({why}); retried every interval");
                    failing = true;
                }
                _ => {}
            }
        }
    });
    Some(registry)
}

/// One request. The reason of a failure names the status or the transport's
/// error kind, never a header.
async fn push(
    client: &reqwest::Client,
    url: &str,
    headers: &crate::Headers,
    body: String,
) -> Result<(), String> {
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    for (name, value) in headers.expose() {
        request = request.header(name.as_str(), value.as_str());
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => Ok(()),
        Ok(response) => Err(format!("HTTP {}", response.status().as_u16())),
        Err(e) if e.is_timeout() => Err("timed out".into()),
        Err(e) if e.is_connect() => Err("could not connect".into()),
        Err(_) => Err("transport error".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A request as the fake receiver read it: its head and its body.
    #[derive(Debug, Clone)]
    struct Heard {
        head: String,
        body: String,
    }

    /// A local OTLP/HTTP receiver: answers every request `200 {}` and keeps
    /// what it was sent.
    async fn receiver() -> (String, Arc<Mutex<Vec<Heard>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let addr = listener.local_addr().expect("its address");
        let heard: Arc<Mutex<Vec<Heard>>> = Arc::default();
        let kept = Arc::clone(&heard);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let kept = Arc::clone(&kept);
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 65536];
                    loop {
                        let Ok(n) = socket.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&data).into_owned();
                        let Some(split) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let head = text[..split].to_owned();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        let body = &data[split + 4..];
                        if body.len() < length {
                            continue;
                        }
                        kept.lock().expect("kept").push(Heard {
                            head,
                            body: String::from_utf8_lossy(&body[..length]).into_owned(),
                        });
                        data.drain(..split + 4 + length);
                        let _ = socket
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                            .await;
                    }
                });
            }
        });
        (format!("http://{addr}"), heard)
    }

    fn identity() -> Identity<'static> {
        Identity {
            service_name: "tuile-viewer",
            version: "0.0.0-test",
            commit: None,
            ref_name: None,
            device_class: "desktop",
            realm: "native",
        }
    }

    /// One test for the push's whole life, since its switches are the
    /// process's: nothing without an endpoint; with one, the mapping as a
    /// receiver reads it, the credential as a header; then nothing while
    /// switched off, and again once on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_push_sends_what_it_should_and_nothing_else() {
        let (base, heard) = receiver().await;
        let handle = tokio::runtime::Handle::current();

        // No endpoint: no registry, no task, nothing heard.
        assert!(start_native(&handle, &Config::default(), identity()).is_none());
        assert!(!crate::offered().metrics);

        let config = Config {
            endpoint: Some(base.clone()),
            headers: crate::Headers::bearer("t0ken"),
            interval: Duration::from_millis(100),
            ..Config::default()
        };
        let registry = start_native(&handle, &config, identity()).expect("a registry");
        assert!(crate::offered().metrics);
        metrics::with_local_recorder(&registry, || {
            metrics::histogram!("tuile_frame_time_seconds").record(0.012);
            metrics::counter!("tuile_tiles_fetch_total", "url" => "https://tiles.example/1.png")
                .increment(1);
            // The imagery layers' series, by role, as the engine publishes
            // them: a substitute asked, answered absent, a decision made.
            let layer = &tuile_core::metrics::metrics().layer;
            let sub = layer.of(tuile_core::metrics::LayerRole::Substitute);
            sub.requests.inc();
            sub.absents.inc();
            layer.fallback_active.set(1);
            tuile_metrics::publish(tuile_core::metrics::metrics());
        });
        // The engine's series reach it too, through `tuile_metrics::publish`
        // — which writes to the global recorder, absent in a test: the
        // registry is fed by hand above, and the engine's mapping is
        // `otlp`'s tests'.
        tokio::time::sleep(Duration::from_millis(350)).await;
        let first = heard.lock().expect("heard").clone();
        assert!(!first.is_empty(), "nothing was pushed");
        let req = &first[0];
        assert!(req.head.starts_with("POST /v1/metrics "), "{}", req.head);
        let lower = req.head.to_ascii_lowercase();
        assert!(
            lower.contains("content-type: application/json"),
            "{}",
            req.head
        );
        assert!(
            lower.contains("authorization: bearer t0ken"),
            "{}",
            req.head
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("JSON");
        let metrics = &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        assert!(metrics
            .as_array()
            .expect("metrics")
            .iter()
            .any(|m| m["name"] == "tuile_frame_time_seconds"));
        assert!(!req.body.contains("tiles.example"), "{}", req.body);
        // The substitution's series, labelled by layer role, allow-listed.
        let named = |n: &str| {
            metrics
                .as_array()
                .expect("metrics")
                .iter()
                .find(|m| m["name"] == n)
                .cloned()
        };
        let requests = named("tuile_layer_requests_total").expect("requests by layer");
        let points = requests["sum"]["dataPoints"].to_string();
        assert!(
            points.contains("\"layer_role\"") && points.contains("\"substitute\""),
            "{points}"
        );
        for n in [
            "tuile_layer_responses_total",
            "tuile_layer_bytes_total",
            "tuile_layer_fetch_seconds_sum",
            "tuile_layer_ancestor_fallbacks_total",
            "tuile_layer_switches_total",
            "tuile_layer_backoffs_total",
            "tuile_layer_fallback_active",
            "tuile_layer_fallback_suspended",
        ] {
            assert!(named(n).is_some(), "{n} not pushed");
        }
        let drawn = req.body.contains("drapes_substitute");
        assert!(drawn, "the drapes drawn by layer and level");
        // The camera's pose, from the first push after it is handed over.
        assert!(
            !req.body.contains("tuile_camera_"),
            "a pose before any was set"
        );
        let attrs = body["resourceMetrics"][0]["resource"]["attributes"].to_string();
        assert!(attrs.contains("\"device.class\""), "{attrs}");
        assert!(!req.body.contains("t0ken"));

        crate::set_camera(crate::CameraPose {
            altitude_m: 640.5,
            latitude_deg: 44.25,
            longitude_deg: 1.5,
            ..crate::CameraPose::default()
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        let last = heard
            .lock()
            .expect("heard")
            .last()
            .cloned()
            .expect("a push");
        let body: serde_json::Value = serde_json::from_str(&last.body).expect("JSON");
        let sent = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics")
            .clone();
        let gauge = |n: &str| {
            sent.iter()
                .find(|m| m["name"] == n)
                .map(|m| m["gauge"]["dataPoints"][0]["asDouble"].clone())
        };
        assert_eq!(gauge("tuile_camera_altitude_meters"), Some(640.5.into()));
        assert_eq!(gauge("tuile_camera_latitude_degrees"), Some(44.25.into()));
        assert_eq!(gauge("tuile_camera_longitude_degrees"), Some(1.5.into()));

        // Off: nothing more is sent.
        crate::set_metrics(false);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let off = heard.lock().expect("heard").len();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(heard.lock().expect("heard").len(), off, "sent while off");

        // On again, without a restart.
        crate::set_metrics(true);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(
            heard.lock().expect("heard").len() > off,
            "not sent once on again"
        );
    }
}
