// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the viewer takes from the machine it runs on: its credential, and the
//! two places it may read and write.
//!
//! # Where the token comes from
//!
//! **From the environment, first and always.** What a session ran with should
//! be visible in the command that started it; that rule is unchanged, and a
//! variable that is set is never overridden by anything.
//!
//! **Then, from the configuration file the session was started with** —
//! `--config <file>`, or `config.tuile` in the support directory — read for
//! that session and not kept (`crate::import`).
//!
//! **Then, from the system's secure storage**, where opening a
//! configuration file in the application keeps its token: the Keychain on
//! macOS and iOS, the Keystore-sealed store on Android (`crate::secrets`).
//!
//! **Then, on a phone, from what the person typed** in the system's settings
//! page for the application (*Settings › Tuile*), under the variable's name.
//!
//! **Then, on a phone, from the bundle** — a value written into `Info.plist`
//! when the application was built (`crates/tuile-viewer/ios/build.sh` does so
//! by default for a device build). **For a developer's own device only:**
//! anything in a bundle can be read back out of the `.app`, so a build for the
//! App Store, TestFlight or anyone else must not carry one.
//!
//! **Last, from one file the person owns.** An application started from an
//! icon has no shell and no environment:
//!
//! ```text
//! macOS      ~/Library/Application Support/Tuile/token
//! iOS        Documents/token in the container (Files › On My iPhone › Tuile)
//! elsewhere  $XDG_CONFIG_HOME/tuile/token   (or ~/.config/tuile/token)
//! ```
//!
//! The file holds the token on a line of its own — or a `NAME=value` line, so
//! a line lifted from a shell profile works as it is. Blank lines and lines
//! starting with `#` are skipped. On a desktop, create it yourself, mode 0600:
//!
//! ```text
//! install -m 600 /dev/null ~/Library/Application\ Support/Tuile/token
//! ```
//!
//! Nothing in this repository creates that file, and the token is never
//! logged: this module hands it to the caller as a `String` and says only
//! *where* it was found.
//!
//! This is not the `.env` in the working directory that `main` explains the
//! removal of. That file made a process behave differently depending on where
//! it was launched from; this one is the same file wherever the process
//! starts, belongs to the person rather than to a checkout, and loses to the
//! environment.

use std::path::{Path, PathBuf};

/// The application's name: its directories, its bundle, its menu. The
/// host's, once it has said who it is — see [`crate::embed::Identity`].
pub(crate) fn app_name() -> &'static str {
    &crate::embed::identity().name
}

/// The machine, as far as this module needs one. A trait so that the rules
/// above are tested against a machine that is a few fields.
pub(crate) trait Host {
    fn var(&self, name: &str) -> Option<String>;
    fn read(&self, path: &Path) -> std::io::Result<String>;
    /// Whether directories follow the macOS layout — which an iOS
    /// application's container does too, under its own home.
    fn is_macos(&self) -> bool;
    /// Whether this is a phone or a tablet, where the person can reach only
    /// the application's `Documents`, through the Files application.
    fn is_handheld(&self) -> bool {
        false
    }
    /// A value the person typed in the system's settings for this
    /// application, under `key`. Only a phone has such a page.
    fn setting(&self, _key: &str) -> Option<String> {
        None
    }
    /// A value written into the application bundle when it was built, under
    /// `key`. For a developer's own builds only: see [`bundled_key`].
    fn bundled(&self, _key: &str) -> Option<String> {
        None
    }
    /// The token of the configuration file this session was started with.
    fn configured(&self) -> Option<String> {
        None
    }
    /// A value in the system's secure storage, under `key`.
    fn secret(&self, _key: &str) -> Option<String> {
        None
    }
}

/// The `Info.plist` key a build-time credential for `variable` is written
/// under.
///
/// **A developer's own device only.** Anything in a bundle can be read back
/// out of it by whoever has the `.app`; a build that is distributed — the
/// App Store, TestFlight, a file sent to someone — must not carry one.
pub(crate) fn bundled_key(variable: &str) -> String {
    format!("{}{variable}", BUNDLED_PREFIX.trim())
}

/// The key's prefix, in one file that the bundler, which writes the key,
/// reads too: the two programs cannot drift apart.
const BUNDLED_PREFIX: &str = include_str!("../ios/bundled-credential-prefix.txt");

