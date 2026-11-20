// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the globe's terrain and imagery come from.
//!
//! The same answers every other tuile consumer gives, as data the plugin is
//! built with: a terrain (a `layer.json` URL, a source the host already has —
//! a height field of its own, say — the bare ellipsoid, or an asset number of
//! the host's [`Sources`]) and an imagery (an XYZ template, a provider the
//! host already has, or an asset number). Nothing here knows a provider.

use std::sync::Arc;

use tuile_core::raster::{ImageryProvider, TilingScheme};
use tuile_planetary::Sources;
use tuile_terrain::{LayerJson, TerrainSource};

/// The ground's relief.
#[derive(Clone)]
pub enum TerrainSpec {
    /// Quantized-mesh terrain at a `layer.json` URL, fetched with no token.
    Url(url::Url),
    /// A source the host holds already, with its `layer.json`: a
    /// `tuile_terrain::HeightfieldSource` of its own map, a connector's.
    Source {
        layer: LayerJson,
        source: Arc<dyn TerrainSource>,
    },
    /// The WGS84 ellipsoid, flat: imagery with no relief.
    Ellipsoid,
    /// Asset `asset` of the host's [`Sources`].
    Hosted {
        sources: Arc<dyn Sources>,
        asset: i64,
    },
}

/// An imagery URL template: `{z}`, `{x}` and `{y}`, rows counted from the
/// north (XYZ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XyzTemplate {
    pub template: String,
    /// Cut on the geographic grid (two tiles at level 0) rather than
    /// web-mercator (one).
    pub geographic: bool,
    /// The deepest level the server has, when it says so.
    pub max_level: Option<u32>,
}

impl XyzTemplate {
    /// A web-mercator template.
    pub fn web_mercator(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            geographic: false,
            max_level: None,
        }
    }

    /// A template on the geographic grid.
    pub fn geographic(template: impl Into<String>) -> Self {
        Self {
            geographic: true,
            ..Self::web_mercator(template)
        }
    }

    /// Stops at `level`.
    #[must_use]
    pub fn with_max_level(mut self, level: u32) -> Self {
        self.max_level = Some(level);
        self
    }

    /// Whether the template names its three coordinates and is a URL once
    /// they are filled in.
    pub fn check(&self) -> Result<(), String> {
        for name in ["{z}", "{x}", "{y}"] {
            if !self.template.contains(name) {
                return Err(format!(
                    "the imagery template {:?} names no {name}",
                    self.template
                ));
            }
        }
        let probe = self
            .template
            .replace("{z}", "0")
            .replace("{x}", "0")
            .replace("{y}", "0");
        url::Url::parse(&probe)
            .map(|_| ())
            .map_err(|e| format!("the imagery template {:?}: {e}", self.template))
    }

    pub(crate) fn scheme(&self) -> TilingScheme {
        let mut scheme = if self.geographic {
            TilingScheme::geographic()
        } else {
            TilingScheme::web_mercator()
        };
        if let Some(deepest) = self.max_level {
            scheme.maximum_level = deepest;
        }
        scheme
    }
}

/// The pictures draped over it.
#[derive(Clone)]
pub enum ImagerySpec {
    /// An XYZ template, fetched with no token.
    Xyz(XyzTemplate),
    /// A provider the host holds already.
    Provider(Arc<dyn ImageryProvider>),
    /// Asset `asset` of the host's [`Sources`].
    Hosted {
        sources: Arc<dyn Sources>,
        asset: i64,
    },
}

/// What the globe is made of.
#[derive(Clone)]
pub struct GlobeSources {
    pub terrain: TerrainSpec,
    pub imagery: ImagerySpec,
    /// A substitute for the imagery's far tiles, and the fallback rule that
    /// says which: `None` (the default) substitutes nothing. See
    /// `docs/explorations/imagery-layer-substitution.md`.
    pub substitute: Option<Substitute>,
}

/// Another imagery the far tiles are draped from, and the rule that says
/// where: tiles at least `rule.gap` levels coarser than the finest drawn.
#[derive(Clone)]
pub struct Substitute {
    pub imagery: ImagerySpec,
    pub rule: tuile_core::budget::FallbackRule,
}

impl Substitute {
    /// `imagery` in the base layer's place, with the default rule.
    pub fn new(imagery: ImagerySpec) -> Self {
        Self {
            imagery,
            rule: tuile_core::budget::FallbackRule::new(tuile_core::budget::LayerId(0)),
        }
    }

    /// The fallback specification this substitute is drawn under.
    pub fn spec(&self) -> tuile_core::budget::FallbackSpec {
        tuile_core::budget::FallbackSpec::one(self.rule)
    }
}

impl GlobeSources {
    /// Terrain at a `layer.json` URL, imagery from a template.
    pub fn addresses(terrain: url::Url, imagery: XyzTemplate) -> Self {
        Self {
            terrain: TerrainSpec::Url(terrain),
            imagery: ImagerySpec::Xyz(imagery),
            substitute: None,
        }
    }
}

