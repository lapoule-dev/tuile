// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Cesium ion terrain source: resolves a TERRAIN asset endpoint, fetches its
//! `layer.json` (parsed by `tuile-terrain`, source-agnostic), and fetches
//! `.terrain` tiles with the asset-scoped bearer, the extensions query, and
//! token refresh on expiry.
//!
//! This is transport + auth only. Decoding the quantized-mesh bytes is the
//! caller's job (`tuile_terrain::decode`, which also gunzips).

use crate::{Attribution, IonClient, IonError, IonHttp};
use bytes::Bytes;
use futures_util::lock::Mutex;
use tuile_terrain::{LayerJson, TileCoord};

/// A terrain asset served by ion (e.g. Cesium World Terrain = asset 1).
pub struct IonTerrainSource<H: IonHttp> {
    client: IonClient<H>,
    asset_id: u64,
    state: Mutex<Option<State>>,
}

struct State {
    base_url: url::Url,
    token: String,
    layer: LayerJson,
    attributions: Vec<Attribution>,
}

impl<H: IonHttp> IonTerrainSource<H> {
    pub fn new(client: IonClient<H>, asset_id: u64) -> Self {
        Self {
            client,
            asset_id,
            state: Mutex::new(None),
        }
    }

    /// The parsed `layer.json` (resolves the endpoint and fetches it once).
    pub async fn layer(&self) -> Result<LayerJson, IonError> {
        self.ensure().await?;
        Ok(self
            .state
            .lock()
            .await
            .as_ref()
            .expect("ensured")
            .layer
            .clone())
    }

    /// Attributions to display for this terrain asset.
    pub async fn attributions(&self) -> Result<Vec<Attribution>, IonError> {
        self.ensure().await?;
        Ok(self
            .state
            .lock()
            .await
            .as_ref()
            .expect("ensured")
            .attributions
            .clone())
    }

    /// Fetches the raw bytes of a `.terrain` tile (still possibly gzipped;
    /// `tuile_terrain::decode` handles that). Refreshes the asset token once
    /// on 401/403.
    pub async fn fetch_tile(&self, coord: TileCoord) -> Result<Bytes, IonError> {
        self.ensure().await?;
        let (url, mut token) = {
            let guard = self.state.lock().await;
            let s = guard.as_ref().expect("ensured");
            (self.tile_url(s, coord)?, s.token.clone())
        };
        for attempt in 0..2 {
            let response = self.client.http().get(&url, Some(&token)).await?;
            match response.status {
                200 => return Ok(response.body),
                404 => return Err(IonError::Status { status: 404, url }),
                401 | 403 if attempt == 0 => token = self.refresh_token().await?,
                status => return Err(IonError::Status { status, url }),
            }
        }
        Err(IonError::Status { status: 401, url })
    }

    /// Builds the tile URL: `base + layer template`, with the extensions
    /// query the layer advertises appended.
    fn tile_url(&self, s: &State, coord: TileCoord) -> Result<url::Url, IonError> {
        let rel = s
            .layer
            .tile_url(coord)
            .ok_or_else(|| IonError::Transport("layer.json has no tile template".into()))?;
        let mut url = s.base_url.join(&rel)?;
        if let Some(ext) = s.layer.extensions_query() {
            url.query_pairs_mut().append_pair("extensions", &ext);
        }
        Ok(url)
    }

    /// Resolves the endpoint and fetches+parses `layer.json` once.
    async fn ensure(&self) -> Result<(), IonError> {
        if self.state.lock().await.is_some() {
            return Ok(());
        }
        let endpoint = self.client.terrain_endpoint(self.asset_id).await?;
        let layer_url = endpoint.url.join("layer.json")?;
        let response = self
            .client
            .http()
            .get(&layer_url, Some(&endpoint.access_token))
            .await?;
        if response.status != 200 {
            return Err(IonError::Status {
                status: response.status,
                url: layer_url,
            });
        }
        let layer = LayerJson::from_slice(&response.body)
            .map_err(|e| IonError::Transport(e.to_string()))?;
        *self.state.lock().await = Some(State {
            base_url: endpoint.url,
            token: endpoint.access_token,
            layer,
            attributions: endpoint.attributions,
        });
        Ok(())
    }

