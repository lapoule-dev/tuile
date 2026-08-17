// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-bing
//!
//! Bing Maps imagery connector. Resolves the Bing metadata REST document to
//! get the tile URL template + subdomains, maps a tile to a quadkey, and
//! implements the core's [`tuile_core::raster::ImageryProvider`] so it drapes
//! onto terrain (or any geometry) like any other imagery source.
//!
//! Independent of ion: the Bing key may come from a `tuile-cesium-ion`
//! `ImageryEndpoint` (`options.key`) or directly. Tile fetching goes through
//! a [`tuile_core::fetch::TileFetcher`] — wasm-clean, no reqwest here.

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use std::sync::Arc;
use tuile_core::fetch::{FetchError, Fetched, TileFetcher};
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};

/// Metadata of a Bing imagery layer (the parts we need).
#[derive(Debug, Clone)]
pub struct BingMetadata {
    /// Template with `{subdomain}` and `{quadkey}` placeholders.
    pub image_url: String,
    pub subdomains: Vec<String>,
    pub tile_width: u32,
    pub tile_height: u32,
}

/// Which imagery style to ask Bing for, from `TUILE_IMAGERY`.
///
/// **The single place that decides.** It lived in the viewer, and the warm-up
/// job did not know about it: the job filled a cache under one name while the
/// viewer read another, so a warmed session was cold and nothing said so. Two
/// hosts cannot agree on a name by both being careful — they agree by calling
/// the same function.
///
/// `labels` is the one worth knowing about. Labelled imagery is not a prettier
/// picture, it is a **legible** one: aerial ground is smooth and a seam between
/// two tiles can be invisible, while a road that stops at a straight edge or a
/// town name cut in half says exactly where one tile ends and whether its
/// neighbour arrived.
///
/// | `TUILE_IMAGERY` | Bing style |
/// |---|---|
/// | *unset* | whatever the endpoint states, else `Aerial` |
/// | `labels` | `AerialWithLabelsOnDemand` |
/// | `aerial` | `Aerial` |
/// | `road` | `RoadOnDemand` |
/// | anything else | passed through unchanged |
pub fn map_style(from_endpoint: Option<&str>) -> String {
    let Ok(asked) = std::env::var("TUILE_IMAGERY") else {
        return from_endpoint.unwrap_or("Aerial").to_owned();
    };
    match asked.trim().to_ascii_lowercase().as_str() {
        "labels" | "labelled" | "labeled" => "AerialWithLabelsOnDemand",
        "aerial" => "Aerial",
        "road" | "roads" => "RoadOnDemand",
        _ => asked.trim(),
    }
    .to_owned()
}

/// The on-disk cache directory for a style.
///
/// A prefix rather than one shared bucket, because the store is keyed by tile
/// coordinate: aerial and labelled imagery for the same coordinate are the same
/// key. One cache would serve whichever arrived first — silently, and looking
/// exactly like a working session showing the wrong picture. Separate
/// directories also stop the two evicting each other while they are compared.
pub fn cache_name(style: &str) -> String {
    format!("tiles-{}", style.to_lowercase())
}

/// The namespace tiles are filed under *inside* a store.
///
/// Belt and braces with [`cache_name`]: a host that shares one store between
/// several providers still needs its keys not to collide.
pub fn cache_namespace(style: &str) -> String {
    // The suffix is a **generation**, and it exists because the store is keyed
    // by tile coordinate rather than by URL: a change in what we ask Bing for
    // is invisible to it, and every tile already cached keeps being served as
    // it was.
    //
    // Generation 2 is the `n=z` change. Everything cached before it may be
    // Bing's "no imagery" placeholder — a pale square with a crossed-out icon,
    // served at HTTP 200, decoding into a perfectly valid texture that nothing
    // downstream can tell from aerial photography. They are indistinguishable
    // from real tiles in the store too, so the only honest move is to stop
    // reading the whole generation.
    format!("bing2-{}", style.to_lowercase())
}

impl BingMetadata {
    /// The Bing metadata REST URL for a map style + key.
    pub fn metadata_url(base: &str, map_style: &str, key: &str) -> String {
        format!(
            "{}/REST/v1/Imagery/Metadata/{}?incl=ImageryProviders&key={}&uriScheme=https",
            base.trim_end_matches('/'),
            map_style,
            key
        )
    }

