// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a page's tiles come from: the catalog it named, and the URL
//! templates it listed — answered by asset number, as every host's are.
//!
//! The catalog is the connector's ([`tuile_cesium_ion::IonSources`]), over
//! the browser's `fetch`, asked through the proxy the page names — which holds
//! the catalog's token, so that the page never does (see
//! `tuile_cesium_ion::proxy`). A template layer is answered here, by the
//! number [`SourcesSpec::choices`] gave it, and so is a terrain named by
//! the URL of its `layer.json` ([`tuile_terrain::HttpTerrainSource`]): no
//! catalog and no token, the browser's `fetch` straight to the server, which
//! must answer with `Access-Control-Allow-Origin`.

use std::sync::Arc;

use async_trait::async_trait;
use tuile_cesium_ion::IonSources;
use tuile_core::raster::{ImageryProvider, TemplateProvider};
use tuile_planetary::Sources;
use tuile_terrain::{HttpTerrainSource, LayerJson, TerrainSource};
use tuile_web::{WebFetcher, WebIonHttp};

use super::options::SourcesSpec;

/// The page's sources, in the worker.
pub(crate) struct PageSources {
    /// Only when some source is named by an asset number.
    catalog: Option<IonSources<WebIonHttp, WebFetcher>>,
    spec: SourcesSpec,
}

impl PageSources {
    /// Over `spec`, through the catalog proxy it names, with no token.
    pub(crate) fn new(spec: SourcesSpec) -> Result<Self, String> {
        let catalog = if spec.needs_catalog() {
            Some(
                IonSources::new(Arc::new(WebIonHttp), Arc::new(WebFetcher), "")
                    .with_api_base(spec.catalog_base()?),
            )
        } else {
            None
        };
        Ok(Self { catalog, spec })
    }

    fn catalog(&self) -> Result<&IonSources<WebIonHttp, WebFetcher>, String> {
        self.catalog
            .as_ref()
            .ok_or_else(|| "this page names no catalog".to_owned())
    }
}

#[async_trait(?Send)]
impl Sources for PageSources {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        let Some(url) = self
            .spec
            .terrain_url()
            .filter(|_| asset == crate::open::URL_TERRAIN)
        else {
            return self.catalog()?.terrain(asset).await;
        };
        let url = crate::open::absolute_url("the terrain", url)?;
        let source = HttpTerrainSource::open(Arc::new(WebFetcher), url.clone())
            .await
            .map_err(|e| format!("the terrain at {url}: {e}"))?;
        Ok((source.layer().clone(), Arc::new(source)))
    }

    /// A 3D Tiles asset of the catalog, through its proxy: one ion points
    /// at on another service is answered by the proxy too, which holds that
    /// service's key.
    async fn tileset(&self, asset: i64) -> Result<tuile_planetary::TilesetSource, String> {
        self.catalog()?.tileset(asset).await
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        let Some(layer) = self.spec.template_of(asset) else {
            return self.catalog()?.imagery(asset).await;
        };
        let template = layer.template.clone().unwrap_or_default();
        Ok(Arc::new(TemplateProvider::new(
            template,
            Arc::new(WebFetcher),
            layer.tiling(),
        )))
    }
}
