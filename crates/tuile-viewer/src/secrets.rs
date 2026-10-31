// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The system's secure storage, for the one secret the viewer keeps: the
//! access token a configuration file brought ([`crate::config_file`]).
//!
//! | platform | where |
//! |---|---|
//! | macOS, iOS | the Keychain: a generic password, service = the bundle identifier, account = the credential's variable name |
//! | Android | the activity's private preferences, sealed with AES-GCM under a key that lives in the Android Keystore and never leaves it |
//! | elsewhere | the person's token file ([`crate::host`]'s `token_file`), mode 0600 — a desktop with no keychain this crate can reach |
//!
//! The value is never logged; failures name the store, never the secret.

use crate::config_file::Token;

/// Secure storage, as far as the importer needs one: a trait so the import
/// is tested against a store that is a map.
pub(crate) trait Secrets {
    /// Keeps `value` under `key`, replacing what was there.
    fn put(&self, key: &str, value: &Token) -> Result<(), String>;
    /// The value under `key`, if there is one.
    fn get(&self, key: &str) -> Option<Token>;
}

/// The machine's own store.
pub(crate) struct System;

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl Secrets for System {
    fn put(&self, key: &str, value: &Token) -> Result<(), String> {
        let service = &crate::embed::identity().bundle_identifier;
        security_framework::passwords::set_generic_password(service, key, value.expose().as_bytes())
            .map_err(|e| {
                format!(
                    "the Keychain refused to keep the token (status {})",
                    e.code()
                )
            })
    }

    fn get(&self, key: &str) -> Option<Token> {
        let service = &crate::embed::identity().bundle_identifier;
        let bytes = security_framework::passwords::get_generic_password(service, key).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let token = Token::new(&text);
        (!token.expose().is_empty()).then_some(token)
    }
}

#[cfg(target_os = "android")]
impl Secrets for System {
    fn put(&self, key: &str, value: &Token) -> Result<(), String> {
        if crate::native::android::seal_secret(key, value.expose()) {
            Ok(())
        } else {
            Err("the Android Keystore refused to keep the token".to_owned())
        }
    }

    fn get(&self, key: &str) -> Option<Token> {
        let text = crate::native::android::open_secret(key)?;
        let token = Token::new(&text);
        (!token.expose().is_empty()).then_some(token)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
impl Secrets for System {
    fn put(&self, _key: &str, value: &Token) -> Result<(), String> {
        let path = crate::host::token_file(&crate::host::Machine)
            .ok_or_else(|| "this machine has no home directory to keep the token in".to_owned())?;
        write_private(&path, &format!("{}\n", value.expose()))
            .map_err(|e| format!("could not write {}: {e}", path.display()))
    }

    /// Read back through the token file, which the credential lookup reads
    /// anyway.
    fn get(&self, _key: &str) -> Option<Token> {
        None
    }
}

/// Writes `text` to `path`, readable by its owner only, creating the
/// directory.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::Secrets;
    use crate::config_file::Token;

    /// A store that is a map, and that can be told to refuse.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub kept: Mutex<HashMap<String, Token>>,
        pub refuse: bool,
    }

    impl Secrets for Fake {
        fn put(&self, key: &str, value: &Token) -> Result<(), String> {
            if self.refuse {
                return Err("the store refused".into());
            }
            self.kept
                .lock()
                .expect("the fake's lock")
                .insert(key.to_owned(), value.clone());
            Ok(())
        }

        fn get(&self, key: &str) -> Option<Token> {
            self.kept.lock().expect("the fake's lock").get(key).cloned()
        }
    }
}
