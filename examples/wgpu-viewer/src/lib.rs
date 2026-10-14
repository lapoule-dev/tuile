// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer, over the public connectors — the host every platform's build
//! runs: `main.rs` on a desktop and a phone, `android_main` on Android (named, not
//! linked: like `DEVELOPMENT_CATALOG` below, it exists only in an Android build).
//!
//! The application itself — window, camera, controls, steering, scripting — is
//! the `tuile-viewer` library. This crate is the one decision a host makes:
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
//! One layer is not ion's: `--imagery ortho-fr` drapes France's open
//! orthophotography, fetched from a public URL template with no key (see
//! [`ORTHO_FR_TEMPLATE`]). The terrain under it is still ion's.
//!
//! ```text
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer -- --imagery ortho-fr \
//!     --lon 0.59 --lat 42.79 --altitude 2500 --heading 180 --pitch 30
//! ```
//!
//! The access token is read from the environment, or — for a session started
//! from an icon — from one file the person owns:
//! `~/Library/Application Support/Tuile/token` on macOS.
//!
//! **On Android there is no token at all.** The catalog is asked through the
//! catalog proxy (`tuile-catalog-proxy`), which holds the token on a server:
//! an APK carries nothing that would let anyone who unpacks it use the
//! account. The proxy's address is `TUILE_CATALOG_URL` — a launch setting,
//! else the value it had when the library was built, else
//! `DEVELOPMENT_CATALOG`, which `scripts/try-viewer.sh android` serves.
//!
//! **Headless, on demand.** `examples/headless` plays the graphics budget's
//! motion patterns on this application without a window, over the real
//! catalog, tile store and clock, and holds each run to the specification's
//! obligations. It needs the network, a GPU and the token, read from
//! `TUILE_ION_TOKEN` by a catalog proxy served in its own process — the
//! application never sees it, and it is never printed:
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example headless -- run all
//! ```
//!
//! Session logs (MCAP) and a JSON summary go to `target/headless/`; the
//! runner exits 1 when an obligation is violated. See the example's own
//! documentation for the patterns and the flags.

use std::sync::Arc;

use tuile_cesium_ion::IonSources;
use tuile_core::fetch::TileFetcher;
use tuile_core::raster::{ImageryProvider, TemplateProvider, TilingScheme};
use tuile_native_fetchers::{default_cache_dir, CacheConfig, NativeHttp};
use tuile_terrain::{LayerJson, TerrainSource};
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

/// The layer served from a URL template rather than by ion. A negative number,
/// which no ion asset has, so [`WithOrthoFr`] can tell the two apart.
const ORTHO_FR: i64 = -1;

/// France's open orthophotography — IGN BD ORTHO, about 20 cm, under the
/// Etalab 2.0 licence — from the public tile service of the Géoplateforme:
/// WMTS key-value requests on the web-mercator matrix set `PM`, standard
/// `z/x/y` numbering, 256-pixel JPEG, no key.
///
/// Levels 0 to 12 are a world mosaic; 13 to 19 cover France only, and
/// elsewhere the service answers 404. The drape reads a 404 as "absent here"
/// and stands on the parent tile, so far from France the ground goes soft —
/// down to the level-12 mosaic — and never black. Near the border it is
/// worse than soft: a tile that straddles it is served whole, with the part
/// outside France filled white, and the tiles beyond stand on that white.
/// Nothing is served past level 19.
const ORTHO_FR_TEMPLATE: &str = "https://data.geopf.fr/wmts?SERVICE=WMTS&REQUEST=GetTile\
    &VERSION=1.0.0&LAYER=ORTHOIMAGERY.ORTHOPHOTOS&STYLE=normal&TILEMATRIXSET=PM\
    &TILEMATRIX={z}&TILEROW={y}&TILECOL={x}&FORMAT=image/jpeg";

/// The deepest level [`ORTHO_FR_TEMPLATE`] serves.
const ORTHO_FR_DEEPEST: u32 = 19;

