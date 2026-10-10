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
//! **Otherwise, from one file the person owns.** An application started from
//! an icon has no shell and no environment, so there is exactly one other
//! place:
//!
//! ```text
//! macOS      ~/Library/Application Support/Tuile/token
//! elsewhere  $XDG_CONFIG_HOME/tuile/token   (or ~/.config/tuile/token)
//! ```
//!
//! The file holds the token on a line of its own — or a `NAME=value` line, so
//! a line lifted from a shell profile works as it is. Blank lines and lines
//! starting with `#` are skipped. Create it yourself, mode 0600:
//!
//! ```text
//! install -m 600 /dev/null ~/Library/Application\ Support/Tuile/token
//! ```
//!
//! Nothing in this repository creates that file, no build puts a token in the
//! application bundle, and the token is never logged: this module hands it to
//! the caller as a `String` and says only *where* it was found.
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
    /// Whether directories follow the macOS layout.
    fn is_macos(&self) -> bool;
}

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
            if mode & 0o077 != 0 {
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
        cfg!(target_os = "macos")
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
pub(crate) fn last_view_file(host: &dyn Host) -> Option<PathBuf> {
    Some(support_dir(host)?.join("last-view"))
}

/// The one file a token may be read from.
pub(crate) fn token_file(host: &dyn Host) -> Option<PathBuf> {
    Some(support_dir(host)?.join("token"))
}

/// Where a token was found. Never the token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenFrom {
    Environment,
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

/// Finds the token: the environment if it has one, the person's file if not.
///
/// A variable that is set but empty counts as unset — it is what an unfilled
/// template exports, and "the token is the empty string" helps nobody.
pub(crate) fn token(
    host: &dyn Host,
    variable: &str,
) -> Result<(String, TokenFrom), MissingToken> {
    if let Some(token) = host
        .var(variable)
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
    {
        return Ok((token, TokenFrom::Environment));
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
        assert_eq!(token(&Fake::default(), TOKEN_VARIABLE), Err(MissingToken { file: None }));
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
