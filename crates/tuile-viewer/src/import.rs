// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Setting the application up from a configuration file, in one step.
//!
//! A file arrives — opened from Files, AirDrop, Mail or a download on a
//! phone, by a double-click or *File › Open…* on a Mac, by a share on
//! Android, or fetched from the address an `import` link names — and
//! [`import`] does the rest:
//!
//! 1. reads and checks it ([`crate::config_file::parse`]): a file that is
//!    wrong in any way changes nothing, and the message names the file and
//!    the key;
//! 2. hands the token to the system's secure storage ([`crate::secrets`]);
//! 3. writes the rest to the application's settings — the file
//!    [`settings_file`], TOML without the token — replacing what an earlier
//!    file set: the file is the whole configuration;
//! 4. answers with what a short confirmation names.
//!
//! The session takes the token at once — a phone's waiting start-up is
//! tried again straight away, and a desktop's — and the API address with
//! it. The terrain, the layers and the start view are the process's, read
//! when it starts: the confirmation says so when the file changed them.
//!
//! # The link
//!
//! ```text
//! tuile://import?src=https://example.org/team/config.tuile
//! ```
//!
//! The application fetches the file from `src` and imports it the same way.
//! **The token never travels in the link**: the link holds an address and
//! nothing else, and an address whose query names a token, a key, a secret
//! or a password is refused. `src` is `https` (or `http` on this machine's
//! own loopback, for a test), at most [`LONGEST_SOURCE`] characters, and the
//! answer at most [`crate::config_file::LONGEST_FILE`] bytes.

use std::path::{Path, PathBuf};

use crate::config_file::{self, ConfigError, Settings};
use crate::secrets::Secrets;

/// The longest `src` a link may name.
pub(crate) const LONGEST_SOURCE: usize = 2048;

/// The settings file's name, in the application's support directory.
pub(crate) const SETTINGS_FILE: &str = "settings.tuile";

/// The configuration a desktop reads at start when no `--config` names one:
/// the person's own file, in the support directory.
pub(crate) const DEFAULT_CONFIG_FILE: &str = "config.tuile";

/// What an import did: what the confirmation says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Imported {
    /// The file's name, as the person knows it.
    pub file: String,
    /// The asset the file is recognised by, when it names one.
    pub headline: Option<String>,
    /// Whether a token was stored.
    pub token: bool,
    /// Whether the terrain, the layers or the start view changed: those
    /// apply from the next start.
    pub next_start: bool,
}

impl Imported {
    /// The confirmation, in one sentence: never the token.
    pub(crate) fn sentence(&self) -> String {
        let what = match &self.headline {
            Some(asset) => format!("set up for {asset}"),
            None => "set up".to_owned(),
        };
        let token = if self.token {
            ", token kept in the system's secure storage"
        } else {
            ""
        };
        let later = if self.next_start {
            " The new layers and start view apply the next time the application starts."
        } else {
            ""
        };
        format!("{}: {what}{token}.{later}", self.file)
    }
}

/// The settings file, in the support directory.
pub(crate) fn settings_file(host: &dyn crate::host::Host) -> Option<PathBuf> {
    Some(crate::host::support_dir(host)?.join(SETTINGS_FILE))
}

/// The default configuration file, in the support directory.
pub(crate) fn default_config_file(host: &dyn crate::host::Host) -> Option<PathBuf> {
    Some(crate::host::support_dir(host)?.join(DEFAULT_CONFIG_FILE))
}

/// The configuration file this session was started with: `--config`, or the
/// default one. Read once, at start, and never kept.
static SESSION: std::sync::OnceLock<config_file::ConfigFile> = std::sync::OnceLock::new();

/// The settings the process started with: the layers, the terrain and the
/// start view it adopted.
static RUNNING: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();

/// The token of the session's configuration file, if it has one.
pub(crate) fn session_token() -> Option<&'static config_file::Token> {
    SESSION.get()?.token.as_ref()
}

/// The settings the process started with.
pub(crate) fn running() -> Settings {
    RUNNING.get().cloned().unwrap_or_default()
}

/// Reads the configuration at start: `flag`'s file when the command line
/// named one — an error in it is the command line's error — else, on a
/// desktop, `config.tuile` in the support directory if it is there, else
/// the settings an earlier import kept. Returns the settings the process
/// runs with.
pub(crate) fn start(
    section: &str,
    flag: Option<&Path>,
    host: &dyn crate::host::Host,
) -> Result<Settings, ConfigError> {
    let stored = || stored_settings(settings_file(host).as_deref(), section);
    let session = match flag {
        Some(path) => Some(read_session(path, section)?),
        None => default_config_file(host)
            .filter(|path| !host.is_handheld() && path.is_file())
            .and_then(|path| match read_session(&path, section) {
                Ok(read) => Some(read),
                Err(error) => {
                    tracing::error!("the default configuration is ignored: {error}");
                    None
                }
            }),
    };
    let settings = match session {
        Some(read) => {
            let settings = if read.settings == Settings::default() {
                stored()
            } else {
                read.settings.clone()
            };
            tracing::info!(
                token = read.token.is_some(),
                "configuration file read for this session"
            );
            let _ = SESSION.set(read);
            settings
        }
        None => stored(),
    };
    let _ = RUNNING.set(settings.clone());
    Ok(settings)
}

