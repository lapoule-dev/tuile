// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! ion as a globe's [`Sources`]: terrain and imagery by asset number.
//!
//! The one place that knows what an ion asset *is* once resolved. An imagery
//! asset ion hosts itself answers with a tile map resource, cut on the
//! geographic grid; one it serves through Bing answers with a key and a map
//! style, cut on web-mercator. Each provider says which grid it is on, and the
//! crossing drapes either.
//!
//! Generic over the transport, so the desktop application (a pooled, cached
//! native client) and the browser (the page's `fetch`) resolve assets with the
//! same code: the two used to carry a copy each, and the browser's had already
//! lost the tile map resource case.

use std::sync::Arc;

use async_trait::async_trait;
use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_core::fetch::TileFetcher;
use tuile_core::raster::ImageryProvider;
use tuile_planetary::{Sources, TilesetSource};
use tuile_terrain::{LayerJson, TerrainSource};
use url::Url;

use crate::tms::TmsImagery;
use crate::{AssetEndpoint, IonClient, IonHttp, IonTerrainSource, IonTileFetcher};

/// Terrain and imagery by ion asset number, resolved on demand.
///
/// `H` carries the ion API, with its bearer; `F` fetches the third-party tiles
/// an asset points at, which carry no ion token. One native client can be
/// both.
pub struct IonSources<H: IonHttp, F: TileFetcher> {
    http: Arc<H>,
    fetcher: Arc<F>,
    token: String,
    api_base: Option<Url>,
}

impl<H: IonHttp, F: TileFetcher> IonSources<H, F> {
    /// Sources over the public ion API with `token`. The token is held and
    /// sent as a bearer; it is never logged.
    pub fn new(http: Arc<H>, fetcher: Arc<F>, token: impl Into<String>) -> Self {
        Self {
            http,
            fetcher,
            token: token.into(),
            api_base: None,
        }
    }

    /// The same, against another ion API base: a self-hosted ion, a test.
    #[must_use]
    pub fn with_api_base(mut self, base: Url) -> Self {
        self.api_base = Some(base);
        self
    }

    fn client(&self) -> IonClient<H> {
        let client = IonClient::new(Arc::clone(&self.http), self.token.clone());
        match &self.api_base {
            Some(base) => client.with_api_base(base.clone()),
            None => client,
        }
    }
}

