// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The configuration file a tester opens to set the application up: one
//! TOML file, opened once.
//!
//! ```toml
//! [ion]                                 # the host's section: Identity::config_section
//! token = "your-access-token"           # stored in the system's secure storage
//! api = "https://api.cesium.com/"       # optional: the service's API base
//! terrain = 1                           # optional: the terrain's asset id
//!
//! [[ion.imagery]]                       # optional: replaces the host's layers
//! key = "aerial"
//! name = "Aerial"
//! asset = 2
//! attribution = "© Microsoft and its data suppliers"
//!
//! [start]                               # optional: where the first session opens
//! lon = 6.86
//! lat = 45.83
//! altitude = 6000
//! heading = 120
//! pitch = 25
//! ```
//!
//! **The token is read once and handed to secure storage.** It never reaches
//! a log line, an error message or the settings file: [`Token`] has no
//! `Display` and a `Debug` that says only that it is there, and every error
//! names the file and the key, never the value — including a TOML syntax
//! error, which is reported by line alone, since the parser's own message
//! quotes the text around the fault.
//!
//! The rest is checked here and kept as [`Settings`], written back as TOML
//! without the token. The start view is held to the command line's bounds
//! (`crate::start`), and a flag given on the command line wins over it.
//!
//! Strict on purpose, like the command line: an unknown key is an error that
//! names it, because a misspelt `terain` that is silently ignored opens a
//! session somewhere other than where the file said.

use toml::{Table, Value};

use crate::embed::ImageryChoice;

/// The longest file read: a configuration is a few hundred bytes, and
/// anything much larger is not one.
pub(crate) const LONGEST_FILE: usize = 64 * 1024;

/// The view numbers a file may say, in the command line's names and units.
const START_KEYS: [&str; 5] = ["lon", "lat", "altitude", "heading", "pitch"];

/// An access token. Never printed: no `Display`, a `Debug` that hides it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Token(String);

impl Token {
    pub(crate) fn new(value: &str) -> Self {
        Self(value.trim().to_owned())
    }

    /// The value, for the one place that needs it: secure storage, or the
    /// request that carries it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<withheld>)")
    }
}

/// What a file says besides its token: kept in the application's settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Settings {
    /// The service's API base, ending in `/`.
    pub api: Option<url::Url>,
    /// The terrain's asset id.
    pub terrain: Option<i64>,
    /// The imagery layers, in order; empty means the host's own.
    pub imagery: Vec<ImageryChoice>,
    /// Start-view numbers by flag name (`lon`, `lat`, …), already checked.
    pub start: Vec<(String, f64)>,
}

impl Settings {
    /// The asset a person would recognise the file by: the first imagery
    /// layer's name, else the terrain's number.
    pub(crate) fn headline(&self) -> Option<String> {
        if let Some(layer) = self.imagery.first() {
            return Some(format!("{} (asset {})", layer.name, layer.asset));
        }
        self.terrain.map(|asset| format!("terrain asset {asset}"))
    }
}

/// A file, read and checked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConfigFile {
    pub token: Option<Token>,
    pub settings: Settings,
}

