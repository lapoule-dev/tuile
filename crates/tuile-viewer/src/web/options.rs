// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a page asks for when it creates a viewer, read into typed Rust.
//!
//! The package's `createViewer` normalises the host's options — resolves
//! `profile: 'auto'`, reads the device's memory, picks up the token — and
//! hands this module a plain object. Everything a host can get wrong is said
//! here, in one sentence, before a worker is started or a byte fetched.

use serde::{Deserialize, Serialize};

use crate::embed::{ImageryChoice, TerrainChoice};
use crate::start::StartView;

/// The options, as the package hands them over.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Options {
    /// The application's name, for messages: the host's, never ours.
    #[serde(default = "default_name")]
    pub name: String,
    pub sources: SourcesSpec,
    /// `desktop` or `handheld`; the package resolves `auto`.
    pub profile: ProfileChoice,
    /// The device's memory in gibibytes, when the browser says.
    #[serde(default)]
    pub device_memory: Option<f64>,
    #[serde(default)]
    pub backend: BackendChoice,
    #[serde(default)]
    pub view: Option<ViewSpec>,
    /// The highest the eye may go, metres above the ellipsoid — as
    /// `--max-altitude`; the camera's own ceiling when left out.
    #[serde(default)]
    pub max_altitude: Option<f64>,
    /// Whether the on-screen controls and the menu are drawn.
    #[serde(default = "yes")]
    pub ui: bool,
    /// Which bottom corner the caption — whose pictures these are — keeps to.
    #[serde(default)]
    pub attribution: CaptionSide,
    /// Whether a viewer that cannot draw says so in its container.
    #[serde(default = "yes")]
    pub show_unsupported_message: bool,
    /// Whether a viewer opened without a `view` opens where the last one
    /// ended, as a desktop's application does.
    #[serde(default = "yes")]
    pub remember_view: bool,
    /// The most Web Workers that decode imagery and terrain beside the
    /// geometry server's: one per core less one, up to this. 0 decodes on the
    /// geometry server's own thread.
    #[serde(default)]
    pub decode_workers: Option<u32>,
    /// How imagery is held on the GPU: `off` (the default), `auto`, `bc7` or
    /// `astc` — the host's choice, resolved against the page's own adapter.
    /// See [`tuile_wgpu::ImageryCompression`].
    #[serde(default, deserialize_with = "compression")]
    pub imagery_compression: tuile_wgpu::ImageryCompression,
    /// The instant the scene is lit for: `now` (the default), `sunset`,
    /// `sunrise` or an RFC 3339 instant — as the `--time` flag.
    #[serde(default)]
    pub time: Option<String>,
    /// Added to it: `-3h30m`, `+45m` — as `--time-offset`.
    #[serde(default)]
    pub time_offset: Option<String>,
    /// Scene seconds a second: 1 by default — as `--time-rate`.
    #[serde(default)]
    pub time_rate: Option<f64>,
    /// The 3D Tiles tilesets on offer over the globe, by the catalog's asset
    /// number: each a switch in the menu, off until the person turns it on —
    /// or on from the start with `on: true`, as `--tileset` is.
    #[serde(default)]
    pub tilesets: Vec<TilesetSpec>,
}

/// A 3D Tiles tileset on offer: `{ asset: 2275207, name: "…", on: false }`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TilesetSpec {
    pub asset: i64,
    /// What the menu calls it; `3D Tiles <asset>` when left out.
    #[serde(default)]
    pub name: Option<String>,
    /// On from the start, whatever the last session left.
    #[serde(default)]
    pub on: bool,
}