/// The real one.
pub(crate) struct Machine;

impl Host for Machine {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn read(&self, path: &Path) -> std::io::Result<String> {
        let text = std::fs::read_to_string(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            // A phone's application is the only account that can read its
            // container, and a file dropped in through Files arrives 0644:
            // nothing to warn about there.
            if mode & 0o077 != 0 && !self.is_handheld() {
                // Said, not refused: a refusal here would be a second way for
                // the application to fail to start over the same file.
                tracing::error!(
                    "{} can be read by other accounts (mode {:o}); run chmod 600 on it",
                    path.display(),
                    mode & 0o777
                );
            }
        }
        Ok(text)
    }

    fn is_macos(&self) -> bool {
        cfg!(any(target_os = "macos", target_os = "ios"))
    }

    fn is_handheld(&self) -> bool {
        cfg!(any(target_os = "ios", target_os = "android"))
    }

    fn setting(&self, key: &str) -> Option<String> {
        system::setting(key)
    }

    fn bundled(&self, key: &str) -> Option<String> {
        system::bundled(key)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn configured(&self) -> Option<String> {
        crate::import::session_token().map(|t| t.expose().to_owned())
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn secret(&self, key: &str) -> Option<String> {
        use crate::secrets::Secrets;
        crate::secrets::System
            .get(key)
            .map(|t| t.expose().to_owned())
    }
}

/// A phone's settings page and bundle, read through Foundation.
#[cfg(all(target_os = "ios", feature = "application"))]
mod system {
    use objc2_foundation::{NSBundle, NSString, NSUserDefaults};

    pub(super) fn setting(key: &str) -> Option<String> {
        NSUserDefaults::standardUserDefaults()
            .stringForKey(&NSString::from_str(key))
            .map(|v| v.to_string())
    }

    pub(super) fn bundled(key: &str) -> Option<String> {
        let value = NSBundle::mainBundle().objectForInfoDictionaryKey(&NSString::from_str(key))?;
        value.downcast::<NSString>().ok().map(|v| v.to_string())
    }
}

#[cfg(not(all(target_os = "ios", feature = "application")))]
mod system {
    pub(super) fn setting(_key: &str) -> Option<String> {
        None
    }

    pub(super) fn bundled(_key: &str) -> Option<String> {
        None
    }
}

fn home(host: &dyn Host) -> Option<PathBuf> {
    host.var("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The directory for what the person configures. `None` only on a machine
/// with no home directory, which has nowhere to keep a file anyway.
pub(crate) fn support_dir(host: &dyn Host) -> Option<PathBuf> {
    support_dir_of(host, app_name())
}

/// [`support_dir`], for an application called `name`.
pub(crate) fn support_dir_of(host: &dyn Host, name: &str) -> Option<PathBuf> {
    if host.is_macos() {
        return Some(home(host)?.join("Library/Application Support").join(name));
    }
    let base = host
        .var("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home(host)?.join(".config")))?;
    Some(base.join(name.to_lowercase()))
}

/// The file a session started from an icon leaves its reasons in: why it
/// could not start, why it stopped. Where the desktop's own log viewer looks.
pub(crate) fn log_file(host: &dyn Host) -> Option<PathBuf> {
    log_file_of(host, app_name())
}

/// [`log_file`], for an application called `name`.
pub(crate) fn log_file_of(host: &dyn Host, name: &str) -> Option<PathBuf> {
    if host.is_macos() {
        return Some(
            home(host)?
                .join("Library/Logs")
                .join(name)
                .join("viewer.log"),
        );
    }
    let base = host
        .var("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home(host)?.join(".local/state")))?;
    Some(base.join(name.to_lowercase()).join("viewer.log"))
}

/// Where a session started from an icon leaves the view it ended on, for the
/// next one to open on: one line, a `goto` URL.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn last_view_file(host: &dyn Host) -> Option<PathBuf> {
    Some(support_dir(host)?.join("last-view"))
}

/// Where the 3D Tiles tilesets a person switched on are kept for the next
/// session: one line, their asset numbers, comma-separated.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn tilesets_on_file(host: &dyn Host) -> Option<PathBuf> {
    Some(support_dir(host)?.join("tilesets-on"))
}

/// The file a token is read from, and the one a person is told to create.
///
/// On a phone that is `Documents/token`: the one directory of an application
/// the person can put a file in, through the Files application ("On My
/// iPhone", under the application's name) — `Library` is out of their reach.
pub(crate) fn token_file(host: &dyn Host) -> Option<PathBuf> {
    if host.is_handheld() {
        return Some(home(host)?.join("Documents").join("token"));
    }
    Some(support_dir(host)?.join("token"))
}

/// Where a token was found. Never the token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenFrom {
    Environment,
    /// The configuration file the session was started with.
    ConfigFile,
    /// The system's secure storage, where an opened configuration file
    /// keeps its token.
    SecureStorage,
    /// The system's settings page for the application.
    Settings,
    /// Written into the bundle at build time.
    Bundle,
    File(PathBuf),
}

/// There is no token, and this is what to do about it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MissingToken {
    /// The file it would be read from, when the machine has a place for one.
    pub file: Option<PathBuf>,
}

/// The token line of a file: the first line that is neither blank nor a
/// comment, without a `NAME=` in front of it or quotes around it.
fn token_line(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;
    let value = line.split_once('=').map_or(line, |(_, value)| value);
    let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Finds the token, first found wins:
///
/// 1. the environment variable — what a command line said;
/// 2. the configuration file the session was started with;
/// 3. the system's secure storage, where an opened file kept its token;
/// 4. the system's settings page (a phone's *Settings › Tuile*), under the
///    variable's name — what the person typed there;
/// 5. a value written into the bundle at build time (a developer's own
///    device only — see [`bundled_key`]);
/// 6. the person's file ([`token_file`]).
///
/// A value that is set but blank counts as unset — it is what an unfilled
/// template, or a settings field cleared by hand, holds.
pub(crate) fn token(host: &dyn Host, variable: &str) -> Result<(String, TokenFrom), MissingToken> {
    let filled = |v: Option<String>| v.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    if let Some(token) = filled(host.var(variable)) {
        return Ok((token, TokenFrom::Environment));
    }
    if let Some(token) = filled(host.configured()) {
        return Ok((token, TokenFrom::ConfigFile));
    }
    if let Some(token) = filled(host.secret(variable)) {
        return Ok((token, TokenFrom::SecureStorage));
    }
    if let Some(token) = filled(host.setting(variable)) {
        return Ok((token, TokenFrom::Settings));
    }
    if let Some(token) = filled(host.bundled(&bundled_key(variable))) {
        return Ok((token, TokenFrom::Bundle));
    }
    let file = token_file(host);
    let found = file
        .as_deref()
        .and_then(|path| host.read(path).ok())
        .as_deref()
        .and_then(token_line);
    match (found, file) {
        (Some(token), Some(file)) => Ok((token, TokenFrom::File(file))),
        (_, file) => Err(MissingToken { file }),
    }
}

/// Whether this executable sits inside an application bundle — which is to
/// say, whether it was probably started from an icon, with no terminal to
/// read its errors and no shell to have set its environment.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn is_bundled(executable: &Path) -> bool {
    let mut up = executable.ancestors().skip(1);
    let in_macos = up.next().and_then(Path::file_name) == Some("MacOS".as_ref());
    let in_contents = up.next().and_then(Path::file_name) == Some("Contents".as_ref());
    let in_app = up
        .next()
        .and_then(Path::extension)
        .is_some_and(|e| e == "app");
    in_macos && in_contents && in_app
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A machine that is two tables.
    #[derive(Default)]
    struct Fake {
        vars: HashMap<&'static str, &'static str>,
        files: HashMap<PathBuf, &'static str>,
        macos: bool,
        handheld: bool,
        settings: HashMap<String, &'static str>,
        bundle: HashMap<String, &'static str>,
        configured: Option<&'static str>,
        secrets: HashMap<String, &'static str>,
    }

    impl Host for Fake {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).map(|v| (*v).to_owned())
        }

        fn read(&self, path: &Path) -> std::io::Result<String> {
            self.files
                .get(path)
                .map(|t| (*t).to_owned())
                .ok_or_else(|| std::io::ErrorKind::NotFound.into())
        }

        fn is_macos(&self) -> bool {
            self.macos
        }

        fn is_handheld(&self) -> bool {
            self.handheld
        }

        fn setting(&self, key: &str) -> Option<String> {
            self.settings.get(key).map(|v| (*v).to_owned())
        }

        fn bundled(&self, key: &str) -> Option<String> {
            self.bundle.get(key).map(|v| (*v).to_owned())
        }

        fn configured(&self) -> Option<String> {
            self.configured.map(str::to_owned)
        }

        fn secret(&self, key: &str) -> Option<String> {
            self.secrets.get(key).map(|v| (*v).to_owned())
        }
    }