/// What was wrong with a file: which file, which key, and why — never a
/// value from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigError {
    pub file: String,
    /// The key's dotted path (`ion.imagery[1].asset`), or what stands for
    /// one (`line 4`) when the file could not be read as TOML at all.
    pub key: String,
    pub why: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.key.is_empty() {
            write!(f, "{}: {}", self.file, self.why)
        } else {
            write!(f, "{}: {}: {}", self.file, self.key, self.why)
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads `text`, the contents of the file called `file`, whose credential
/// section is `section` (the host's: `ion` for the public viewer).
pub(crate) fn parse(file: &str, text: &str, section: &str) -> Result<ConfigFile, ConfigError> {
    let fail = |key: &str, why: &str| ConfigError {
        file: file.to_owned(),
        key: key.to_owned(),
        why: why.to_owned(),
    };
    if text.len() > LONGEST_FILE {
        return Err(fail(
            "",
            &format!(
                "larger than {} KiB: not a configuration",
                LONGEST_FILE / 1024
            ),
        ));
    }
    let table: Table = text.parse().map_err(|e: toml::de::Error| {
        // The span alone: the message quotes the text around the fault, and
        // that text may be the token's line.
        let line = e
            .span()
            .map(|span| text[..span.start.min(text.len())].lines().count().max(1));
        fail(
            &line.map_or_else(String::new, |n| format!("line {n}")),
            "not valid TOML",
        )
    })?;

    let mut token = None;
    let mut settings = Settings::default();
    for (key, value) in &table {
        if key == section {
            let Value::Table(source) = value else {
                return Err(fail(key, "must be a table: [section]"));
            };
            read_section(section, source, &mut token, &mut settings)
                .map_err(|(k, why)| fail(&k, &why))?;
        } else if key == "start" {
            let Value::Table(start) = value else {
                return Err(fail(key, "must be a table: [start]"));
            };
            settings.start = read_start(start).map_err(|(k, why)| fail(&k, &why))?;
        } else {
            return Err(fail(
                key,
                &format!("unknown section; this file knows [{section}] and [start]"),
            ));
        }
    }
    if token.is_none() && settings == Settings::default() {
        return Err(fail(
            section,
            &format!("missing: the file says nothing; it needs at least [{section}] token"),
        ));
    }
    Ok(ConfigFile { token, settings })
}

type Problem = (String, String);

fn read_section(
    section: &str,
    source: &Table,
    token: &mut Option<Token>,
    settings: &mut Settings,
) -> Result<(), Problem> {
    let at = |key: &str| format!("{section}.{key}");
    for (key, value) in source {
        match key.as_str() {
            "token" => {
                let Value::String(text) = value else {
                    return Err((at(key), "must be a string in quotes".into()));
                };
                let given = Token::new(text);
                if given.expose().is_empty() {
                    return Err((at(key), "is empty".into()));
                }
                if given
                    .expose()
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err((
                        at(key),
                        "holds spaces or line breaks: copy the token again".into(),
                    ));
                }
                if looks_like_placeholder(given.expose()) {
                    return Err((
                        at(key),
                        "is still the example's placeholder: put your own token there".into(),
                    ));
                }
                *token = Some(given);
            }
            "api" => {
                let Value::String(text) = value else {
                    return Err((at(key), "must be a URL in quotes".into()));
                };
                settings.api = Some(api_base(text).map_err(|why| (at(key), why))?);
            }
            "terrain" => settings.terrain = Some(asset(value).map_err(|why| (at(key), why))?),
            "imagery" => {
                let Value::Array(layers) = value else {
                    return Err((at(key), "must be a list of [[imagery]] tables".into()));
                };
                settings.imagery = read_imagery(section, layers)?;
            }
            _ => {
                return Err((
                    at(key),
                    "unknown key; the keys are token, api, terrain and imagery".into(),
                ))
            }
        }
    }
    Ok(())
}