/// What the licence asks to be shown with the pictures.
const ORTHO_FR_ATTRIBUTION: &str = "© IGN – BD ORTHO, Etalab 2.0";

/// The open French orthophotography, over any transport.
fn ortho_fr<F: TileFetcher>(http: Arc<F>) -> TemplateProvider<F> {
    TemplateProvider::new(
        ORTHO_FR_TEMPLATE,
        http,
        TilingScheme {
            maximum_level: ORTHO_FR_DEEPEST,
            ..TilingScheme::web_mercator()
        },
    )
}

/// The catalog's sources, with the French orthophotography beside them:
/// [`ORTHO_FR`] is fetched from its template, every other asset is asked of
/// the catalog.
struct WithOrthoFr {
    catalog: Arc<dyn Sources>,
    http: Arc<NativeHttp>,
}

#[async_trait::async_trait]
impl Sources for WithOrthoFr {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        self.catalog.terrain(asset).await
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        if asset == ORTHO_FR {
            return Ok(Arc::new(ortho_fr(Arc::clone(&self.http))));
        }
        self.catalog.imagery(asset).await
    }
}

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
                "No token on this device: the catalog is asked through a proxy that holds it \
                 (TUILE_CATALOG_URL)."
                    .to_owned()
            } else {
                format!(
                    "The access token is read from {TOKEN_VARIABLE}, or from the file `token` \
                     in the application's support directory."
                )
            },
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
            // France only, and sharper than any of the three over it; soft
            // ground everywhere else. Fetched without ion — see the template.
            ImageryChoice::new(
                "ortho-fr",
                "Orthophoto France",
                ORTHO_FR,
                ORTHO_FR_ATTRIBUTION,
            ),
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
        #[cfg(target_os = "android")]
        let catalog = {
            let catalog = catalog_url()?;
            tracing::info!(%catalog, "catalog proxy");
            through_catalog(Arc::clone(&http), catalog)
        };
        #[cfg(not(target_os = "android"))]
        let catalog: Arc<dyn Sources> = {
            let token = tuile_viewer::credential(TOKEN_VARIABLE).map_err(|e| e.to_string())?;
            Arc::new(IonSources::new(Arc::clone(&http), Arc::clone(&http), token))
        };
        Ok(Arc::new(WithOrthoFr { catalog, http }))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::fetch::FsFetcher;
    use tuile_core::raster::ImageryCoord;

    /// The very request checked by hand against the service: one level-19
    /// tile over Luchon, which answered with a 256-pixel JPEG. Column and row
    /// swapped, or the wrong matrix set, and it answers 404 — which the drape
    /// would quietly read as "no picture here" everywhere.
    #[test]
    fn the_french_ortho_layer_asks_for_the_tile_the_service_serves() {
        let provider = ortho_fr(Arc::new(FsFetcher));
        let url = provider
            .tile_url(ImageryCoord {
                level: 19,
                x: 263_008,
                y: 193_067,
            })
            .expect("a well-formed url");
        assert_eq!(
            url.as_str(),
            "https://data.geopf.fr/wmts?SERVICE=WMTS&REQUEST=GetTile&VERSION=1.0.0\
             &LAYER=ORTHOIMAGERY.ORTHOPHOTOS&STYLE=normal&TILEMATRIXSET=PM\
             &TILEMATRIX=19&TILEROW=193067&TILECOL=263008&FORMAT=image/jpeg"
        );
        assert_eq!(provider.tiling_scheme().maximum_level, 19);
    }

    /// The layer is on offer, by the key the command line takes, with the
    /// attribution its licence asks for.
    #[test]
    fn the_french_ortho_layer_is_offered_with_its_attribution() {
        let layers = Public.imagery();
        let layer = layers
            .iter()
            .find(|l| l.key == "ortho-fr")
            .expect("offered");
        assert_eq!(layer.asset, ORTHO_FR);
        assert!(layer.attribution.contains("Etalab 2.0"));
        // Not the opening layer: a session elsewhere opens on photography
        // that covers it.
        assert_ne!(layers[0].key, "ortho-fr");
    }
}