    /// A configuration file opened in the application keeps its token in
    /// secure storage, which comes after the environment and the session's
    /// own file, and before everything a phone or a file on disk holds.
    #[test]
    fn the_configured_and_kept_tokens_sit_between_the_environment_and_the_rest() {
        let mut host = mac(&[], Some("from-file"));
        host.secrets.insert(TOKEN_VARIABLE.into(), "from-keychain");
        host.settings.insert(TOKEN_VARIABLE.into(), "from-settings");
        assert_eq!(
            token(&host, TOKEN_VARIABLE),
            Ok(("from-keychain".into(), TokenFrom::SecureStorage))
        );
        host.configured = Some("from-config");
        assert_eq!(
            token(&host, TOKEN_VARIABLE),
            Ok(("from-config".into(), TokenFrom::ConfigFile))
        );
        host.vars.insert(TOKEN_VARIABLE, "from-env");
        assert_eq!(
            token(&host, TOKEN_VARIABLE),
            Ok(("from-env".into(), TokenFrom::Environment))
        );
        // A blank one is no token, as everywhere else.
        let mut blank = mac(&[], Some("from-file"));
        blank.secrets.insert(TOKEN_VARIABLE.into(), "  ");
        assert_eq!(
            token(&blank, TOKEN_VARIABLE),
            Ok(("from-file".into(), TokenFrom::File(PathBuf::from(FILE))))
        );
    }