fn read_session(path: &Path, section: &str) -> Result<config_file::ConfigFile, ConfigError> {
    let text = read_file(path)?;
    config_file::parse(&file_name(&path.display().to_string()), &text, section)
}

/// The API address to use now: the session file's, else what the last
/// import kept — read afresh, so an import takes effect at the next
/// connection.
pub(crate) fn current_api(section: &str, host: &dyn crate::host::Host) -> Option<url::Url> {
    if let Some(read) = SESSION.get() {
        if read.settings.api.is_some() {
            return read.settings.api.clone();
        }
    }
    stored_settings(settings_file(host).as_deref(), section).api
}

/// Imports a file into this application: the host's section and
/// credential, the system's secure storage, the support directory's
/// settings.
pub(crate) fn import_here(file: &str, text: &str) -> Result<Imported, ConfigError> {
    let identity = crate::embed::identity();
    let section = &identity.config_section;
    if identity.credential_variable.is_empty() && text.contains("token") {
        if let Ok(read) = config_file::parse(file, text, section) {
            if read.token.is_some() {
                return Err(ConfigError {
                    file: file.to_owned(),
                    key: format!("{section}.token"),
                    why: "this application takes no token".into(),
                });
            }
        }
    }
    let host = crate::host::Machine;
    import(
        file,
        text,
        section,
        &identity.credential_variable,
        &crate::secrets::System,
        settings_file(&host).as_deref(),
        &running(),
    )
}

/// Imports `text`, the contents of the file called `file`.
///
/// `section` is the host's section of the file, `variable` the name its
/// credential goes by — the secure store's key. `settings` is where the
/// settings are kept; `running` what the process started with, to say
/// whether the new ones wait for the next start.
pub(crate) fn import(
    file: &str,
    text: &str,
    section: &str,
    variable: &str,
    secrets: &dyn Secrets,
    settings: Option<&Path>,
    running: &Settings,
) -> Result<Imported, ConfigError> {
    let read = config_file::parse(file, text, section)?;
    let fail = |key: String, why: String| ConfigError {
        file: file.to_owned(),
        key,
        why,
    };
    // Nothing is written until the whole file has been read and checked.
    if let Some(token) = &read.token {
        secrets
            .put(variable, token)
            .map_err(|why| fail(format!("{section}.token"), why))?;
    }
    if let Some(path) = settings {
        let kept = config_file::settings_text(section, &read.settings);
        let written = if kept.trim().is_empty() {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            crate::secrets::write_private(path, &kept)
        };
        written.map_err(|e| fail(String::new(), format!("could not keep the settings: {e}")))?;
    }
    let s = &read.settings;
    let next_start = s.terrain != running.terrain
        || s.imagery
            .iter()
            .map(|l| (&l.key, l.asset))
            .ne(running.imagery.iter().map(|l| (&l.key, l.asset)))
        || s.start != running.start;
    Ok(Imported {
        file: file.to_owned(),
        headline: s.headline(),
        token: read.token.is_some(),
        next_start,
    })
}

/// The settings an earlier import kept, read with the same checks as a file.
/// A settings file that no longer reads is said once and treated as absent.
pub(crate) fn stored_settings(path: Option<&Path>, section: &str) -> Settings {
    let Some(path) = path else {
        return Settings::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    match config_file::parse(&path.display().to_string(), &text, section) {
        Ok(read) => read.settings,
        Err(error) => {
            tracing::error!("the kept settings are ignored: {error}");
            Settings::default()
        }
    }
}

/// The file's name, as a person would recognise it: the last part of a
/// path or an address.
pub(crate) fn file_name(path_or_url: &str) -> String {
    path_or_url
        .trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .map_or_else(|| "the configuration".to_owned(), str::to_owned)
}

/// Reads a configuration file from disk, bounded.
pub(crate) fn read_file(path: &Path) -> Result<String, ConfigError> {
    let file = file_name(&path.display().to_string());
    let fail = |why: String| ConfigError {
        file: file.clone(),
        key: String::new(),
        why,
    };
    let size = std::fs::metadata(path)
        .map_err(|e| fail(format!("cannot be read: {e}")))?
        .len();
    if size > config_file::LONGEST_FILE as u64 {
        return Err(fail(format!(
            "larger than {} KiB: not a configuration",
            config_file::LONGEST_FILE / 1024
        )));
    }
    std::fs::read_to_string(path).map_err(|e| fail(format!("cannot be read as text: {e}")))
}

/// Whether `url` is an import link of the scheme `scheme`, and if so the
/// address it names — or why it is refused.
pub(crate) fn link_source(url: &str, scheme: &str) -> Option<Result<url::Url, String>> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.scheme().eq_ignore_ascii_case(scheme) || parsed.host_str() != Some("import") {
        return None;
    }
    Some(source_of(&parsed))
}