fn read_imagery(section: &str, layers: &[Value]) -> Result<Vec<ImageryChoice>, Problem> {
    let mut out: Vec<ImageryChoice> = Vec::new();
    for (i, layer) in layers.iter().enumerate() {
        let at = |key: &str| {
            format!(
                "{section}.imagery[{i}]{}{key}",
                if key.is_empty() { "" } else { "." }
            )
        };
        let Value::Table(layer) = layer else {
            return Err((at(""), "must be a table".into()));
        };
        let text = |key: &str| -> Result<Option<String>, Problem> {
            match layer.get(key) {
                None => Ok(None),
                Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
                Some(_) => Err((at(key), "must be a non-empty string".into())),
            }
        };
        for key in layer.keys() {
            if !["key", "name", "asset", "attribution"].contains(&key.as_str()) {
                return Err((
                    at(key),
                    "unknown key; a layer has key, name, asset and attribution".into(),
                ));
            }
        }
        let asset = asset(
            layer
                .get("asset")
                .ok_or_else(|| (at("asset"), "missing".into()))?,
        )
        .map_err(|why| (at("asset"), why))?;
        let key = text("key")?.unwrap_or_else(|| format!("asset{asset}"));
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err((
                at("key"),
                "lower-case letters, digits and dashes only".into(),
            ));
        }
        if out.iter().any(|l| l.key == key) {
            return Err((at("key"), "used by an earlier layer".into()));
        }
        let name = text("name")?.unwrap_or_else(|| key.clone());
        let attribution = text("attribution")?.unwrap_or_default();
        out.push(ImageryChoice::new(&key, &name, asset, &attribution));
    }
    Ok(out)
}

fn read_start(start: &Table) -> Result<Vec<(String, f64)>, Problem> {
    let mut out = Vec::new();
    for (key, value) in start {
        let at = format!("start.{key}");
        if !START_KEYS.contains(&key.as_str()) {
            return Err((
                at,
                format!("unknown key; the keys are {}", START_KEYS.join(", ")),
            ));
        }
        let number = match value {
            Value::Integer(n) => *n as f64,
            Value::Float(x) => *x,
            _ => return Err((at, "must be a number".into())),
        };
        let checked = crate::start::checked(key, number).map_err(|why| (at.clone(), why))?;
        out.push((key.clone(), checked));
    }
    Ok(out)
}

fn asset(value: &Value) -> Result<i64, String> {
    match value {
        Value::Integer(n) if *n > 0 => Ok(*n),
        Value::Integer(_) => Err("must be a positive asset number".into()),
        _ => Err("must be a number, without quotes".into()),
    }
}