impl TilesetSpec {
    /// The choices on offer, and those on at the start: the ones the page
    /// says are, else the ones `kept` (the last session's) that are on offer.
    pub(crate) fn choose(
        specs: &[TilesetSpec],
        kept: &[i64],
    ) -> (Vec<crate::embed::TilesetChoice>, Vec<i64>) {
        let offered: Vec<crate::embed::TilesetChoice> = specs
            .iter()
            .map(|t| {
                let name = t
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("3D Tiles {}", t.asset));
                crate::embed::TilesetChoice::new(&name, t.asset)
            })
            .collect();
        let asked: Vec<i64> = specs.iter().filter(|t| t.on).map(|t| t.asset).collect();
        let on = if asked.is_empty() {
            kept.iter()
                .copied()
                .filter(|a| specs.iter().any(|t| t.asset == *a))
                .collect()
        } else {
            asked
        };
        (offered, on)
    }
}

impl Options {
    /// The camera's ceiling these options state, held to the bounds an
    /// altitude is, or the sentence that says it is out of them.
    pub(crate) fn max_altitude(&self) -> Result<f64, String> {
        self.max_altitude
            .map(|m| {
                crate::start::checked("altitude", m)
                    .map_err(|why| why.replace("--altitude", "maxAltitude"))
            })
            .unwrap_or_else(|| Ok(tuile_camera::default_max_altitude()))
    }

    /// The scene's clock these options state, or the sentence that says
    /// which of them is wrong.
    pub(crate) fn clock(&self) -> Result<tuile_atmosphere::ClockSettings, String> {
        let rate = self.time_rate.map(|r| r.to_string());
        tuile_atmosphere::ClockSettings::parse(
            self.time.as_deref(),
            self.time_offset.as_deref(),
            rate.as_deref(),
        )
    }
}

/// A compression named as the flag names it: `off`, `auto`, `bc7`, `astc`.
fn compression<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<tuile_wgpu::ImageryCompression, D::Error> {
    let name = String::deserialize(d)?;
    tuile_wgpu::compression_named(&name).map_err(serde::de::Error::custom)
}

/// Which bottom corner the caption keeps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CaptionSide {
    BottomLeft,
    #[default]
    BottomRight,
}

fn default_name() -> String {
    "Globe".to_owned()
}

fn yes() -> bool {
    true
}

/// Which numbers the session runs with: see `Profile::browser`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProfileChoice {
    Desktop,
    Handheld,
}

/// Which graphics backend to draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BackendChoice {
    /// WebGPU where the browser has it, WebGL2 otherwise.
    #[default]
    Auto,
    Webgpu,
    Webgl2,
}

impl BackendChoice {
    /// The backends the instance may choose from.
    pub(crate) fn backends(self) -> wgpu::Backends {
        match self {
            Self::Auto => wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            Self::Webgpu => wgpu::Backends::BROWSER_WEBGPU,
            Self::Webgl2 => wgpu::Backends::GL,
        }
    }
}

/// Where the eye opens: degrees and metres, as the start flags speak.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewSpec {
    pub lon: f64,
    pub lat: f64,
    pub altitude: f64,
    #[serde(default)]
    pub heading: f64,
    #[serde(default = "straight_down")]
    pub pitch: f64,
}

fn straight_down() -> f64 {
    90.0
}

impl ViewSpec {
    /// The view, held to the same bounds as the flags.
    pub(crate) fn view(self) -> Result<StartView, String> {
        Ok(StartView {
            lon: crate::start::checked("lon", self.lon)?,
            lat: crate::start::checked("lat", self.lat)?,
            altitude: crate::start::checked("altitude", self.altitude)?,
            heading: crate::start::checked("heading", self.heading)?,
            pitch: crate::start::checked("pitch", self.pitch)?,
        })
    }
}

/// Where terrain and imagery come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourcesSpec {
    /// The catalog that resolves asset numbers to endpoints: the address of
    /// the proxy that holds its token. A page never holds one. Required only
    /// when some source is named by an asset number.
    #[serde(default)]
    pub catalog: Option<CatalogSpec>,
    pub terrain: TerrainSpec,
    pub imagery: Vec<LayerSpec>,
}

/// The catalog: the proxy that answers for it and holds its token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogSpec {
    pub url: String,
}

