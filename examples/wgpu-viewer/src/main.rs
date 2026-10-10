// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer, over the public connectors.
//!
//! The application itself — window, camera, controls, steering, scripting — is
//! the `tuile-viewer` library. This binary is the one decision a host makes:
//! **where terrain and imagery come from.** Here that is Cesium ion, with Bing
//! imagery behind it, over a native HTTP transport; the choice is made in this
//! file and nowhere deeper. Another host answers the same three questions with
//! its own service and reuses everything else — see
//! `docs/16-embedding-the-viewer.md`.
//!
//! ```text
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer -- \
//!     --lon 6.86 --lat 45.83 --altitude 6000 --heading 120 --pitch 25
//! ```
//!
//! The access token is read from the environment, or — for a session started
//! from an icon — from one file the person owns:
//! `~/Library/Application Support/Tuile/token` on macOS.

use std::sync::Arc;

use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_cesium_ion::tms::TmsImagery;
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource};
use tuile_core::raster::ImageryProvider;
use tuile_native_fetchers::NativeHttp;
use tuile_terrain::{LayerJson, TerrainSource};
use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};

/// The environment variable the access token is read from.
const TOKEN_VARIABLE: &str = "CESIUM_ION_TOKEN";

/// The ion asset of the terrain: Cesium World Terrain.
const WORLD_TERRAIN: i64 = 1;

/// Terrain and imagery by ion asset number, resolved on demand.
struct Ion {
    http: Arc<NativeHttp>,
    token: String,
}

impl Ion {
    fn client(&self) -> IonClient<NativeHttp> {
        IonClient::new(Arc::clone(&self.http), self.token.clone())
    }
}

#[async_trait::async_trait]
impl Sources for Ion {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        let asset = u64::try_from(asset).map_err(|_| format!("{asset} is not an asset"))?;
        let source = IonTerrainSource::new(self.client(), asset);
        let layer = source.layer().await.map_err(|e| e.to_string())?;
        Ok((layer, Arc::new(source)))
    }

    /// What the endpoint *is*, not what one hopes it is: an asset ion serves
    /// through Bing answers with a key and a map style, and one ion hosts
    /// itself answers with a tile map resource. The two are cut on different
    /// grids — web-mercator and geographic — and each provider says which.
    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        let id = u64::try_from(asset).map_err(|_| format!("{asset} is not an asset"))?;
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
                    BingImageryProvider::from_metadata_url(Arc::clone(&self.http), &metadata)
                        .await
                        .map_err(|e| e.to_string())?,
                ))
            }
            Some(other) => Err(format!("asset {asset} is served as {other:?}, which is not handled")),
        }
    }
}

/// The public host: the project's own application over ion.
struct Public;

#[async_trait::async_trait]
impl ViewerHost for Public {
    fn identity(&self) -> Identity {
        tuile_viewer::renamed_by_environment(Identity {
            // The store this viewer has always written, so that a session
            // after this change starts as warm as the one before it.
            store: tuile_bing::cache_name("Aerial"),
            credentials: format!(
                "The access token is read from {TOKEN_VARIABLE}, or from the file `token` \
                 in the application's support directory."
            ),
            ..Identity::default()
        })
    }

    fn terrain(&self) -> TerrainChoice {
        TerrainChoice {
            asset: WORLD_TERRAIN,
            cache: "ion-cwt".into(),
        }
    }

    fn imagery(&self) -> Vec<ImageryChoice> {
        vec![
            // Asset 2, under the namespace its tiles have always been stored in.
            ImageryChoice::new("aerial", "Aerial", 2, "© Microsoft, © Maxar, © Earthstar Geographics")
                .cached_as(&tuile_bing::cache_namespace("Aerial")),
        ]
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        let token = tuile_viewer::credential(TOKEN_VARIABLE).map_err(|e| e.to_string())?;
        // One pooled, cached native transport drives both ion and Bing.
        let http = Arc::new(NativeHttp::shared().await.map_err(|e| e.to_string())?);
        Ok(Arc::new(Ion { http, token }))
    }
}

fn main() {
    tuile_viewer::main(Public)
}