fn source_of(link: &url::Url) -> Result<url::Url, String> {
    let pairs: Vec<(String, String)> = link.query_pairs().into_owned().collect();
    let [(key, src)] = pairs.as_slice() else {
        return Err("an import link has exactly one parameter, src".into());
    };
    if key != "src" {
        return Err(format!(
            "an import link has one parameter, src, not {key:?}"
        ));
    }
    if src.len() > LONGEST_SOURCE {
        return Err(format!("src is longer than {LONGEST_SOURCE} characters"));
    }
    let source = url::Url::parse(src).map_err(|_| "src is not an address".to_owned())?;
    let local = matches!(source.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if source.scheme() != "https" && !(source.scheme() == "http" && local) {
        return Err("src must be an https:// address".into());
    }
    if !source.username().is_empty() || source.password().is_some() {
        return Err("src must not carry a user or a password".into());
    }
    let secret = |name: &str| {
        let name = name.to_ascii_lowercase();
        ["token", "key", "secret", "password"]
            .iter()
            .any(|word| name.contains(word))
    };
    if source.query_pairs().any(|(name, _)| secret(&name)) {
        return Err("a token never travels in a link: src names one in its query".into());
    }
    if link.fragment().is_some() || source.fragment().is_some() {
        return Err("an import link has no fragment".into());
    }
    Ok(source)
}

/// Fetches the file a link names: `https`, a few redirects at most (each
/// held to the same rule), a time limit, and a size limit.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn fetch(source: url::Url) -> Result<(String, String), String> {
    use futures_util::StreamExt;

    let name = file_name(source.path());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| format!("cannot fetch: {e}"))?;
    let response = client
        .get(source.clone())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("{name}: could not be fetched: {}", e.without_url()))?;
    if response
        .content_length()
        .is_some_and(|n| n > config_file::LONGEST_FILE as u64)
    {
        return Err(format!("{name}: larger than a configuration"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| format!("{name}: could not be fetched: {}", e.without_url()))?;
        body.extend_from_slice(&chunk);
        if body.len() > config_file::LONGEST_FILE {
            return Err(format!("{name}: larger than a configuration"));
        }
    }
    let text = String::from_utf8(body).map_err(|_| format!("{name}: not text"))?;
    Ok((name, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::fake::Fake;

    const FAKE: &str = "fake.0123456789abcdef.not-a-real-token";

    fn file(extra: &str) -> String {
        format!("[ion]\ntoken = \"{FAKE}\"\n{extra}")
    }

    #[test]
    fn a_file_puts_the_token_in_the_store_and_the_rest_in_the_settings() {
        let dir = tempfile::tempdir().expect("a directory");
        let settings = dir.path().join(SETTINGS_FILE);
        let store = Fake::default();
        let done = import(
            "mine.tuile",
            &file("terrain = 1\n[[ion.imagery]]\nkey = \"aerial\"\nname = \"Aerial\"\nasset = 2\n"),
            "ion",
            "SOME_TOKEN",
            &store,
            Some(&settings),
            &Settings::default(),
        )
        .expect("imported");
        assert_eq!(
            store.get("SOME_TOKEN").as_ref().map(|t| t.expose()),
            Some(FAKE)
        );
        let kept = std::fs::read_to_string(&settings).expect("settings kept");
        assert!(!kept.contains(FAKE), "the settings file holds the token");
        assert_eq!(stored_settings(Some(&settings), "ion").terrain, Some(1));
        assert!(done.token && done.next_start);
        assert_eq!(done.headline.as_deref(), Some("Aerial (asset 2)"));
        let said = done.sentence();
        assert!(
            said.starts_with("mine.tuile: set up for Aerial (asset 2)"),
            "{said}"
        );
        assert!(!said.contains(FAKE));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&settings)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the settings are readable by others");
        }
    }

    #[test]
    fn a_wrong_file_changes_nothing() {
        let dir = tempfile::tempdir().expect("a directory");
        let settings = dir.path().join(SETTINGS_FILE);
        std::fs::write(&settings, "[ion]\nterrain = 7\n").expect("an earlier import");
        let store = Fake::default();
        let error = import(
            "bad.tuile",
            &file("terrain = \"one\"\n"),
            "ion",
            "SOME_TOKEN",
            &store,
            Some(&settings),
            &Settings::default(),
        )
        .expect_err("refused");
        assert_eq!(error.key, "ion.terrain");
        assert!(
            store.get("SOME_TOKEN").is_none(),
            "a refused file stored its token"
        );
        assert_eq!(stored_settings(Some(&settings), "ion").terrain, Some(7));
    }

    #[test]
    fn a_store_that_refuses_is_said_by_key_and_keeps_the_settings() {
        let dir = tempfile::tempdir().expect("a directory");
        let settings = dir.path().join(SETTINGS_FILE);
        let store = Fake {
            refuse: true,
            ..Fake::default()
        };
        let error = import(
            "mine.tuile",
            &file("terrain = 1\n"),
            "ion",
            "SOME_TOKEN",
            &store,
            Some(&settings),
            &Settings::default(),
        )
        .expect_err("refused");
        assert_eq!(error.key, "ion.token");
        assert!(!error.to_string().contains(FAKE));
        assert!(!settings.exists());
    }

    #[test]
    fn a_token_only_file_clears_earlier_settings() {
        let dir = tempfile::tempdir().expect("a directory");
        let settings = dir.path().join(SETTINGS_FILE);
        std::fs::write(&settings, "[ion]\nterrain = 7\n").expect("an earlier import");
        let done = import(
            "mine.tuile",
            &file(""),
            "ion",
            "SOME_TOKEN",
            &Fake::default(),
            Some(&settings),
            &Settings::default(),
        )
        .expect("imported");
        assert!(!settings.exists());
        assert!(!done.next_start);
    }

    #[test]
    fn import_links_name_an_address_and_never_a_token() {
        let ok = link_source(
            "tuile://import?src=https://example.test/team/config.tuile",
            "tuile",
        )
        .expect("an import link")
        .expect("a good link");
        assert_eq!(ok.as_str(), "https://example.test/team/config.tuile");
        // Percent-encoded, as a page would write it.
        assert!(link_source(
            "tuile://import?src=https%3A%2F%2Fexample.test%2Fc.tuile%3Fv%3D2",
            "tuile"
        )
        .expect("an import link")
        .is_ok());
        assert!(link_source("tuile://goto?lon=1", "tuile").is_none());
        assert!(link_source("other://import?src=https://example.test/c", "tuile").is_none());
        for refused in [
            "tuile://import",
            "tuile://import?src=http://example.test/c.tuile",
            "tuile://import?src=https://example.test/c.tuile&token=abc",
            "tuile://import?src=https%3A%2F%2Fexample.test%2Fc%3Faccess_token%3Dabc",
            "tuile://import?src=https://me:pw@example.test/c.tuile",
            "tuile://import?source=https://example.test/c.tuile",
            "tuile://import?src=https://example.test/c.tuile#x",
            "tuile://import?src=file:///etc/passwd",
        ] {
            assert!(
                matches!(link_source(refused, "tuile"), Some(Err(_))),
                "{refused} was accepted"
            );
        }
        assert!(
            link_source("tuile://import?src=http://127.0.0.1:8000/c.tuile", "tuile")
                .expect("an import link")
                .is_ok()
        );
    }

    #[test]
    fn names_are_the_last_part() {
        assert_eq!(file_name("/a/b/mine.tuile"), "mine.tuile");
        assert_eq!(file_name("https://x.test/a/c.tuile"), "c.tuile");
        assert_eq!(file_name(""), "the configuration");
    }

    /// The example committed beside the host is a configuration in every
    /// respect but its placeholder token, which is refused by name.
    #[test]
    fn the_committed_example_reads_and_asks_for_a_real_token() {
        let text = include_str!("../../../examples/wgpu-viewer/config.example.tuile");
        let error =
            config_file::parse("config.example.tuile", text, "ion").expect_err("placeholder");
        assert_eq!(error.key, "ion.token");
        assert!(error.why.contains("placeholder"));
        let filled = text.replace("YOUR-ION-TOKEN", FAKE);
        assert!(config_file::parse("mine.tuile", &filled, "ion").is_ok());
        // And with every optional part switched on.
        let all = filled
            .lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .filter(|r| r.contains('=') || r.starts_with('['))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let read = config_file::parse("mine.tuile", &all, "ion").expect("every option");
        assert_eq!(read.settings.imagery.len(), 2);
        assert_eq!(read.settings.start.len(), 5);
    }
}