fn asset_number(asset: i64) -> Result<u64, String> {
    u64::try_from(asset).map_err(|_| format!("{asset} is not an asset"))
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<H, F> Sources for IonSources<H, F>
where
    H: IonHttp + 'static,
    F: TileFetcher + 'static,
{
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        let source = IonTerrainSource::new(self.client(), asset_number(asset)?);
        let layer = source.layer().await.map_err(|e| e.to_string())?;
        Ok((layer, Arc::new(source)))
    }

    /// What the endpoint *is*, not what one hopes it is: see the module.
    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        let id = asset_number(asset)?;
        let ion = self.client();
        let endpoint = match ion.asset_endpoint(id).await.map_err(|e| e.to_string())? {
            AssetEndpoint::Imagery(endpoint) => endpoint,
            _ => return Err(format!("asset {asset} is not imagery")),
        };
        match endpoint.external_type.as_deref() {
            None => Ok(Arc::new(
                TmsImagery::from_endpoint(ion, id, endpoint)
                    .await
                    .map_err(|e| e.to_string())?,
            )),
            Some("BING") => {
                let o = &endpoint.options;
                let style = o.map_style.as_deref().unwrap_or("Aerial");
                let metadata = BingMetadata::metadata_url(
                    o.url.as_deref().ok_or("the endpoint names no URL")?,
                    style,
                    o.key.as_deref().ok_or("the endpoint carries no key")?,
                );
                Ok(Arc::new(
                    BingImageryProvider::from_metadata_url(Arc::clone(&self.fetcher), &metadata)
                        .await
                        .map_err(|e| e.to_string())?,
                ))
            }
            Some(other) => Err(format!(
                "asset {asset} is served as {other:?}, which is not handled"
            )),
        }
    }

    /// A 3D Tiles asset: ion's own, or one ion points at elsewhere — the
    /// endpoint says which, and the fetcher carries what each wants.
    async fn tileset(&self, asset: i64) -> Result<TilesetSource, String> {
        let fetcher = IonTileFetcher::new(self.client(), asset_number(asset)?);
        let url = fetcher.tileset_url().await.map_err(|e| e.to_string())?;
        let credits = fetcher
            .attributions()
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(crate::Attribution::credit)
            .collect();
        Ok(TilesetSource {
            url,
            fetcher: Arc::new(fetcher),
            credits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpResponse;
    use bytes::Bytes;
    use std::sync::Mutex;
    use tuile_core::fetch::FetchError;

    /// An ion API that answers every endpoint with `body`, and remembers
    /// which URLs and bearers it was asked with.
    struct Api {
        body: &'static str,
        asked: Mutex<Vec<(String, Option<String>)>>,
    }

    #[async_trait]
    impl IonHttp for Api {
        async fn get(
            &self,
            url: &Url,
            bearer: Option<&str>,
        ) -> Result<HttpResponse, crate::IonError> {
            self.asked
                .lock()
                .expect("asked")
                .push((url.to_string(), bearer.map(str::to_owned)));
            Ok(HttpResponse {
                status: 200,
                body: Bytes::from_static(self.body.as_bytes()),
                max_age: None,
            })
        }
    }

    /// Fetches nothing: the tests below never reach a tile.
    struct Nothing;

    #[async_trait]
    impl TileFetcher for Nothing {
        async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
            Err(FetchError::NotFound(url.clone()))
        }
    }

    fn sources(body: &'static str) -> (Arc<Api>, IonSources<Api, Nothing>) {
        let api = Arc::new(Api {
            body,
            asked: Mutex::new(Vec::new()),
        });
        let sources = IonSources::new(Arc::clone(&api), Arc::new(Nothing), "secret")
            .with_api_base(Url::parse("https://catalog.test/").expect("a url"));
        (api, sources)
    }

    /// An asset that is not imagery is refused in words, and the endpoint is
    /// asked on the configured base with the account token as the bearer.
    #[test]
    fn an_asset_that_is_not_imagery_is_refused() {
        let (api, sources) = sources(r#"{"type":"TERRAIN","url":"https://t.test/"}"#);
        let refused = futures_executor::block_on(sources.imagery(7))
            .err()
            .expect("terrain is not imagery");
        assert_eq!(refused, "asset 7 is not imagery");
        let asked = api.asked.lock().expect("asked");
        assert_eq!(asked[0].0, "https://catalog.test/v1/assets/7/endpoint");
        assert_eq!(asked[0].1.as_deref(), Some("secret"));
    }

    /// Imagery served by a service this connector does not handle is said
    /// to be so, by name.
    #[test]
    fn an_unknown_external_service_is_named() {
        let (_, sources) = sources(r#"{"type":"IMAGERY","externalType":"ELSEWHERE"}"#);
        let refused = futures_executor::block_on(sources.imagery(9))
            .err()
            .expect("not handled");
        assert!(refused.contains("\"ELSEWHERE\""), "{refused}");
    }

    /// A 3D Tiles asset ion points elsewhere answers with that root, the
    /// credits the endpoint lists, and a fetcher that sends no ion bearer
    /// there; an asset that is not 3D Tiles is refused in words.
    #[test]
    fn a_3d_tiles_asset_is_a_tileset_with_its_credits() {
        let (api, ion) = sources(include_str!("../fixtures/external-3dtiles-endpoint.json"));
        let tileset = futures_executor::block_on(ion.tileset(2_275_207)).expect("a tileset");
        assert_eq!(
            tileset.url.as_str(),
            "https://tile.googleapis.com/v1/3dtiles/root.json?key=FAKE_KEY"
        );
        let names: Vec<&str> = tileset.credits.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, ["Cesium ion", "Google"]);
        futures_executor::block_on(tileset.fetcher.fetch(&tileset.url)).expect("the root");
        let asked = api.asked.lock().expect("asked");
        assert_eq!(asked.last().map(|a| a.1.clone()), Some(None), "no bearer");
        drop(asked);
        let (_, terrain) = sources(r#"{"type":"TERRAIN","url":"https://t.test/"}"#);
        assert!(futures_executor::block_on(terrain.tileset(1)).is_err());
    }

    /// A negative number is not an asset, and nothing is asked.
    #[test]
    fn a_negative_asset_is_refused_before_any_request() {
        let (api, sources) = sources("{}");
        assert!(futures_executor::block_on(sources.imagery(-1)).is_err());
        assert!(api.asked.lock().expect("asked").is_empty());
    }
}
