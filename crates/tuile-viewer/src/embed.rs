// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a host says to make this application its own.
//!
//! The viewer is a window, a camera, its controls, the steering URLs, the
//! scripting dictionary and the session around them. None of that depends on
//! where the ground comes from, and a second program that wants the same
//! application over *its own* tiles should not have to copy it. So the whole
//! application is this library, and a binary is three answers:
//!
//! - **who it is** — [`Identity`]: the name in the menu bar and the title, the
//!   bundle identifier, the URL scheme, and from the name the directories it
//!   reads and writes;
//! - **what it can show** — one terrain ([`TerrainChoice`]) and a list of
//!   imagery layers ([`ImageryChoice`]), as data: a name for a person, a key
//!   for a flag or a URL, the asset number to ask the sources for, the
//!   attribution to keep on screen, and the namespace its tiles are stored
//!   under;
//! - **where the tiles come from** — [`ViewerHost::connect`], which finds the
//!   host's credentials wherever the host keeps them and answers with a
//!   [`Sources`]: terrain and imagery by asset number.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};
//!
//! struct Mine;
//!
//! #[async_trait::async_trait]
//! impl ViewerHost for Mine {
//!     fn identity(&self) -> Identity {
//!         Identity {
//!             name: "Atlas".into(),
//!             bundle_identifier: "org.example.atlas".into(),
//!             scheme: "atlas".into(),
//!             executable: "atlas".into(),
//!             store: "atlas-tiles".into(),
//!             credentials: "The key is read from ATLAS_KEY, or from the file `token` \
//!                           in the application's support directory."
//!                 .into(),
//!         }
//!     }
//!     fn terrain(&self) -> TerrainChoice {
//!         TerrainChoice { asset: 1, cache: "terrain-1".into() }
//!     }
//!     fn imagery(&self) -> Vec<ImageryChoice> {
//!         vec![ImageryChoice::new("aerial", "Aerial", 10, "© the aerial survey")]
//!     }
//!     async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
//!         let key = tuile_viewer::credential("ATLAS_KEY").map_err(|e| e.to_string())?;
//!         # let _ = key;
//!         # fn my_service(_: String) -> Arc<dyn Sources> { unimplemented!() }
//!         Ok(my_service(key))
//!     }
//! }
//!
//! fn main() {
//!     tuile_viewer::main(Mine)
//! }
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub use tuile_planetary::Sources;

use crate::host::{self, Host as _};

/// Who the application is, on the machine it runs on.
///
/// One value for the process: the name is in the title bar, in the alert that
/// says why a session could not start, and in every directory the application
/// touches; the scheme is the one its steering URLs are written in. The
/// application bundle is built with the same four words (see
/// `tuile-viewer-bundle`), or the system delivers the URLs to somebody else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The application's name: `Tuile`. Its support directory, its log
    /// directory and its window title carry it.
    pub name: String,
    /// Reverse-DNS, as the bundle's `CFBundleIdentifier`.
    pub bundle_identifier: String,
    /// The URL scheme a running session is steered by: `tuile`, for
    /// `tuile://goto?…`.
    pub scheme: String,
    /// The executable's name, for `--help` and for messages on stderr.
    pub executable: String,
    /// The name of the on-disk tile store this application keeps between
    /// runs. Terrain and each imagery layer are filed inside it under their
    /// own namespaces ([`TerrainChoice::cache`], [`ImageryChoice::cache`]).
    pub store: String,
    /// One sentence for `--help`: where this host's credentials are read
    /// from. Empty when it needs none.
    pub credentials: String,
}

impl Default for Identity {
    /// The project's own application.
    fn default() -> Self {
        Self {
            name: "Tuile".into(),
            bundle_identifier: "dev.lapoule.tuile.viewer".into(),
            scheme: "tuile".into(),
            executable: "tuile-wgpu-viewer".into(),
            store: "tiles".into(),
            credentials: String::new(),
        }
    }
}

impl Identity {
    /// The directory for what the person configures — a credential file, the
    /// last view: `~/Library/Application Support/<name>` on macOS,
    /// `$XDG_CONFIG_HOME/<name, lower case>` elsewhere.
    pub fn support_dir(&self) -> Option<PathBuf> {
        host::support_dir_of(&host::Machine, &self.name)
    }