/// An API base: `https`, ending in `/` so the service's paths are read under
/// it. `http` only for this machine's own loopback.
fn api_base(text: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(text.trim()).map_err(|_| "is not a URL".to_owned())?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err("must be an https:// URL".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err("must not carry a user, a password or a query".into());
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

/// The example file's token, or one like it: refused, so a tester who
/// forgot to paste theirs is told so and not sent to a 401.
fn looks_like_placeholder(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("your") && lower.contains("token") || lower.starts_with("paste")
}

/// The settings, as the TOML the application keeps — no token, ever.
pub(crate) fn settings_text(section: &str, settings: &Settings) -> String {
    let mut root = Table::new();
    let mut source = Table::new();
    if let Some(api) = &settings.api {
        source.insert("api".into(), Value::String(api.to_string()));
    }
    if let Some(terrain) = settings.terrain {
        source.insert("terrain".into(), Value::Integer(terrain));
    }
    if !settings.imagery.is_empty() {
        let layers = settings
            .imagery
            .iter()
            .map(|l| {
                let mut t = Table::new();
                t.insert("key".into(), Value::String(l.key.clone()));
                t.insert("name".into(), Value::String(l.name.clone()));
                t.insert("asset".into(), Value::Integer(l.asset));
                if !l.attribution.is_empty() {
                    t.insert("attribution".into(), Value::String(l.attribution.clone()));
                }
                Value::Table(t)
            })
            .collect();
        source.insert("imagery".into(), Value::Array(layers));
    }
    if !source.is_empty() {
        root.insert(section.into(), Value::Table(source));
    }
    if !settings.start.is_empty() {
        let start = settings
            .start
            .iter()
            .map(|(k, v)| (k.clone(), Value::Float(*v)))
            .collect();
        root.insert("start".into(), Value::Table(start));
    }
    toml::to_string(&root).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token shaped like a real one and belonging to no one.
    const FAKE: &str = "fake.0123456789abcdef.not-a-real-token";

    fn read(text: &str) -> Result<ConfigFile, ConfigError> {
        parse("config.tuile", text, "ion")
    }

    #[test]
    fn a_whole_file_is_read() {
        let file = read(&format!(
            r#"
            [ion]
            token = "{FAKE}"
            api = "https://api.example.test/v1"
            terrain = 1

            [[ion.imagery]]
            key = "aerial"
            name = "Aerial"
            asset = 2
            attribution = "© someone"

            [[ion.imagery]]
            asset = 3954

            [start]
            lon = 6.86
            lat = 45.83
            altitude = 6000
            "#
        ))
        .expect("a good file");
        assert_eq!(file.token.as_ref().map(Token::expose), Some(FAKE));
        let s = &file.settings;
        assert_eq!(
            s.api.as_ref().map(url::Url::as_str),
            Some("https://api.example.test/v1/")
        );
        assert_eq!(s.terrain, Some(1));
        assert_eq!(s.imagery.len(), 2);
        assert_eq!(s.imagery[0].asset, 2);
        assert_eq!(s.imagery[1].key, "asset3954");
        assert_eq!(s.start.len(), 3);
        assert_eq!(s.headline().as_deref(), Some("Aerial (asset 2)"));
    }

    #[test]
    fn a_token_alone_is_a_file() {
        let file = read(&format!("[ion]\ntoken = \"{FAKE}\"\n")).expect("a token alone");
        assert_eq!(file.settings, Settings::default());
    }

    /// Every way a file can be wrong names the file and the key — and no
    /// message ever carries the token, even when the fault is on its line.
    #[test]
    fn errors_name_the_key_and_never_the_value() {
        let cases = [
            (format!("[ion]\ntoken = {FAKE}\n"), "line 2"),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\nterain = 1\n"),
                "ion.terain",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\nterrain = \"1\"\n"),
                "ion.terrain",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\napi = \"http://example.test\"\n"),
                "ion.api",
            ),
            (format!("[ion]\ntoken = \"{FAKE} {FAKE}\"\n"), "ion.token"),
            (
                "[ion]\ntoken = \"YOUR-ION-TOKEN\"\n".to_owned(),
                "ion.token",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\n[[ion.imagery]]\nname = \"x\"\n"),
                "ion.imagery[0].asset",
            ),
            (
                format!("[ion]\ntoken = \"{FAKE}\"\n[start]\nlat = 91\n"),
                "start.lat",
            ),
            (format!("[ion]\ntoken = \"{FAKE}\"\n[other]\n"), "other"),
            ("# nothing\n".to_owned(), "ion"),
        ];
        for (text, key) in cases {
            let error = read(&text).expect_err(&text);
            assert_eq!(error.key, key, "{text}");
            assert_eq!(error.file, "config.tuile");
            let said = error.to_string();
            assert!(!said.contains(FAKE), "the token leaked into {said:?}");
            assert!(
                !said.contains("fake."),
                "part of the token leaked into {said:?}"
            );
        }
    }

    #[test]
    fn the_token_is_never_printed() {
        let file = read(&format!("[ion]\ntoken = \"{FAKE}\"\n")).expect("a token");
        assert!(!format!("{file:?}").contains(FAKE));
    }

    #[test]
    fn the_settings_file_holds_no_token_and_reads_back() {
        let file = read(&format!(
            "[ion]\ntoken = \"{FAKE}\"\nterrain = 1\n[[ion.imagery]]\nkey = \"aerial\"\nasset = 2\n[start]\nlon = 3\n"
        ))
        .expect("a good file");
        let kept = settings_text("ion", &file.settings);
        assert!(!kept.contains(FAKE));
        assert!(!kept.contains("token"));
        let again = read(&kept).expect("the settings file is itself a configuration");
        assert_eq!(again.token, None);
        assert_eq!(again.settings, file.settings);
    }

    #[test]
    fn a_huge_file_is_refused_unread() {
        let error = read(&"#".repeat(LONGEST_FILE + 1)).expect_err("too large");
        assert!(error.why.contains("KiB"));
    }
}