/// The terrain: an asset of the catalog, by number, or quantized-mesh terrain
/// at the URL of its `layer.json` — `{ "asset": 1 }` or
/// `{ "url": "https://…/layer.json" }`, one or the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "TerrainFields", into = "TerrainFields")]
pub(crate) enum TerrainSpec {
    Asset(i64),
    Url(String),
}

/// The terrain's object as the page writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerrainFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    asset: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

impl TryFrom<TerrainFields> for TerrainSpec {
    type Error = String;

    fn try_from(fields: TerrainFields) -> Result<Self, String> {
        match (fields.asset, fields.url) {
            (Some(asset), None) => Ok(Self::Asset(asset)),
            (None, Some(url)) => Ok(Self::Url(url)),
            (Some(_), Some(_)) => {
                Err("`sources.terrain` gives both an `asset` and a `url`: give one".to_owned())
            }
            (None, None) => Err(
                "`sources.terrain` needs a catalog `asset` or the `url` of a layer.json".to_owned(),
            ),
        }
    }
}

impl From<TerrainSpec> for TerrainFields {
    fn from(spec: TerrainSpec) -> Self {
        match spec {
            TerrainSpec::Asset(asset) => Self {
                asset: Some(asset),
                url: None,
            },
            TerrainSpec::Url(url) => Self {
                asset: None,
                url: Some(url),
            },
        }
    }
}

/// One imagery layer: from the catalog by asset number, or from a URL
/// template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LayerSpec {
    pub key: String,
    pub name: String,
    /// Whose pictures these are: kept on screen while the layer is.
    pub attribution: String,
    #[serde(default)]
    pub asset: Option<i64>,
    /// `https://…/{z}/{x}/{y}.png`.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub scheme: Option<Grid>,
    #[serde(default)]
    pub max_level: Option<u32>,
}

/// The grid a template's tiles are cut on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Grid {
    Webmercator,
    Geographic,
}

impl SourcesSpec {
    /// The terrain, and the layers as the application lists them.
    ///
    /// A template layer has no catalog number, so it is given one the catalog
    /// never uses — negative, by its place in the list — which is what the
    /// worker's sources answer it by.
    ///
    /// The catalog is asked for only when some source is named by its number:
    /// a terrain at a URL under template layers needs none.
    pub(crate) fn choices(&self) -> Result<(TerrainChoice, Vec<ImageryChoice>), String> {
        if self.needs_catalog() {
            self.catalog_base()?;
        }
        if self.imagery.is_empty() {
            return Err("`sources.imagery` lists no layer".to_owned());
        }
        let mut layers = Vec::with_capacity(self.imagery.len());
        for (index, layer) in self.imagery.iter().enumerate() {
            let asset = match (&layer.asset, &layer.template) {
                (Some(asset), None) if *asset >= 0 => *asset,
                (None, Some(template)) => {
                    crate::open::check_template(&format!("imagery {:?}:", layer.key), template)?;
                    template_asset(index)
                }
                _ => {
                    return Err(format!(
                        "imagery {:?}: give either a catalog `asset` or a `template`",
                        layer.key
                    ))
                }
            };
            if layers.iter().any(|l: &ImageryChoice| l.key == layer.key) {
                return Err(format!("imagery {:?} is listed twice", layer.key));
            }
            layers.push(ImageryChoice::new(
                &layer.key,
                &layer.name,
                asset,
                &layer.attribution,
            ));
        }
        let terrain = match &self.terrain {
            TerrainSpec::Asset(asset) => TerrainChoice {
                asset: *asset,
                cache: format!("terrain-{asset}"),
            },
            TerrainSpec::Url(url) => {
                crate::open::absolute_url("`sources.terrain.url`", url)?;
                TerrainChoice::at_url(url)
            }
        };
        Ok((terrain, layers))
    }

    /// Whether some source is named by a catalog's asset number.
    pub(crate) fn needs_catalog(&self) -> bool {
        matches!(self.terrain, TerrainSpec::Asset(_))
            || self.imagery.iter().any(|l| l.asset.is_some())
    }