/// A terrain and an imagery, resolved: what `tuile_planetary::globe_on`
/// crosses.
pub(crate) struct Resolved {
    pub layer: LayerJson,
    pub terrain: Arc<dyn TerrainSource>,
    pub imagery: Arc<dyn ImageryProvider>,
    /// The substitute's provider, resolved like the imagery.
    pub substitute: Option<Arc<dyn ImageryProvider>>,
}

/// The transport an address is fetched through: the native HTTP stack, or a
/// browser's.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Http = tuile_native_fetchers::NativeHttp;

#[cfg(target_arch = "wasm32")]
pub(crate) type Http = tuile_web::WebFetcher;

#[cfg(not(target_arch = "wasm32"))]
async fn http() -> Result<Arc<Http>, String> {
    Http::shared()
        .await
        .map(Arc::new)
        .map_err(|e| format!("the HTTP client: {e}"))
}

/// The page's `fetch`: nothing to open.
#[cfg(target_arch = "wasm32")]
async fn http() -> Result<Arc<Http>, String> {
    Ok(Arc::new(tuile_web::WebFetcher))
}

impl GlobeSources {
    /// Resolves both halves: an address is opened, a hosted asset asked for.
    pub(crate) async fn resolve(&self) -> Result<Resolved, String> {
        let needs_http = matches!(self.terrain, TerrainSpec::Url(_))
            || matches!(self.imagery, ImagerySpec::Xyz(_))
            || matches!(
                self.substitute.as_ref().map(|s| &s.imagery),
                Some(ImagerySpec::Xyz(_))
            );
        let http = if needs_http {
            Some(http().await?)
        } else {
            None
        };
        let (layer, terrain) = match &self.terrain {
            TerrainSpec::Url(url) => {
                let http = http.clone().ok_or("no HTTP client")?;
                let source = tuile_terrain::HttpTerrainSource::open(http, url.clone())
                    .await
                    .map_err(|e| format!("the terrain at {url}: {e}"))?;
                let layer = source.layer().clone();
                (layer, Arc::new(source) as Arc<dyn TerrainSource>)
            }
            TerrainSpec::Source { layer, source } => (layer.clone(), Arc::clone(source)),
            TerrainSpec::Ellipsoid => (
                tuile_terrain::Ellipsoid::layer(),
                Arc::new(tuile_terrain::Ellipsoid) as Arc<dyn TerrainSource>,
            ),
            TerrainSpec::Hosted { sources, asset } => sources
                .terrain(*asset)
                .await
                .map_err(|why| format!("terrain {asset}: {why}"))?,
        };
        let imagery = imagery_of(&self.imagery, http.clone()).await?;
        let substitute = match &self.substitute {
            Some(s) => Some(imagery_of(&s.imagery, http).await?),
            None => None,
        };
        Ok(Resolved {
            layer,
            terrain,
            imagery,
            substitute,
        })
    }
}

/// An imagery spec, resolved: a template opened over `http`, a provider as
/// it is, a hosted asset asked for.
async fn imagery_of(
    spec: &ImagerySpec,
    http: Option<Arc<Http>>,
) -> Result<Arc<dyn ImageryProvider>, String> {
    Ok(match spec {
        ImagerySpec::Xyz(template) => {
            template.check()?;
            let http = http.ok_or("no HTTP client")?;
            Arc::new(tuile_core::raster::TemplateProvider::new(
                template.template.clone(),
                http,
                template.scheme(),
            ))
        }
        ImagerySpec::Provider(provider) => Arc::clone(provider),
        ImagerySpec::Hosted { sources, asset } => sources
            .imagery(*asset)
            .await
            .map_err(|why| format!("imagery {asset}: {why}"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A template is checked before anything is fetched: the three names,
    /// and a URL once they are filled in.
    #[test]
    fn a_template_names_its_three_coordinates() {
        assert!(XyzTemplate::web_mercator("http://h/{z}/{x}/{y}.png")
            .check()
            .is_ok());
        let missing = XyzTemplate::web_mercator("http://h/{z}/{x}.png")
            .check()
            .unwrap_err();
        assert!(missing.contains("{y}"), "{missing}");
        assert!(XyzTemplate::web_mercator("h/{z}/{x}/{y}.png")
            .check()
            .is_err());
    }

    /// The grid a template is cut on is the template's to say, and so is its
    /// deepest level.
    #[test]
    fn a_template_says_its_grid() {
        let geo = XyzTemplate::geographic("http://h/{z}/{x}/{y}.png").with_max_level(15);
        let scheme = geo.scheme();
        assert_eq!(scheme.maximum_level, 15);
        assert_eq!(scheme, {
            let mut s = TilingScheme::geographic();
            s.maximum_level = 15;
            s
        });
        assert_ne!(
            XyzTemplate::web_mercator("http://h/{z}/{x}/{y}.png").scheme(),
            scheme
        );
    }
}