    /// The file a session started from an icon leaves its reasons in:
    /// `~/Library/Logs/<name>/viewer.log` on macOS.
    pub fn log_file(&self) -> Option<PathBuf> {
        host::log_file_of(&host::Machine, &self.name)
    }

    /// The one file a credential may be read from when the environment has
    /// none: `token`, in [`Identity::support_dir`].
    pub fn credential_file(&self) -> Option<PathBuf> {
        Some(self.support_dir()?.join("token"))
    }

    /// Where the tile store lives on disk.
    pub fn caches_dir(&self) -> PathBuf {
        tuile_storage_foyer::default_cache_dir().join(&self.store)
    }
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

/// The identity this process runs under: the host's, once [`crate::main`] has
/// taken it, and the project's own before that — which is what every test of
/// this library runs under.
pub(crate) fn identity() -> &'static Identity {
    static DEFAULT: OnceLock<Identity> = OnceLock::new();
    IDENTITY
        .get()
        .unwrap_or_else(|| DEFAULT.get_or_init(Identity::default))
}

/// Fixes the process's identity. Once: the first host wins, and there is one.
pub(crate) fn adopt(identity: Identity) {
    let _ = IDENTITY.set(identity);
}

/// The terrain a session stands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainChoice {
    /// The number [`Sources::terrain`] is asked for.
    pub asset: i64,
    /// The namespace its tiles are stored under, inside [`Identity::store`].
    pub cache: String,
}

/// One imagery layer a session can drape, as the host describes it.
///
/// Data, not code: the list is what the `--imagery` flag accepts, what a
/// steering URL names and what a menu shows, and the host is the only one who
/// knows it. The grid a layer is cut on is not stated here — the provider
/// [`Sources::imagery`] answers with says so itself, and web-mercator and
/// geographic layers drape alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageryChoice {
    /// What a flag, a URL and a script call it: lower case, no spaces.
    pub key: String,
    /// What a person reads.
    pub name: String,
    /// The number [`Sources::imagery`] is asked for.
    pub asset: i64,
    /// Who the pictures belong to, kept on screen while the layer is.
    pub attribution: String,
    /// The namespace its tiles are stored under, inside [`Identity::store`].
    /// Per layer, always: the store is keyed by tile address, and two layers
    /// under one namespace serve each other's pictures.
    pub cache: String,
}

impl ImageryChoice {
    /// A layer stored under `imagery-<key>`.
    pub fn new(key: &str, name: &str, asset: i64, attribution: &str) -> Self {
        Self {
            key: key.to_owned(),
            name: name.to_owned(),
            asset,
            attribution: attribution.to_owned(),
            cache: format!("imagery-{key}"),
        }
    }

    /// The same layer, stored under a namespace the host chooses — one that
    /// already holds its tiles, say.
    #[must_use]
    pub fn cached_as(mut self, namespace: &str) -> Self {
        namespace.clone_into(&mut self.cache);
        self
    }
}

/// The three answers that make the application a host's own: see the module.
#[async_trait::async_trait]
pub trait ViewerHost: Send + Sync + 'static {
    /// Who the application is. Asked once, before anything else.
    fn identity(&self) -> Identity;

    /// The terrain the globe is built on.
    fn terrain(&self) -> TerrainChoice;

    /// The imagery layers on offer, in the order a person should meet them.
    /// The first is what a session opens on. Must not be empty.
    fn imagery(&self) -> Vec<ImageryChoice>;

    /// Finds the host's credentials and answers with its sources.
    ///
    /// Called once, on the session's async runtime, before the window exists.
    /// An error is a sentence for the person — "there is no key; put one
    /// here" — shown in a window when there is no terminal, and the session
    /// does not start. [`credential`] is the lookup this application's own
    /// binary uses, for a host that wants the same rules.
    async fn connect(&self) -> Result<Arc<dyn Sources>, String>;
}

/// There is no credential, and this is what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCredential {
    /// The environment variable that was looked at.
    pub variable: String,
    /// The file it would otherwise be read from, when the machine has a
    /// place for one.
    pub file: Option<PathBuf>,
}

impl std::fmt::Display for MissingCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "There is no access token for terrain and imagery. Export {} before \
             starting from a terminal",
            self.variable
        )?;
        match &self.file {
            Some(file) => write!(
                f,
                ", or put the token on a line of its own in this file, readable \
                 by you alone (chmod 600):\n\n{}",
                file.display()
            ),
            None => write!(f, "."),
        }
    }
}