    /// The Web Mercator tiling scheme for this layer. Bing serves no level-0
    /// tile (the empty quadkey is invalid), so draping clamps to level 1.
    pub fn tiling_scheme(&self) -> TilingScheme {
        let mut scheme = TilingScheme::web_mercator();
        scheme.tile_size = self.tile_width.max(1);
        scheme.minimum_level = 1;
        scheme
    }

    /// Bing quadkey of a Web Mercator tile: interleave x/y bits, MSB → level 1.
    pub fn quadkey(x: u64, y: u64, level: u32) -> String {
        let mut s = String::with_capacity(level as usize);
        for i in (1..=level).rev() {
            let mask = 1u64 << (i - 1);
            let mut digit = 0u8;
            if x & mask != 0 {
                digit += 1;
            }
            if y & mask != 0 {
                digit += 2;
            }
            s.push((b'0' + digit) as char);
        }
        s
    }

    /// The image URL for a tile: subdomain + quadkey substituted into the
    /// metadata template. Pure (no fetch) — usable from a wasm host that lets
    /// JS do the fetching.
    pub fn tile_url(&self, coord: ImageryCoord) -> String {
        let quadkey = Self::quadkey(coord.x, coord.y, coord.level);
        let subdomain = if self.subdomains.is_empty() {
            ""
        } else {
            let i = (coord.x + coord.y + u64::from(coord.level)) as usize % self.subdomains.len();
            &self.subdomains[i]
        };
        let url = self
            .image_url
            .replace("{subdomain}", subdomain)
            .replace("{quadkey}", &quadkey);
        // `n=z`: answer a tile Bing does not have with a **zero-length body**
        // rather than with a placeholder image.
        //
        // The default is the trap. Bing answers a missing tile with a pale
        // square carrying a "no imagery" icon, at HTTP 200, and it decodes into
        // a perfectly valid texture — so nothing downstream can tell it from
        // aerial photography. It is cached, draped, drawn, and reads on screen
        // as a flat pale rectangle over ground that has real imagery beside it.
        // Every instrument agrees the tile is fine, including
        // `tiles_without_imagery`, because the tile *does* have imagery.
        //
        // An empty body is a miss, and a miss is what makes the drape fall back
        // to the ancestor's picture — coarser, and of the actual ground.
        //
        // The reference implementation sends the same parameter for the same
        // stated reason (`BingMapsImageryProvider.js`, `buildImageResource`).
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}n=z")
    }

    /// Parses the Bing metadata JSON document.
    pub fn from_json(bytes: &[u8]) -> Result<Self, RasterError> {
        let doc: MetadataDoc =
            serde_json::from_slice(bytes).map_err(|e| RasterError::Image(e.to_string()))?;
        let resource = doc
            .resource_sets
            .into_iter()
            .next()
            .and_then(|rs| rs.resources.into_iter().next())
            .ok_or_else(|| RasterError::Image("Bing metadata has no resources".into()))?;
        Ok(Self {
            image_url: resource.image_url,
            subdomains: resource.image_url_subdomains,
            tile_width: resource.image_width.unwrap_or(256),
            tile_height: resource.image_height.unwrap_or(256),
        })
    }
}

#[derive(Deserialize)]
struct MetadataDoc {
    #[serde(rename = "resourceSets")]
    resource_sets: Vec<ResourceSet>,
}
#[derive(Deserialize)]
struct ResourceSet {
    resources: Vec<Resource>,
}
#[derive(Deserialize)]
struct Resource {
    #[serde(rename = "imageUrl")]
    image_url: String,
    #[serde(rename = "imageUrlSubdomains")]
    image_url_subdomains: Vec<String>,
    #[serde(rename = "imageWidth")]
    image_width: Option<u32>,
    #[serde(rename = "imageHeight")]
    image_height: Option<u32>,
}

/// Bing imagery provider over a [`TileFetcher`].
pub struct BingImageryProvider<F: TileFetcher> {
    fetcher: Arc<F>,
    metadata: BingMetadata,
    scheme: TilingScheme,
}

impl<F: TileFetcher> BingImageryProvider<F> {
    /// Builds from already-fetched metadata.
    pub fn new(fetcher: Arc<F>, metadata: BingMetadata) -> Self {
        let scheme = metadata.tiling_scheme();
        Self {
            fetcher,
            metadata,
            scheme,
        }
    }

