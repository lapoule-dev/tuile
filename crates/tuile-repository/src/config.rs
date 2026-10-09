// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a bench serves: its projects and its tile store, from a TOML file.
//!
//! Everything that names a deployment — buckets, projects, how an
//! orchestrator lays out its runs — lives in that file and nowhere in the
//! code.

use std::path::{Path, PathBuf};

use crate::RunLayout;

/// A bucket on the configured endpoint, a directory, or — for the tile
/// store alone — another server of this API's `store/…` routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Bucket(String),
    Dir(PathBuf),
    /// A tile store served by someone else: `url` is what its
    /// `store/live/…` and `store/b8/…` routes are under. `header` names the
    /// request header that server wants a credential in; the credential
    /// itself is the host's secret (`TUILE_TILES_REMOTE_SECRET`), never in
    /// this file.
    Remote { url: String, header: Option<String> },
}

/// The key layout a project's bucket is read with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// The engine's own: packs keyed by scene, under these roots.
    Scenes(Vec<String>),
    /// Run directories, laid out as the file says.
    Runs(RunLayout),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    pub place: Place,
    pub layout: Layout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub projects: Vec<ProjectConfig>,
    pub tiles: Option<Place>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlace {
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    dir: Option<PathBuf>,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    header: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    name: String,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    dir: Option<PathBuf>,
    #[serde(default)]
    scenes: Option<Vec<String>>,
    #[serde(default)]
    runs: Option<RunLayout>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default, rename = "project")]
    projects: Vec<RawProject>,
    #[serde(default)]
    tiles: Option<RawPlace>,
}

fn place(what: &str, bucket: Option<String>, dir: Option<PathBuf>) -> Result<Place, String> {
    match (bucket, dir) {
        (Some(b), None) if !b.is_empty() => Ok(Place::Bucket(b)),
        (None, Some(d)) => Ok(Place::Dir(d)),
        _ => Err(format!("{what}: exactly one of `bucket` and `dir`")),
    }
}

impl Config {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.to_string())?;
        if raw.projects.is_empty() {
            return Err("no [[project]]".into());
        }
        let mut projects = Vec::new();
        for p in raw.projects {
            let named = !p.name.is_empty()
                && p.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !named {
                return Err(format!(
                    "project `{}`: a name is letters, digits, - and _",
                    p.name
                ));
            }
            if projects.iter().any(|q: &ProjectConfig| q.name == p.name) {
                return Err(format!("project `{}` appears twice", p.name));
            }
            let what = format!("project `{}`", p.name);
            let layout = match (p.scenes, p.runs) {
                (Some(roots), None) if !roots.is_empty() => Layout::Scenes(roots),
                (None, Some(runs)) => Layout::Runs(runs),
                _ => {
                    return Err(format!(
                        "{what}: exactly one of `scenes` (roots) and `[project.runs]`"
                    ))
                }
            };
            projects.push(ProjectConfig {
                place: place(&what, p.bucket, p.dir)?,
                name: p.name,
                layout,
            });
        }
        let tiles = match raw.tiles {
            Some(RawPlace {
                bucket: None,
                dir: None,
                remote: Some(url),
                header,
            }) => {
                let named = header.as_deref().is_none_or(|h| {
                    !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                });
                if !(url.starts_with("https://") || url.starts_with("http://")) || !named {
                    return Err(
                        "[tiles]: `remote` is an http(s) address, `header` a header's name".into(),
                    );
                }
                Some(Place::Remote {
                    url: url.trim_end_matches('/').to_string(),
                    header,
                })
            }
            Some(RawPlace {
                remote: None,
                header: None,
                bucket,
                dir,
            }) => Some(place("[tiles]", bucket, dir)?),
            Some(_) => {
                return Err(
                    "[tiles]: one of `bucket`, `dir` and `remote`; `header` goes with `remote`"
                        .into(),
                )
            }
            None => None,
        };
        Ok(Config { projects, tiles })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../examples/film-web/film-bench.example.toml");

    /// The example a deployment copies must itself be a configuration.
    #[test]
    fn the_example_file_parses() {
        let config = Config::parse(EXAMPLE).expect("example");
        assert_eq!(config.projects.len(), 2);
        assert!(matches!(&config.projects[0].layout, Layout::Scenes(roots) if roots == &["packs"]));
        let Layout::Runs(runs) = &config.projects[1].layout else {
            panic!("the second project reads runs");
        };
        assert_eq!(runs.depth, 2);
        assert!(runs
            .chunk_pack
            .as_deref()
            .is_some_and(|t| t.contains("{index")));
        assert!(config.tiles.is_some());
    }

    #[test]
    fn a_project_needs_one_place_and_one_layout() {
        let both =
            "[[project]]\nname = \"a\"\nbucket = \"b\"\ndir = \"/d\"\nscenes = [\"packs\"]\n";
        assert!(Config::parse(both).is_err());
        let neither = "[[project]]\nname = \"a\"\nbucket = \"b\"\n";
        assert!(Config::parse(neither).is_err());
        let unknown =
            "[[project]]\nname = \"a\"\nbucket = \"b\"\nscenes = [\"packs\"]\ntapes = true\n";
        assert!(
            Config::parse(unknown).is_err(),
            "an unknown key is a mistake, not a default"
        );
        let twice = "[[project]]\nname = \"a\"\ndir = \"/x\"\nscenes = [\"p\"]\n[[project]]\nname = \"a\"\ndir = \"/y\"\nscenes = [\"p\"]\n";
        assert!(Config::parse(twice).is_err());
        assert!(Config::parse("").is_err());
    }

    #[test]
    fn a_tile_store_served_by_someone_else_is_named_by_its_address() {
        let project = "[[project]]\nname = \"p\"\nbucket = \"b\"\nscenes = [\"packs\"]\n";
        let tiles = |body: &str| Config::parse(&format!("{project}[tiles]\n{body}")).map(|c| c.tiles);
        assert_eq!(
            tiles("remote = \"https://tiles.example/root/\"\nheader = \"X-Token\""),
            Ok(Some(Place::Remote {
                url: "https://tiles.example/root".into(),
                header: Some("X-Token".into()),
            }))
        );
        assert_eq!(
            tiles("remote = \"http://127.0.0.1:4010/v1/tiles\""),
            Ok(Some(Place::Remote {
                url: "http://127.0.0.1:4010/v1/tiles".into(),
                header: None,
            }))
        );
        // One place, and a header only where there is somebody to send it to.
        assert!(tiles("remote = \"https://t.example\"\nbucket = \"b\"").is_err());
        assert!(tiles("bucket = \"b\"\nheader = \"X-Token\"").is_err());
        assert!(tiles("remote = \"t.example\"").is_err());
        assert!(tiles("remote = \"https://t.example\"\nheader = \"X Token\"").is_err());
    }
}