impl std::error::Error for MissingCredential {}

/// A host's credential, by the rules of [`crate::host`]: the environment
/// variable `variable` first and always; otherwise the file `token` in the
/// application's support directory, which the person creates and owns.
///
/// The value is handed over and never logged; the log says only where it was
/// found.
pub fn credential(variable: &str) -> Result<String, MissingCredential> {
    match host::token(&host::Machine, variable) {
        Ok((token, from)) => {
            tracing::info!("access token from {from:?}");
            Ok(token)
        }
        Err(missing) => Err(MissingCredential {
            variable: variable.to_owned(),
            file: missing.file,
        }),
    }
}

/// Reads an [`Identity`] override from the environment, for running the same
/// binary as a second, separate application — a test copy beside the one a
/// person is using. `TUILE_VIEWER_NAME`, `TUILE_VIEWER_IDENTIFIER` and
/// `TUILE_VIEWER_SCHEME` replace the three words that must differ for the two
/// not to share a support directory or steal each other's URLs; the bundler
/// writes them into a bundle it is asked to rename.
#[must_use]
pub fn renamed_by_environment(identity: Identity) -> Identity {
    renamed(identity, |name| host::Machine.var(name))
}

fn renamed(mut identity: Identity, var: impl Fn(&str) -> Option<String>) -> Identity {
    let var = |name: &str| var(name).filter(|v| !v.trim().is_empty());
    if let Some(name) = var("TUILE_VIEWER_NAME") {
        identity.name = name;
    }
    if let Some(identifier) = var("TUILE_VIEWER_IDENTIFIER") {
        identity.bundle_identifier = identifier;
    }
    if let Some(scheme) = var("TUILE_VIEWER_SCHEME") {
        identity.scheme = scheme;
    }
    identity
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second copy of the application keeps to itself only if all three
    /// words change; an empty variable is an unfilled template, not a name.
    #[test]
    fn a_renamed_copy_takes_its_three_words_from_the_environment() {
        let said = |name: &str| match name {
            "TUILE_VIEWER_NAME" => Some("Tuile Test".to_owned()),
            "TUILE_VIEWER_IDENTIFIER" => Some("dev.lapoule.tuile.viewer.test".to_owned()),
            "TUILE_VIEWER_SCHEME" => Some("tuile-test".to_owned()),
            _ => None,
        };
        let copy = renamed(Identity::default(), said);
        assert_eq!(copy.name, "Tuile Test");
        assert_eq!(copy.bundle_identifier, "dev.lapoule.tuile.viewer.test");
        assert_eq!(copy.scheme, "tuile-test");
        assert_eq!(copy.executable, Identity::default().executable);

        let blank = renamed(Identity::default(), |_| Some("  ".to_owned()));
        assert_eq!(blank, Identity::default());
        assert_eq!(renamed(Identity::default(), |_| None), Identity::default());
    }

    /// The directories follow the name, so two applications never share a
    /// credential file or a journal.
    #[test]
    fn an_applications_files_carry_its_name() {
        let other = Identity {
            name: "Atlas".into(),
            ..Identity::default()
        };
        let (mine, theirs) = (Identity::default(), other);
        if let (Some(a), Some(b)) = (mine.credential_file(), theirs.credential_file()) {
            assert_ne!(a, b);
            assert!(b.to_string_lossy().to_lowercase().contains("atlas"), "{b:?}");
            assert!(b.ends_with("token"));
        }
        if let (Some(a), Some(b)) = (mine.log_file(), theirs.log_file()) {
            assert_ne!(a, b);
        }
    }

    /// A layer is stored under its own key unless the host says otherwise.
    #[test]
    fn a_layer_is_stored_under_its_own_namespace() {
        let layer = ImageryChoice::new("aerial", "Aerial", 2, "©");
        assert_eq!(layer.cache, "imagery-aerial");
        assert_eq!(layer.clone().cached_as("old").cache, "old");
        assert_eq!(layer.asset, 2);
    }

    /// What a person without a credential is told names both places.
    #[test]
    fn a_missing_credential_says_where_to_put_one() {
        let said = MissingCredential {
            variable: "SOME_KEY".into(),
            file: Some("/somewhere/token".into()),
        }
        .to_string();
        assert!(said.contains("SOME_KEY") && said.contains("/somewhere/token"), "{said}");
    }
}