    const TOKEN_VARIABLE: &str = "SOME_TOKEN";
    const FILE: &str = "/Users/someone/Library/Application Support/Tuile/token";

    fn mac(vars: &[(&'static str, &'static str)], file: Option<&'static str>) -> Fake {
        let mut vars: HashMap<_, _> = vars.iter().copied().collect();
        vars.insert("HOME", "/Users/someone");
        Fake {
            vars,
            files: file.map(|t| (PathBuf::from(FILE), t)).into_iter().collect(),
            macos: true,
            handheld: false,
            ..Fake::default()
        }
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let host = mac(
            &[(TOKEN_VARIABLE, "from-the-shell")],
            Some("from-the-file\n"),
        );
        assert_eq!(
            token(&host, TOKEN_VARIABLE),
            Ok(("from-the-shell".to_owned(), TokenFrom::Environment))
        );
    }

    #[test]
    fn the_file_is_used_when_the_environment_has_nothing() {
        for vars in [&[][..], &[(TOKEN_VARIABLE, "")], &[(TOKEN_VARIABLE, "  ")]] {
            let host = mac(vars, Some("from-the-file\n"));
            assert_eq!(
                token(&host, TOKEN_VARIABLE),
                Ok(("from-the-file".to_owned(), TokenFrom::File(FILE.into())))
            );
        }
    }

    #[test]
    fn the_file_may_be_a_bare_token_or_a_line_from_a_shell_profile() {
        for text in [
            "abc.def\n",
            "\n# my token\n  abc.def  \n",
            "SOME_TOKEN=abc.def\n",
            "SOME_TOKEN=\"abc.def\"\nOTHER=1\n",
        ] {
            let host = mac(&[], Some(text));
            assert_eq!(
                token(&host, TOKEN_VARIABLE).map(|(t, _)| t),
                Ok("abc.def".to_owned()),
                "{text:?}"
            );
        }
    }

    /// Neither: the error names the variable and the file, so the message can
    /// be shown as it is to someone who has never heard of either.
    #[test]
    fn with_neither_the_message_says_where_to_put_one() {
        for file in [
            None,
            Some(""),
            Some("# nothing yet\n\n"),
            Some("SOME_TOKEN=\n"),
        ] {
            let host = mac(&[], file);
            let missing = token(&host, TOKEN_VARIABLE).expect_err("there is no token");
            assert_eq!(missing.file, Some(PathBuf::from(FILE)));
            let said = crate::embed::MissingCredential {
                variable: TOKEN_VARIABLE.to_owned(),
                file: missing.file.clone(),
            }
            .to_string();
            assert!(
                said.contains(TOKEN_VARIABLE) && said.contains(FILE),
                "{said}"
            );
        }
    }