    /// The terrain's `layer.json`, when it is named by its address.
    pub(crate) fn terrain_url(&self) -> Option<&str> {
        match &self.terrain {
            TerrainSpec::Url(url) => Some(url),
            TerrainSpec::Asset(_) => None,
        }
    }

    /// The template layer an asset number stands for, if it is one.
    pub(crate) fn template_of(&self, asset: i64) -> Option<&LayerSpec> {
        let index = usize::try_from(-1 - asset).ok()?;
        self.imagery.get(index).filter(|l| l.template.is_some())
    }

    /// The catalog proxy's address, ending in `/` so that the catalog's
    /// paths are read under it.
    pub(crate) fn catalog_base(&self) -> Result<url::Url, String> {
        let Some(url) = self.catalog.as_ref().map(|c| c.url.as_str()) else {
            return Err("a source is named by a catalog asset number: \
                 `sources.catalog.url` is required (or name the terrain by `url` \
                 and the imagery by `template`)"
                .to_owned());
        };
        let mut base = url::Url::parse(url).map_err(|e| format!("catalog url {url:?}: {e}"))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(base)
    }
}

impl LayerSpec {
    /// The grid a template layer's tiles are cut on, as deep as the page
    /// says they go.
    pub(crate) fn tiling(&self) -> tuile_core::raster::TilingScheme {
        crate::open::template_tiling(self.scheme == Some(Grid::Geographic), self.max_level)
    }
}