    /// Fetches and parses the Bing metadata, then builds the provider.
    pub async fn from_metadata_url(
        fetcher: Arc<F>,
        metadata_url: &str,
    ) -> Result<Self, RasterError> {
        let url = metadata_url
            .parse()
            .map_err(|e: url::ParseError| RasterError::Image(e.to_string()))?;
        let bytes = fetcher.fetch(&url).await?;
        Ok(Self::new(fetcher, BingMetadata::from_json(&bytes)?))
    }

    /// Bing quadkey of a Web Mercator tile (see [`BingMetadata::quadkey`]).
    pub fn quadkey(x: u64, y: u64, level: u32) -> String {
        BingMetadata::quadkey(x, y, level)
    }

    /// Image URL of a tile (see [`BingMetadata::tile_url`]).
    pub fn tile_url(&self, coord: ImageryCoord) -> String {
        self.metadata.tile_url(coord)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<F: TileFetcher> ImageryProvider for BingImageryProvider<F> {
    fn tiling_scheme(&self) -> TilingScheme {
        self.scheme
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let url = self
            .metadata
            .tile_url(coord)
            .parse()
            .map_err(|e: url::ParseError| RasterError::Image(e.to_string()))?;
        let fetched = self.fetcher.fetch_cacheable(&url).await?;
        // An empty body is Bing saying it has no imagery here — see the `n=z`
        // parameter in [`BingMetadata::tile_url`]. It arrives with HTTP 200, so
        // nothing below this line would call it a miss: the decoder would simply
        // fail on zero bytes, the whole tile load would fail with it, and the
        // ground would stay coarse for as long as the camera looked at it while
        // the traversal asked again and again.
        //
        // Named as `NotFound` because that is what it is, and because the drape
        // already knows what to do with one: climb to the ancestor and use its
        // picture of the same ground.
        if fetched.value.is_empty() {
            return Err(RasterError::Fetch(FetchError::NotFound(url)));
        }
        Ok(fetched)
    }
}

#[cfg(test)]
mod missing_tile_tests {
    use super::tests::MockFetcher;
    use super::*;

    fn metadata(template: &str) -> BingMetadata {
        BingMetadata {
            image_url: template.into(),
            subdomains: vec!["t0".into()],
            tile_width: 256,
            tile_height: 256,
        }
    }

    /// **Every tile request asks for a zero-length response where Bing has no
    /// imagery, rather than for a picture of a crossed-out camera.**
    ///
    /// Bing's default is to answer a tile it does not have with a *placeholder
    /// image* — a pale square carrying a "no imagery" icon. It arrives with HTTP
    /// 200, it decodes, and it is a perfectly valid texture, so nothing
    /// downstream can tell it from aerial photography: it is draped, drawn, and
    /// reads on screen as a flat pale rectangle over ground that has real
    /// imagery beside it. `tiles_without_imagery` stays at zero throughout,
    /// because the tile *does* have imagery. It has that.
    ///
    /// `n=z` asks for a zero-length body instead, which the drape treats as "no
    /// tile at this level" and answers with the ancestor's imagery — coarser,
    /// and a picture of the actual ground. The reference implementation sends
    /// exactly this parameter, and says why in the same words:
    /// "this parameter tells the Bing servers to send a zero-length response
    /// instead of a placeholder image for missing tiles"
    /// (`BingMapsImageryProvider.js`, `buildImageResource`).
    /// **An empty body is a miss, not a decode failure.**
    ///
    /// The other half of `n=z`, and useless without it. Bing answers a tile it
    /// does not have with HTTP **200** and zero bytes, so nothing between the
    /// socket and the drape would call it missing on its own: the decoder would
    /// fail on an empty buffer, the failure would take the whole geometry tile
    /// down with it, and the ground would stay coarse for as long as the camera
    /// looked at it — while the traversal asked for the same tile again every
    /// pass.
    ///
    /// `NotFound` is what the drape already knows how to answer: climb to the
    /// ancestor and drape its picture of the same ground, which is exactly the
    /// behaviour that was wanted.
    #[test]
    fn an_empty_body_is_reported_as_a_missing_tile() {
        let fetcher = Arc::new(MockFetcher::default());
        let coord = ImageryCoord {
            level: 12,
            x: 2100,
            y: 1500,
        };
        let meta = metadata("https://example.test/{subdomain}/{quadkey}.jpeg");
        fetcher.put(&meta.tile_url(coord), Vec::new());
        let meta = metadata("https://example.test/{subdomain}/{quadkey}.jpeg");
        let provider = BingImageryProvider::new(Arc::clone(&fetcher), meta);

        match futures_executor::block_on(provider.fetch_tile_bytes(coord)) {
            Err(RasterError::Fetch(FetchError::NotFound(_))) => {}
            other => panic!(
                "an empty Bing body must read as a missing tile so the drape \
                 climbs to the ancestor; got {other:?}"
            ),
        }
    }

    #[test]
    fn a_tile_request_asks_for_no_placeholder() {
        // Bing's own template already carries a query string; a second one
        // would make the whole parameter silently ignored.
        for template in [
            "https://ecn.{subdomain}.tiles.virtualearth.net/tiles/a{quadkey}.jpeg?g=1&mkt=en-GB",
            "https://example.test/{subdomain}/{quadkey}.jpeg",
        ] {
            let url = metadata(template).tile_url(ImageryCoord {
                level: 12,
                x: 2100,
                y: 1500,
            });
            assert!(
                url.contains("n=z"),
                "{url} would be answered with Bing's placeholder image, which is \
                 indistinguishable from real imagery once decoded"
            );
            assert_eq!(
                url.matches('?').count(),
                1,
                "{url} has two query strings, so the parameter is not read"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tuile_core::fetch::FetchError;
    use url::Url;

    #[derive(Default)]
    pub(super) struct MockFetcher {
        files: Mutex<HashMap<String, Vec<u8>>>,
        log: Mutex<Vec<String>>,
    }
    impl MockFetcher {
        pub(super) fn put(&self, url: &str, bytes: Vec<u8>) {
            self.files
                .lock()
                .expect("lock")
                .insert(url.to_owned(), bytes);
        }
    }
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl TileFetcher for MockFetcher {
        async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
            self.log.lock().expect("lock").push(url.to_string());
            self.files
                .lock()
                .expect("lock")
                .get(url.as_str())
                .map(|b| Bytes::from(b.clone()))
                .ok_or_else(|| FetchError::NotFound(url.clone()))
        }
    }

    fn png_2x2() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode");
        buf.into_inner()
    }

    #[test]
    fn quadkey_matches_bing_reference() {
        // Verified against the live Bing tile fetched for the Alps:
        // (x=541, y=364, z=10) → "1202213301".
        assert_eq!(
            BingImageryProvider::<MockFetcher>::quadkey(541, 364, 10),
            "1202213301"
        );
        // Canonical examples.
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(0, 0, 1), "0");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(1, 0, 1), "1");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(0, 1, 1), "2");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(1, 1, 1), "3");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(3, 5, 3), "213");
    }

    #[test]
    fn metadata_parses_real_shape() {
        let json = r#"{ "resourceSets": [{ "resources": [{
            "imageUrl": "https://ecn.{subdomain}.tiles.virtualearth.net/tiles/a{quadkey}.jpeg?g=1",
            "imageUrlSubdomains": ["t0","t1","t2","t3"],
            "imageWidth": 256, "imageHeight": 256 }]}]}"#;
        let m = BingMetadata::from_json(json.as_bytes()).expect("parse");
        assert_eq!(m.subdomains.len(), 4);
        assert!(m.image_url.contains("{quadkey}"));
    }

    #[test]
    fn builds_tile_url_and_fetches_decoded_texture() {
        let meta = BingMetadata {
            image_url: "https://ecn.{subdomain}.tiles.virtualearth.net/tiles/a{quadkey}.jpeg"
                .into(),
            subdomains: vec!["t0".into(), "t1".into(), "t2".into(), "t3".into()],
            tile_width: 256,
            tile_height: 256,
        };
        let fetcher = Arc::new(MockFetcher::default());
        let coord = ImageryCoord {
            level: 10,
            x: 541,
            y: 364,
        };
        let provider = BingImageryProvider::new(Arc::clone(&fetcher), meta);
        let url = provider.tile_url(coord);
        // subdomain index = (541+364+10) % 4 = 915 % 4 = 3 → "t3"; quadkey as above.
        assert_eq!(
            url,
            "https://ecn.t3.tiles.virtualearth.net/tiles/a1202213301.jpeg?n=z"
        );
        fetcher.put(&url, png_2x2());

        let tex = futures_executor::block_on(provider.fetch_tile(coord)).expect("fetch");
        assert_eq!((tex.value.width, tex.value.height), (2, 2));
        assert_eq!(&tex.value.rgba8[0..4], &[10, 20, 30, 255]);
    }
}
