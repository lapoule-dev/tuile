// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Shared test transport: a scripted [`IonHttp`] mock with a call log,
//! used by the unit tests of every module (no network).

use crate::{HttpResponse, IonError, IonHttp};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use url::Url;

/// Scripted transport: per-URL response queues + a call log of
/// `(url, bearer)`. A URL with several queued responses returns them in
/// order (the last one repeats).
#[derive(Default)]
pub struct MockHttp {
    responses: StdMutex<HashMap<String, Vec<HttpResponse>>>,
    log: StdMutex<Vec<(String, Option<String>)>>,
}

impl MockHttp {
    pub fn push(&self, url: &str, status: u16, body: &str) {
        self.responses
            .lock()
            .expect("lock")
            .entry(url.to_owned())
            .or_default()
            .push(HttpResponse {
                max_age: None,
                status,
                body: Bytes::from(body.to_owned()),
            });
    }

    pub fn calls(&self) -> Vec<(String, Option<String>)> {
        self.log.lock().expect("lock").clone()
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl IonHttp for MockHttp {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        self.log
            .lock()
            .expect("lock")
            .push((url.to_string(), bearer.map(str::to_owned)));
        let mut map = self.responses.lock().expect("lock");
        let queue = map
            .get_mut(url.as_str())
            .unwrap_or_else(|| unreachable!("unexpected request to {url}"));
        Ok(if queue.len() > 1 {
            queue.remove(0)
        } else {
            queue[0].clone()
        })
    }
}