    #[test]
    fn the_directories_follow_the_platform() {
        let host = mac(&[], None);
        assert_eq!(
            log_file(&host),
            Some("/Users/someone/Library/Logs/Tuile/viewer.log".into())
        );
        let other = Fake {
            vars: [("HOME", "/home/someone")].into_iter().collect(),
            ..Fake::default()
        };
        assert_eq!(
            token_file(&other),
            Some("/home/someone/.config/tuile/token".into())
        );
        let moved = Fake {
            vars: [("HOME", "/home/someone"), ("XDG_CONFIG_HOME", "/etc/xdg")]
                .into_iter()
                .collect(),
            ..Fake::default()
        };
        assert_eq!(token_file(&moved), Some("/etc/xdg/tuile/token".into()));
        // Nowhere to look is a missing token, not a panic.
        assert_eq!(
            token(&Fake::default(), TOKEN_VARIABLE),
            Err(MissingToken { file: None })
        );
    }

    /// The view a session left is the view the next one opens on; a file
    /// that does not hold a whole, in-bounds view is no view at all.
    #[test]
    fn the_last_view_is_read_back_whole_or_not_at_all() {
        const LAST: &str = "/Users/someone/Library/Application Support/Tuile/last-view";
        let with = |text: &'static str| {
            let mut host = mac(&[], None);
            host.files.insert(PathBuf::from(LAST), text);
            crate::start::remembered(&host)
        };
        let view = crate::start::StartView {
            lon: -4.0167,
            lat: 5.3364,
            altitude: 1500.0,
            heading: 45.0,
            pitch: 30.0,
        };
        let got = with("tuile://goto?lon=-4.0167&lat=5.3364&altitude=1500&heading=45&pitch=30\n");
        assert_eq!(got, Some(view));
        for bad in [
            "",
            "tuile://north",
            "tuile://goto?lon=-4.0167&lat=5.3364",
            // Everything but the altitude: still not a whole view.
            "tuile://goto?lon=-4.0167&lat=5.3364&heading=45&pitch=30",
            "tuile://goto?lon=-4&lat=95&altitude=1500&heading=45&pitch=30",
        ] {
            assert_eq!(with(bad), None, "{bad:?}");
        }
        assert_eq!(crate::start::remembered(&mac(&[], None)), None);
    }

    /// On a phone the token is read from, and asked for in, the one folder
    /// the person can reach: the application's `Documents`.
    #[test]
    fn a_phone_reads_its_token_from_documents() {
        const HOME: &str = "/var/mobile/Containers/Data/Application/ABCD";
        let phone = |file: Option<&'static str>| Fake {
            vars: [("HOME", HOME)].into_iter().collect(),
            files: file
                .map(|t| (PathBuf::from(format!("{HOME}/Documents/token")), t))
                .into_iter()
                .collect(),
            macos: true,
            handheld: true,
            ..Fake::default()
        };
        assert_eq!(
            token(&phone(Some("abc.def\n")), TOKEN_VARIABLE),
            Ok((
                "abc.def".to_owned(),
                TokenFrom::File(format!("{HOME}/Documents/token").into())
            ))
        );
        assert_eq!(
            token(&phone(None), TOKEN_VARIABLE),
            Err(MissingToken {
                file: Some(format!("{HOME}/Documents/token").into())
            })
        );
        // The rest of the application's files stay out of the person's way.
        assert_eq!(
            last_view_file(&phone(None)),
            Some(format!("{HOME}/Library/Application Support/Tuile/last-view").into())
        );
    }

    /// Environment, then the settings page, then the bundle, then the
    /// file: each one wins over everything after it, and a blank one is
    /// passed over.
    /// The bundled key, as the bundler writes it: the shared prefix, then
    /// the variable.
    #[test]
    fn the_bundled_key_is_the_shared_prefix_and_the_variable() {
        assert_eq!(
            bundled_key("SOME_TOKEN"),
            "TuileBundledCredential_SOME_TOKEN"
        );
    }

    #[test]
    fn the_token_is_looked_for_in_order() {
        const HOME: &str = "/var/mobile/Containers/Data/Application/ABCD";
        let file = PathBuf::from(format!("{HOME}/Documents/token"));
        let phone = |env: Option<&'static str>,
                     setting: Option<&'static str>,
                     bundle: Option<&'static str>,
                     in_file: Option<&'static str>| {
            let mut vars: HashMap<_, _> = [("HOME", HOME)].into_iter().collect();
            if let Some(v) = env {
                vars.insert(TOKEN_VARIABLE, v);
            }
            Fake {
                vars,
                files: in_file.map(|t| (file.clone(), t)).into_iter().collect(),
                macos: true,
                handheld: true,
                settings: setting
                    .map(|t| (TOKEN_VARIABLE.to_owned(), t))
                    .into_iter()
                    .collect(),
                bundle: bundle
                    .map(|t| (bundled_key(TOKEN_VARIABLE), t))
                    .into_iter()
                    .collect(),
                ..Fake::default()
            }
        };
        let got = |h: Fake| token(&h, TOKEN_VARIABLE);
        assert_eq!(
            got(phone(Some("e"), Some("s"), Some("b"), Some("f"))),
            Ok(("e".into(), TokenFrom::Environment))
        );
        assert_eq!(
            got(phone(None, Some("s"), Some("b"), Some("f"))),
            Ok(("s".into(), TokenFrom::Settings))
        );
        assert_eq!(
            got(phone(None, Some("  "), Some("b"), Some("f"))),
            Ok(("b".into(), TokenFrom::Bundle)),
            "a cleared settings field falls through"
        );
        assert_eq!(
            got(phone(None, None, Some(""), Some("f\n"))),
            Ok(("f".into(), TokenFrom::File(file.clone())))
        );
        assert_eq!(
            got(phone(None, None, None, None)),
            Err(MissingToken { file: Some(file) })
        );
    }

    /// Wherever the token came from, the real resolution — the one the
    /// application runs, logging included — writes no line that carries it.
    #[test]
    fn the_token_is_never_in_a_log_line() {
        const SECRET: &str = "eyJhbGciOiJIUzI1NiJ9.secret-part.signature";
        #[derive(Clone, Default)]
        struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let mut found = Vec::new();
        tracing::subscriber::with_default(subscriber, || {
            let in_env = mac(&[(TOKEN_VARIABLE, SECRET)], None);
            let mut in_settings = mac(&[], None);
            in_settings
                .settings
                .insert(TOKEN_VARIABLE.to_owned(), SECRET);
            let mut in_bundle = mac(&[], None);
            in_bundle.bundle.insert(bundled_key(TOKEN_VARIABLE), SECRET);
            let in_file = mac(&[], Some(SECRET));
            for host in [&in_env, &in_settings, &in_bundle, &in_file] {
                found.push(crate::embed::credential_from(host, TOKEN_VARIABLE));
            }
            // And the refusal, which names where to put one.
            found.push(crate::embed::credential_from(
                &mac(&[], None),
                TOKEN_VARIABLE,
            ));
        });
        assert!(found[..4].iter().all(|f| f.as_deref() == Ok(SECRET)));
        assert!(found[4].is_err());
        let log = String::from_utf8(captured.0.lock().expect("log").clone()).expect("utf-8");
        assert_eq!(log.matches("access token from").count(), 4, "{log}");
        assert!(!log.contains("secret-part"), "the token reached the log");
    }

    #[test]
    fn an_executable_knows_whether_it_is_inside_a_bundle() {
        assert!(is_bundled(Path::new(
            "/Users/someone/Applications/Tuile.app/Contents/MacOS/tuile-wgpu-viewer"
        )));
        for outside in [
            "/work/target/release/tuile-wgpu-viewer",
            "/work/Tuile.app/tuile-wgpu-viewer",
            "/work/Tuile.app/Contents/Resources/tuile-wgpu-viewer",
            "/work/MacOS/Contents/x.app",
            "/work/Tuile/Contents/MacOS/tuile-wgpu-viewer",
            "tuile-wgpu-viewer",
        ] {
            assert!(!is_bundled(Path::new(outside)), "{outside}");
        }
    }
}
