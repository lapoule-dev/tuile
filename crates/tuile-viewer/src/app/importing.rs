// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A configuration file arriving in a running application: see
//! `crate::import`. Imported here, on the loop's thread, and then said in a
//! short message — what the file set up, or which key of which file was
//! wrong. Never the token.

use super::App;
use crate::steer::Request;

/// What a message about an import is titled.
const DONE: &str = "Configuration imported";
const REFUSED: &str = "Configuration not imported";

impl App {
    /// Carries out an import request; `false` when `request` is not one.
    pub(super) fn take_an_import(&mut self, request: &Request) -> bool {
        match request {
            Request::Import { file, text } => self.import(file, text),
            Request::ImportFile(path) => match crate::import::read_file(path) {
                Ok(text) => {
                    let file = crate::import::file_name(&path.display().to_string());
                    self.import(&file, &text);
                }
                Err(error) => self.messages.say(REFUSED.into(), error.to_string()),
            },
            Request::ImportFailed(why) => self.messages.say(REFUSED.into(), why.clone()),
            Request::Url(url) => {
                let Some(link) = crate::import::link_source(url, crate::steer::scheme()) else {
                    return false;
                };
                match link {
                    Ok(source) => fetch(source),
                    Err(why) => self
                        .messages
                        .say(REFUSED.into(), format!("the import link: {why}")),
                }
            }
            _ => return false,
        }
        true
    }

    fn import(&mut self, file: &str, text: &str) {
        match crate::import::import_here(file, text) {
            Ok(done) => {
                tracing::info!(file, token = done.token, "configuration imported");
                self.messages.say(DONE.into(), done.sentence());
                // A start-up that is waiting — for a token, most often — is
                // tried again now.
                if self.session.is_none() {
                    if let Some(retries) = self.retries.as_mut() {
                        retries.foreground(web_time::Instant::now());
                    }
                }
            }
            Err(error) => {
                tracing::error!("configuration not imported: {error}");
                self.messages.say(REFUSED.into(), error.to_string());
            }
        }
    }
}

/// On Android, what a view or a share brought is held by the activity and
/// taken here, once a second, onto the loop's queue.
#[cfg(target_os = "android")]
pub(super) fn take_from_the_activity() {
    use std::sync::Mutex;
    static LAST: Mutex<Option<web_time::Instant>> = Mutex::new(None);
    let now = web_time::Instant::now();
    let Ok(mut last) = LAST.lock() else {
        return;
    };
    if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(1)) {
        return;
    }
    *last = Some(now);
    if let Some(request) = crate::native::android::take_import() {
        crate::desktop::queue(request);
    }
}

/// Fetches an import link's file off the loop, and queues what came back.
fn fetch(source: url::Url) {
    let spawned = std::thread::Builder::new()
        .name("tuile-import".into())
        .spawn(move || {
            let outcome = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|runtime| runtime.block_on(crate::import::fetch(source)));
            crate::desktop::queue(match outcome {
                Ok((file, text)) => Request::Import { file, text },
                Err(why) => Request::ImportFailed(why),
            });
        });
    if let Err(error) = spawned {
        tracing::error!("cannot fetch the configuration: {error}");
    }
}