/// The asset number of the template layer at `index`.
fn template_asset(index: usize) -> i64 {
    -1 - i64::try_from(index).unwrap_or(i64::MAX - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(json: &str) -> Result<Options, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    const CATALOG: &str = r#"{
        "name": "Atlas",
        "profile": "desktop",
        "sources": {
            "catalog": { "url": "https://tiles.test/catalog" },
            "terrain": { "asset": 1 },
            "imagery": [
                { "key": "aerial", "name": "Aerial", "attribution": "© survey", "asset": 2 },
                { "key": "streets", "name": "Streets", "attribution": "© maps",
                  "template": "https://tiles.test/{z}/{x}/{y}.png", "scheme": "webmercator" }
            ]
        }
    }"#;

    /// Catalog layers keep their numbers, template layers get numbers the
    /// catalog never uses, and the worker finds each template by its number.
    #[test]
    fn layers_from_a_catalog_and_from_templates() {
        let o = options(CATALOG).expect("valid");
        let (terrain, layers) = o.sources.choices().expect("valid");
        assert_eq!(terrain.asset, 1);
        assert_eq!(layers[0].asset, 2);
        assert!(layers[1].asset < 0);
        assert_eq!(layers[1].attribution, "© maps");
        let template = o.sources.template_of(layers[1].asset).expect("a template");
        assert_eq!(template.key, "streets");
        assert!(o.sources.template_of(layers[0].asset).is_none());
        assert!(o.sources.template_of(-99).is_none());
        assert!(o.ui, "the controls are on unless the page says otherwise");
        assert_eq!(o.attribution, CaptionSide::BottomRight);
        assert!(o.show_unsupported_message && o.remember_view);
        assert_eq!(o.backend, BackendChoice::Auto);
        assert_eq!(o.profile, ProfileChoice::Desktop);
        assert_eq!(
            o.imagery_compression,
            tuile_wgpu::ImageryCompression::Off,
            "compression is off unless the page asks"
        );
        assert_eq!(
            template.tiling(),
            tuile_core::raster::TilingScheme::web_mercator()
        );
        assert_eq!(
            o.sources.catalog_base().map(String::from).as_deref(),
            Ok("https://tiles.test/catalog/"),
            "a base the catalog's paths are read under"
        );
    }

    /// Every option the package documents reaches Rust as it was given.
    #[test]
    fn every_option_is_read() {
        let o = options(
            r#"{
            "name": "Atlas", "profile": "handheld", "deviceMemory": 4, "backend": "webgl2",
            "ui": false, "view": { "lon": 6.86, "lat": 45.83, "altitude": 6000 },
            "attribution": "bottom-left", "showUnsupportedMessage": false, "rememberView": false,
            "decodeWorkers": 0, "imageryCompression": "astc",
            "sources": {
                "catalog": { "url": "https://catalog.test/" },
                "terrain": { "asset": 7 },
                "imagery": [{ "key": "s", "name": "S", "attribution": "©", "template":
                    "https://t.test/{z}/{x}/{y}.jpg", "scheme": "geographic", "maxLevel": 12 }]
            }}"#,
        )
        .expect("valid");
        assert_eq!(o.name, "Atlas");
        assert_eq!(o.profile, ProfileChoice::Handheld);
        assert_eq!(o.device_memory, Some(4.0));
        assert_eq!(o.backend, BackendChoice::Webgl2);
        assert!(!o.ui);
        assert_eq!(o.attribution, CaptionSide::BottomLeft);
        assert!(!o.show_unsupported_message);
        assert!(!o.remember_view);
        assert_eq!(o.decode_workers, Some(0));
        assert_eq!(
            o.imagery_compression,
            tuile_wgpu::ImageryCompression::Format(tuile_core::texels::BlockFormat::Astc4x4)
        );
        let view = o.view.expect("a view").view().expect("in bounds");
        assert_eq!((view.heading, view.pitch), (0.0, 90.0));
        assert_eq!(
            String::from(o.sources.catalog_base().expect("a url")),
            "https://catalog.test/"
        );
        let tiling = o.sources.imagery[0].tiling();
        assert_eq!(
            tiling.projection,
            tuile_core::raster::TilingScheme::geographic().projection
        );
        assert_eq!(tiling.maximum_level, 12);
        assert_eq!(o.sources.imagery[0].scheme, Some(Grid::Geographic));
        assert_ne!(
            BackendChoice::Webgpu.backends(),
            BackendChoice::Webgl2.backends()
        );
    }

    /// Each compression the package documents reads as the flag's; anything
    /// else is refused, naming the four.
    /// The scene's clock, as the flags give it: the present in real time by
    /// default, or a start, an offset and a rate; a wrong one refused.
    #[test]
    fn the_max_altitude_is_an_altitude() {
        let with = |extra: &str| {
            options(&CATALOG.replacen(
                r#""name": "Atlas","#,
                &format!(r#""name": "Atlas", {extra}"#),
                1,
            ))
            .expect("valid")
            .max_altitude()
        };
        assert_eq!(
            options(CATALOG).expect("valid").max_altitude(),
            Ok(tuile_camera::default_max_altitude())
        );
        assert_eq!(with(r#""maxAltitude": 500000,"#), Ok(500_000.0));
        let why = with(r#""maxAltitude": 0,"#).expect_err("out of bounds");
        assert!(why.contains("maxAltitude"), "{why}");
    }

    #[test]
    fn the_clock_is_read_as_the_flags_read_it() {
        let with = |extra: &str| {
            options(&CATALOG.replacen(
                r#""name": "Atlas","#,
                &format!(r#""name": "Atlas", {extra}"#),
                1,
            ))
            .expect("valid")
            .clock()
        };
        let none = options(CATALOG).expect("valid").clock();
        assert_eq!(none, Ok(tuile_atmosphere::ClockSettings::default()));
        let set =
            with(r#""time": "sunset", "timeOffset": "-30m", "timeRate": 30,"#).expect("valid");
        assert_eq!(set.start, tuile_atmosphere::Start::Sunset);
        assert_eq!((set.offset, set.rate), (-1800.0, 30.0));
        assert!(with(r#""time": "teatime","#).is_err());
    }

    #[test]
    fn imagery_compression_is_named() {
        use tuile_core::texels::BlockFormat;
        use tuile_wgpu::ImageryCompression;
        let with = |value: &str| {
            options(&CATALOG.replacen(
                r#""name": "Atlas","#,
                &format!(r#""name": "Atlas", "imageryCompression": {value},"#),
                1,
            ))
        };
        for (value, wanted) in [
            (r#""off""#, ImageryCompression::Off),
            (r#""auto""#, ImageryCompression::Auto),
            (r#""bc7""#, ImageryCompression::Format(BlockFormat::Bc7)),
            (
                r#""ASTC""#,
                ImageryCompression::Format(BlockFormat::Astc4x4),
            ),
        ] {
            assert_eq!(
                with(value).expect(value).imagery_compression,
                wanted,
                "{value}"
            );
        }
        let refused = with(r#""etc2""#).expect_err("not a compression");
        assert!(refused.contains("off, auto, bc7, astc"), "{refused}");
        assert!(with("true").is_err());
    }

    /// What a host can get wrong is refused in a sentence that names it.
    #[test]
    fn a_wrong_source_is_named() {
        let mut o = options(CATALOG).expect("valid");
        o.sources.imagery[0].template = Some("https://x/{z}/{x}/{y}".into());
        assert!(o.sources.choices().unwrap_err().contains("\"aerial\""));
        o.sources.imagery[0].template = None;
        o.sources.imagery[1].template = Some("https://x/tile.png".into());
        assert!(o.sources.choices().unwrap_err().contains("{z}"));
        o.sources.imagery.clear();
        assert!(o.sources.choices().is_err());
        let mut o = options(CATALOG).expect("valid");
        o.sources.catalog = None;
        assert!(o.sources.choices().unwrap_err().contains("catalog"));
        let mut o = options(CATALOG).expect("valid");
        o.sources.imagery[1].key = "aerial".into();
        assert!(o.sources.choices().unwrap_err().contains("twice"));
        assert!(options(
            r#"{"profile":"desktop","sources":{"terrain":{"asset":1},"imagery":[]},"tokn":"x"}"#
        )
        .unwrap_err()
        .contains("tokn"));
    }

    /// A terrain named by its `layer.json` under template layers needs no
    /// catalog; the worker is handed the same spec it was given.
    #[test]
    fn a_terrain_at_a_url_needs_no_catalog() {
        let o = options(
            r#"{"profile":"desktop","sources":{
                "terrain": { "url": "http://127.0.0.1:8092/terrain/layer.json" },
                "imagery": [{ "key": "relief", "name": "Relief", "attribution": "synthetic",
                    "template": "http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png",
                    "scheme": "geographic", "maxLevel": 15 }]
            }}"#,
        )
        .expect("valid");
        assert!(!o.sources.needs_catalog());
        let (terrain, layers) = o.sources.choices().expect("no catalog is needed");
        assert_eq!(
            terrain,
            TerrainChoice::at_url("http://127.0.0.1:8092/terrain/layer.json")
        );
        assert_eq!(
            o.sources.terrain_url(),
            Some("http://127.0.0.1:8092/terrain/layer.json")
        );
        assert!(layers[0].asset < 0);
        assert_eq!(o.sources.imagery[0].tiling().maximum_level, 15);
        // What the page posts to the worker reads back as the same spec.
        let posted = serde_json::to_string(&o.sources).expect("serialisable");
        assert!(posted.contains(r#""terrain":{"url":"#), "{posted}");
        let read: SourcesSpec = serde_json::from_str(&posted).expect("readable");
        assert_eq!(read, o.sources);
    }

    /// An asset number without a catalog, a terrain with both or neither of
    /// its two names, and an address that is not one: each refused in a
    /// sentence that says what to give.
    #[test]
    fn a_wrong_terrain_is_named() {
        let asset_without_catalog = options(
            r#"{"profile":"desktop","sources":{"terrain":{"asset":1},
                "imagery":[{"key":"a","name":"A","attribution":"©",
                "template":"https://t.test/{z}/{x}/{y}.png"}]}}"#,
        )
        .expect("readable");
        let why = asset_without_catalog.sources.choices().unwrap_err();
        assert!(why.contains("`sources.catalog.url` is required"), "{why}");
        let imagery_asset_without_catalog = options(
            r#"{"profile":"desktop","sources":{"terrain":{"url":"https://t.test/layer.json"},
                "imagery":[{"key":"a","name":"A","attribution":"©","asset":2}]}}"#,
        )
        .expect("readable");
        assert!(imagery_asset_without_catalog
            .sources
            .choices()
            .unwrap_err()
            .contains("catalog"));
        let both = options(
            r#"{"profile":"desktop","sources":{"terrain":{"asset":1,"url":"https://t.test/layer.json"},"imagery":[]}}"#,
        )
        .unwrap_err();
        assert!(both.contains("give one"), "{both}");
        let neither =
            options(r#"{"profile":"desktop","sources":{"terrain":{},"imagery":[]}}"#).unwrap_err();
        assert!(neither.contains("layer.json"), "{neither}");
        let unknown = options(
            r#"{"profile":"desktop","sources":{"terrain":{"href":"https://t.test/layer.json"},"imagery":[]}}"#,
        )
        .unwrap_err();
        assert!(unknown.contains("href"), "{unknown}");
        let relative = options(
            r#"{"profile":"desktop","sources":{"terrain":{"url":"terrain/layer.json"},
                "imagery":[{"key":"a","name":"A","attribution":"©",
                "template":"https://t.test/{z}/{x}/{y}.png"}]}}"#,
        )
        .expect("readable");
        assert!(relative
            .sources
            .choices()
            .unwrap_err()
            .contains("not a URL"));
    }

    /// A view is held to the flags' bounds; the backend choice maps to the
    /// instance's backends.
    #[test]
    fn views_and_backends() {
        let view = ViewSpec {
            lon: 6.86,
            lat: 45.83,
            altitude: 6000.0,
            heading: 120.0,
            pitch: 25.0,
        };
        assert_eq!(view.view().expect("in bounds").lat, 45.83);
        assert!(ViewSpec { lat: 95.0, ..view }.view().is_err());
        assert_eq!(BackendChoice::Webgl2.backends(), wgpu::Backends::GL);
        assert!(BackendChoice::Auto
            .backends()
            .contains(wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL));
    }

    /// A page offers tilesets by asset number: off unless it says `on`, or
    /// the last session left them on — and none by default, the viewer as
    /// it was before tilesets existed.
    #[test]
    fn tilesets_are_offered_off_unless_on() {
        let o = options(CATALOG).expect("valid");
        assert!(o.tilesets.is_empty());
        let (offered, on) = TilesetSpec::choose(&o.tilesets, &[2275207]);
        assert!(
            offered.is_empty() && on.is_empty(),
            "nothing offered, nothing on"
        );

        let page = CATALOG.replacen(
            "\"profile\"",
            r#""tilesets": [{ "asset": 2275207, "name": "Photorealistic 3D Tiles" },
                            { "asset": 96188 }],
               "profile""#,
            1,
        );
        let o = options(&page).expect("valid");
        let (offered, on) = TilesetSpec::choose(&o.tilesets, &[]);
        let names: Vec<&str> = offered.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Photorealistic 3D Tiles", "3D Tiles 96188"]);
        assert!(on.is_empty(), "off by default");
        let (_, on) = TilesetSpec::choose(&o.tilesets, &[96188, 5]);
        assert_eq!(on, vec![96188], "the last session's, among those offered");

        let asked = page.replacen(
            "{ \"asset\": 96188 }",
            "{ \"asset\": 96188, \"on\": true }",
            1,
        );
        let o = options(&asked).expect("valid");
        assert_eq!(TilesetSpec::choose(&o.tilesets, &[2275207]).1, vec![96188]);
        assert!(
            options(&page.replacen("\"asset\": 96188", "\"asset\": 96188, \"x\": 1", 1)).is_err()
        );
    }
}