    /// Re-resolves the endpoint to get a fresh asset token (the layer.json
    /// stays put).
    async fn refresh_token(&self) -> Result<String, IonError> {
        let endpoint = self.client.terrain_endpoint(self.asset_id).await?;
        let mut guard = self.state.lock().await;
        if let Some(s) = guard.as_mut() {
            s.token = endpoint.access_token.clone();
        }
        Ok(endpoint.access_token)
    }
}

/// Exposes the ion terrain source through the backend-agnostic
/// [`tuile_terrain::TerrainSource`] seam, so `tuile-planetary` consumes it
/// without knowing about ion.
#[async_trait::async_trait]
impl<H: IonHttp> tuile_terrain::TerrainSource for IonTerrainSource<H> {
    async fn fetch_tile(
        &self,
        coord: TileCoord,
    ) -> Result<Vec<u8>, tuile_terrain::TerrainSourceError> {
        // Disambiguate from the trait method of the same name (inherent call).
        IonTerrainSource::fetch_tile(self, coord)
            .await
            .map(|b| b.to_vec())
            .map_err(|e| tuile_terrain::TerrainSourceError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::MockHttp;
    use std::sync::Arc;

    const ENDPOINT: &str = "https://api.cesium.com/v1/assets/1/endpoint";
    const BASE: &str = "https://assets.ion.cesium.com/1/CesiumWorldTerrain/v1.2/";

    fn endpoint_body(token: &str) -> String {
        format!(
            r#"{{ "type": "TERRAIN", "url": "{BASE}", "accessToken": "{token}",
                  "attributions": [{{ "html": "<span>USGS</span>", "collapsible": true }}] }}"#
        )
    }

    const LAYER: &str = r#"{
      "format": "quantized-mesh-1.0", "version": "1.2.0", "scheme": "tms",
      "projection": "EPSG:4326", "tiles": ["{z}/{x}/{y}.terrain?v={version}"],
      "maxzoom": 19, "extensions": ["watermask", "octvertexnormals"],
      "available": [[{ "startX": 0, "startY": 0, "endX": 1, "endY": 0 }]]
    }"#;

    #[test]
    fn fetches_layer_and_tile_with_bearer_and_extensions() {
        let http = Arc::new(MockHttp::default());
        http.push(ENDPOINT, 200, &endpoint_body("asset-tok"));
        http.push(&format!("{BASE}layer.json"), 200, LAYER);
        // The tile URL must carry v=1.2.0 (template) AND extensions (appended).
        let tile = format!("{BASE}9/541/386.terrain?v=1.2.0&extensions=octvertexnormals-watermask");
        http.push(&tile, 200, "QMTILE");

        let source = IonTerrainSource::new(IonClient::new(Arc::clone(&http), "account-tok"), 1);
        futures_executor::block_on(async {
            let layer = source.layer().await.expect("layer");
            assert!(layer.is_quantized_mesh());
            let bytes = source
                .fetch_tile(TileCoord::new(9, 541, 386))
                .await
                .expect("tile");
            assert_eq!(&bytes[..], b"QMTILE");
            assert_eq!(source.attributions().await.expect("attr").len(), 1);
        });

        let calls = http.calls();
        // Endpoint resolution uses the account token…
        assert_eq!(calls[0].1.as_deref(), Some("account-tok"));
        // …layer.json and tiles use the asset token.
        assert!(calls
            .iter()
            .skip(1)
            .all(|c| c.1.as_deref() == Some("asset-tok")));
    }

    #[test]
    fn refreshes_token_once_on_401() {
        let http = Arc::new(MockHttp::default());
        http.push(ENDPOINT, 200, &endpoint_body("expired"));
        http.push(ENDPOINT, 200, &endpoint_body("fresh"));
        http.push(&format!("{BASE}layer.json"), 200, LAYER);
        let tile = format!("{BASE}0/0/0.terrain?v=1.2.0&extensions=octvertexnormals-watermask");
        http.push(&tile, 401, "");
        http.push(&tile, 200, "OK");

        let source = IonTerrainSource::new(IonClient::new(Arc::clone(&http), "account-tok"), 1);
        let bytes = futures_executor::block_on(source.fetch_tile(TileCoord::new(0, 0, 0)))
            .expect("after refresh");
        assert_eq!(&bytes[..], b"OK");
        let tile_calls: Vec<_> = http.calls().into_iter().filter(|c| c.0 == tile).collect();
        assert_eq!(tile_calls.len(), 2);
        assert_eq!(tile_calls[0].1.as_deref(), Some("expired"));
        assert_eq!(tile_calls[1].1.as_deref(), Some("fresh"));
    }
}
