// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

use std::sync::Arc;

use tuile_cesium_ion::IonSources;
use tuile_native_fetchers::{default_cache_dir, CacheConfig, NativeHttp};
use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};

/// The environment variable the access token is read from — on every
/// platform but Android, which holds none.
const TOKEN_VARIABLE: &str = "CESIUM_ION_TOKEN";

/// The ion asset of the terrain: Cesium World Terrain.
const WORLD_TERRAIN: i64 = 1;

/// The ion assets of the three imagery layers.
const AERIAL: i64 = 2;
const AERIAL_WITH_LABELS: i64 = 3;
const SATELLITE_MOSAIC: i64 = 3954;

/// Whose pictures the layers are: shown beside the layer's name for as long
/// as it is on screen.
const PHOTOGRAPHY: &str = "© Microsoft and its data suppliers";
const MOSAIC: &str = "Contains modified Copernicus Sentinel data";

/// The catalog proxy a development build asks, on the developer's machine:
/// `adb reverse tcp:8091 tcp:8091` makes it the device's own loopback, for an
/// emulator and a phone on a cable alike.
#[cfg(target_os = "android")]
pub const DEVELOPMENT_CATALOG: &str = "http://127.0.0.1:8091/catalog/";

/// The public host: the project's own application over ion.
pub struct Public;

#[async_trait::async_trait]
impl ViewerHost for Public {
    fn identity(&self) -> Identity {
        tuile_viewer::renamed_by_environment(Identity {
            // The store this viewer has always written, so that a session
            // after this change starts as warm as the one before it.
            store: tuile_bing::cache_name("Aerial"),
            credentials: if cfg!(target_os = "android") {
                "The access token is the one a configuration file opened in the application \
                 kept; without one, the catalog is asked through a proxy that holds it \
                 (TUILE_CATALOG_URL)."
                    .to_owned()
            } else {
                format!(
                    "The access token is read from {TOKEN_VARIABLE}, from --config's file, from \
                     a configuration file opened in the application, or from the file `token` \
                     in the application's support directory."
                )
            },
            // A configuration file's `[ion]` table: the token, kept under
            // the variable's name in the system's secure storage, the API
            // address, the terrain and the layers.
            credential_variable: TOKEN_VARIABLE.to_owned(),
            config_section: "ion".to_owned(),
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
        // The three the connector has. Neutral names: what a layer shows, not
        // who serves it — the attribution says that.
        vec![
            // Asset 2, under the namespace its tiles have always been stored
            // in, so a session after this change starts as warm as before.
            ImageryChoice::new("aerial", "Aerial", AERIAL, PHOTOGRAPHY)
                .cached_as(&tuile_bing::cache_namespace("Aerial")),
            // The same photography with roads and place names drawn over it.
            ImageryChoice::new(
                "labels",
                "Aerial with labels",
                AERIAL_WITH_LABELS,
                PHOTOGRAPHY,
            ),
            // One cloudless mosaic of the whole planet from a single
            // instrument: coarser, and the same colour everywhere. Served on
            // the geographic grid, where the two above are web-mercator.
            ImageryChoice::new("satellite", "Satellite", SATELLITE_MOSAIC, MOSAIC),
        ]
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        // One pooled, cached native transport drives both ion and Bing — with
        // a phone's smaller cache on a phone, where the viewer's own tile
        // store already holds the tiles.
        let http = if cfg!(any(target_os = "ios", target_os = "android")) {
            NativeHttp::with_config(CacheConfig::handheld(default_cache_dir())).await
        } else {
            NativeHttp::shared().await
        };
        let http = Arc::new(http.map_err(|e| e.to_string())?);
        // The API address a configuration file set, read at each
        // connection: an import takes effect at the next one.
        let api = tuile_viewer::embed::configured_api();
        #[cfg(target_os = "android")]
        {
            // A token a configuration file brought, kept in the Keystore;
            // without one, the catalog proxy that holds it.
            if let Ok(token) = tuile_viewer::credential(TOKEN_VARIABLE) {
                return Ok(ion(http, token, api));
            }
            let catalog = catalog_url()?;
            tracing::info!(%catalog, "catalog proxy");
            Ok(through_catalog(http, catalog))
        }
        #[cfg(not(target_os = "android"))]
        {
            let token = tuile_viewer::credential(TOKEN_VARIABLE).map_err(|e| e.to_string())?;
            Ok(ion(http, token, api))
        }
    }
}

/// The sources over the service, with `token`, at `api` when a
/// configuration file named one.
fn ion(http: Arc<NativeHttp>, token: String, api: Option<url::Url>) -> Arc<dyn Sources> {
    let sources = IonSources::new(Arc::clone(&http), http, token);
    Arc::new(match api {
        Some(api) => sources.with_api_base(api),
        None => sources,
    })
}

/// The sources through a catalog proxy that holds the token, at `catalog`
/// (ending in `/`): what a device that holds no token asks — Android, and a
/// headless run of this application, which serves the proxy beside it.
pub fn through_catalog(http: Arc<NativeHttp>, catalog: url::Url) -> Arc<dyn Sources> {
    Arc::new(IonSources::new(Arc::clone(&http), http, "").with_api_base(catalog))
}

/// The catalog proxy's address, ending in `/` so the catalog's paths are
/// read under it.
#[cfg(target_os = "android")]
fn catalog_url() -> Result<url::Url, String> {
    let given = std::env::var("TUILE_CATALOG_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| option_env!("TUILE_CATALOG_URL").map(str::to_owned))
        .unwrap_or_else(|| DEVELOPMENT_CATALOG.to_owned());
    let mut url = url::Url::parse(given.trim())
        .map_err(|e| format!("the catalog proxy's address {given:?}: {e}"))?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

/// The entry the system calls once the activity has started: on a thread of
/// its own, which becomes the application's main thread.
///
/// Exported unmangled because that is how `android-activity` finds it in the
/// library the activity loads — the one item of this crate that is.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[no_mangle]
fn android_main(app: tuile_viewer::android::AndroidApp) {
    tuile_viewer::android::main(app, Public)
}
