// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # ion-hosted imagery: TileMapService
//!
//! An ion imagery asset with no `externalType` is not a slippy map. It is a
//! **TileMapService**, and it says so in a document beside it:
//! `{base}/tilemapresource.xml`. That document — not a constant in this file —
//! decides the projection, the tile size, the file extension, and how deep the
//! pyramid goes.
//!
//! Verified against ion on 17 September 2026, asset 3954 (Sentinel-2 cloudless
//! by EOX):
//!
//! ```text
//! SRS EPSG:4326 · profile global-geodetic · TileFormat 256×256 jpg
//! Origin -180 -90 · TileSets 0..13
//! ```
//!
//! and the reference implementation takes the same branch: no `externalType`
//! means `TileMapServiceRasterOverlay`
//! (`CesiumRasterOverlays/src/IonRasterOverlay.cpp`, the `else` after `BING`).
//!
//! ## The y axis, which is the whole trap
//!
//! TMS counts rows **from the south** — its `Origin` is `-180 -90`. Our
//! [`ImageryCoord`] counts them from the north: [`TilingScheme::tile_rect`]
//! maps `(x0, y0)` to the north-west corner, and `Projection::to_normalized`
//! documents "v grows southward". Every tile fetch therefore flips:
//!
//! ```text
//! y_tms = rows_at_level - 1 - y
//! ```
//!
//! CesiumJS writes the same thing as a URL template, `{z}/{x}/{reverseY}`
//! (`Scene/TileMapServiceImageryProvider.js`). Forget it and the world renders
//! upside down — silently, because every tile still exists.
//!
//! ## Why not `TileFetcher`
//!
//! Because an ion tile needs `Authorization: Bearer <asset token>` and
//! [`TileFetcher::fetch`] takes a URL and nothing else. The seam that carries a
//! bearer is [`IonHttp`], which the same transport also implements — this is
//! not a second HTTP client, it is the other trait on the same object. Terrain
//! already goes this way (`terrain.rs`), including re-resolving the endpoint
//! once on 401/403, and so does this.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, Projection, RasterError, TilingScheme};
use url::Url;

use crate::{Attribution, IonClient, IonError, IonHttp};

/// What `tilemapresource.xml` states, and nothing this file decided.
#[derive(Debug, Clone, PartialEq)]
pub struct TileMapResource {
    pub scheme: TilingScheme,
    /// File extension of a tile, `TileFormat@extension` (e.g. `jpg`).
    pub extension: String,
    /// The `href` of each level's tileset, keyed by `TileSet@order`.
    ///
    /// Kept rather than assumed equal to the level: the TMS spec allows any
    /// href, and gdal2tiles writes absolute URLs in some versions. Sentinel-2
    /// writes `"0".."13"`, which is the common case and not a rule.
    pub hrefs: BTreeMap<u32, String>,
}

impl TileMapResource {
    /// Parses the document, deriving everything from it.
    ///
    /// Accepts gzip: ion serves this file with `Content-Encoding: gzip`, and a
    /// transport that does not decompress hands the magic bytes straight
    /// through. Sniffing two bytes costs nothing and removes a failure whose
    /// symptom — "not well-formed XML" — names the wrong cause.
    pub fn parse(body: &[u8]) -> Result<Self, IonError> {
        let owned;
        let bytes = if body.starts_with(&[0x1f, 0x8b]) {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(body)
                .read_to_end(&mut out)
                .map_err(|e| IonError::Transport(format!("gunzipping tilemapresource: {e}")))?;
            owned = out;
            &owned[..]
        } else {
            body
        };
        let text = std::str::from_utf8(bytes)
            .map_err(|e| IonError::Transport(format!("tilemapresource is not utf-8: {e}")))?;
        let doc = roxmltree::Document::parse(text)
            .map_err(|e| IonError::Transport(format!("parsing tilemapresource: {e}")))?;
        let root = doc.root_element();

        let child = |name: &str| root.children().find(|n| n.has_tag_name(name));
        let format = child("TileFormat");
        let extension = format
            .and_then(|n| n.attribute("extension"))
            .unwrap_or("png")
            .to_string();
        let tile_size: u32 = format
            .and_then(|n| n.attribute("width"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);

        let tilesets = child("TileSets");
        let mut hrefs = BTreeMap::new();
        if let Some(sets) = tilesets {
            for set in sets.children().filter(|n| n.has_tag_name("TileSet")) {
                if let (Some(order), Some(href)) = (set.attribute("order"), set.attribute("href")) {
                    if let Ok(level) = order.parse::<u32>() {
                        hrefs.insert(level, href.to_string());
                    }
                }
            }
        }
        if hrefs.is_empty() {
            return Err(IonError::Transport(
                "tilemapresource lists no TileSet: nothing says how deep this imagery goes".into(),
            ));
        }

        // The profile first, the SRS as the fallback — the order cesium-native
        // reads them in. `global-` is the TMS standard's spelling, the bare
        // words are gdal2tiles'; both name the same two projections.
        let profile = tilesets.and_then(|n| n.attribute("profile")).unwrap_or("");
        let srs = child("SRS").and_then(|n| n.text()).unwrap_or("");
        let projection = match profile {
            "geodetic" | "global-geodetic" => Projection::Geographic,
            "mercator" | "global-mercator" => Projection::WebMercator,
            _ if srs.contains("4326") => Projection::Geographic,
            _ if srs.contains("3857") || srs.contains("900913") => Projection::WebMercator,
            other => {
                return Err(IonError::Transport(format!(
                    "tilemapresource profile {other:?} / SRS {srs:?} is neither geodetic nor \
                     mercator, and guessing which would render a world in the wrong place"
                )))
            }
        };

        let mut scheme = match projection {
            Projection::Geographic => TilingScheme::geographic(),
            Projection::WebMercator => TilingScheme::web_mercator(),
        };
        scheme.tile_size = tile_size;
        scheme.minimum_level = *hrefs.keys().next().expect("non-empty");
        scheme.maximum_level = *hrefs.keys().next_back().expect("non-empty");

        Ok(Self {
            scheme,
            extension,
            hrefs,
        })
    }

    /// The tile's path, relative to the base URL, with the y axis flipped.
    pub fn tile_path(&self, coord: ImageryCoord) -> Option<String> {
        let href = self.hrefs.get(&coord.level)?;
        let (_, rows) = self.scheme.tiles_at(coord.level);
        let y = rows.checked_sub(1)?.checked_sub(coord.y)?;
        Some(format!("{href}/{}/{y}.{}", coord.x, self.extension))
    }
}

struct State {
    base: Url,
    token: String,
    resource: TileMapResource,
    attributions: Vec<Attribution>,
}

/// Imagery served by ion itself, through a TileMapService.
pub struct TmsImagery<H: IonHttp> {
    client: IonClient<H>,
    asset_id: u64,
    state: Mutex<Option<State>>,
}

impl<H: IonHttp> TmsImagery<H> {
    /// Resolves the endpoint and its descriptor. Both are needed before a
    /// single tile can be addressed — the descriptor is what says how.
    pub async fn open(client: IonClient<H>, asset_id: u64) -> Result<Self, IonError> {
        let endpoint = match client.asset_endpoint(asset_id).await? {
            crate::AssetEndpoint::Imagery(e) => e,
            other => {
                return Err(IonError::UnsupportedAssetType {
                    asset_id,
                    kind: format!("{other:?}"),
                })
            }
        };
        Self::from_endpoint(client, asset_id, endpoint).await
    }

    /// The same, for a caller that has already resolved the endpoint.
    ///
    /// Resolving costs a round trip to `api.cesium.com` and mints a token that
    /// expires; doing it twice for one globe is a second chance to get a
    /// different answer for no benefit.
    pub async fn from_endpoint(
        client: IonClient<H>,
        asset_id: u64,
        endpoint: crate::ImageryEndpoint,
    ) -> Result<Self, IonError> {
        if let Some(kind) = &endpoint.external_type {
            return Err(IonError::Transport(format!(
                "ion asset {asset_id} is external imagery ({kind}), not an ion-hosted \
                 TileMapService"
            )));
        }
        let base = endpoint.url.clone().ok_or_else(|| {
            IonError::Transport(format!("ion asset {asset_id} imagery endpoint has no url"))
        })?;
        let descriptor = base.join("tilemapresource.xml")?;
        let response = client
            .http()
            .get(&descriptor, Some(&endpoint.access_token))
            .await?;
        if response.status != 200 {
            return Err(IonError::Transport(format!(
                "{descriptor} answered {}",
                response.status
            )));
        }
        let resource = TileMapResource::parse(&response.body)?;
        tracing::info!(
            asset = asset_id,
            projection = ?resource.scheme.projection,
            tile_size = resource.scheme.tile_size,
            levels = format!(
                "{}..{}",
                resource.scheme.minimum_level, resource.scheme.maximum_level
            ),
            extension = resource.extension,
            "ion imagery resolved from its descriptor"
        );
        Ok(Self {
            client,
            asset_id,
            state: Mutex::new(Some(State {
                base,
                token: endpoint.access_token,
                resource,
                attributions: endpoint.attributions,
            })),
        })
    }

    /// What ion's terms require a consumer to display.
    pub fn attributions(&self) -> Vec<Attribution> {
        self.state
            .lock()
            .expect("not poisoned")
            .as_ref()
            .map(|s| s.attributions.clone())
            .unwrap_or_default()
    }

    fn snapshot(&self) -> Result<(Url, String, TileMapResource), RasterError> {
        let guard = self.state.lock().expect("not poisoned");
        let s = guard
            .as_ref()
            .ok_or_else(|| RasterError::Provider("imagery endpoint not resolved".into()))?;
        Ok((s.base.clone(), s.token.clone(), s.resource.clone()))
    }

    /// A fresh asset token. Asset tokens expire; the endpoint mints a new one.
    async fn refresh(&self) -> Result<String, IonError> {
        let endpoint = match self.client.asset_endpoint(self.asset_id).await? {
            crate::AssetEndpoint::Imagery(e) => e,
            other => {
                return Err(IonError::UnsupportedAssetType {
                    asset_id: self.asset_id,
                    kind: format!("{other:?}"),
                })
            }
        };
        if let Some(s) = self.state.lock().expect("not poisoned").as_mut() {
            s.token = endpoint.access_token.clone();
        }
        Ok(endpoint.access_token)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<H: IonHttp> ImageryProvider for TmsImagery<H> {
    fn tiling_scheme(&self) -> TilingScheme {
        self.state
            .lock()
            .expect("not poisoned")
            .as_ref()
            .map(|s| s.resource.scheme)
            .unwrap_or_else(TilingScheme::geographic)
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let (base, mut token, resource) = self.snapshot()?;
        let path = resource.tile_path(coord).ok_or_else(|| {
            RasterError::Provider(format!(
                "level {} is outside this imagery's {}..{}",
                coord.level, resource.scheme.minimum_level, resource.scheme.maximum_level
            ))
        })?;
        let url = base
            .join(&path)
            .map_err(|e| RasterError::Provider(e.to_string()))?;
        for attempt in 0..2 {
            let response = self
                .client
                .http()
                .get(&url, Some(&token))
                .await
                .map_err(|e| RasterError::Provider(e.to_string()))?;
            match response.status {
                200 => {
                    // La fraîcheur que l'origine a énoncée, pas une durée
                    // inventée ici : c'est elle qui décide combien de temps un
                    // cache a le droit de resservir ces octets.
                    return Ok(Fetched {
                        value: response.body,
                        ttl: response.max_age,
                    })
                }
                401 | 403 if attempt == 0 => {
                    token = self
                        .refresh()
                        .await
                        .map_err(|e| RasterError::Provider(e.to_string()))?;
                }
                status => {
                    return Err(RasterError::Provider(format!("{url} answered {status}")));
                }
            }
        }
        Err(RasterError::Provider(format!(
            "{url} still refused the refreshed token"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing, trimmed: asset 3954's descriptor as ion serves it.
    const SENTINEL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<TileMap version="1.0.0" tilemapservice="http://tms.osgeo.org/1.0.0">
  <Title>Sentinel-2</Title>
  <Abstract></Abstract>
  <SRS>EPSG:4326</SRS>
  <BoundingBox minx="-180" miny="-90" maxx="180" maxy="90"/>
  <Origin x="-180" y="-90"/>
  <TileFormat width="256" height="256" mime-type="image/jpeg" extension="jpg"/>
  <TileSets profile="global-geodetic">
    <TileSet href="0" units-per-pixel="0.703125" order="0"/>
    <TileSet href="1" units-per-pixel="0.3515625" order="1"/>
    <TileSet href="13" units-per-pixel="0.0000858" order="13"/>
  </TileSets>
</TileMap>"#;

    const MERCATOR: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<TileMap>
  <SRS>EPSG:3857</SRS>
  <TileFormat width="512" height="512" extension="png"/>
  <TileSets profile="mercator">
    <TileSet href="2" order="2"/>
    <TileSet href="3" order="3"/>
  </TileSets>
</TileMap>"#;

    /// Everything comes from the document — that is the point of reading it.
    #[test]
    fn the_descriptor_decides_projection_size_extension_and_depth() {
        let r = TileMapResource::parse(SENTINEL.as_bytes()).expect("parses");
        assert_eq!(r.scheme.projection, Projection::Geographic);
        assert_eq!(r.scheme.root_tiles_x, 2, "geodetic is two tiles wide");
        assert_eq!(r.scheme.tile_size, 256);
        assert_eq!(r.extension, "jpg");
        assert_eq!(r.scheme.minimum_level, 0);
        // Treize, lu du descripteur : Sentinel-2 ne va pas plus loin, et
        // aucune constante de ce dépôt n'a le droit de le décider.
        assert_eq!(r.scheme.maximum_level, 13);

        let m = TileMapResource::parse(MERCATOR.as_bytes()).expect("parses");
        assert_eq!(m.scheme.projection, Projection::WebMercator);
        assert_eq!(m.scheme.root_tiles_x, 1);
        assert_eq!(m.scheme.tile_size, 512);
        assert_eq!(m.extension, "png");
        assert_eq!((m.scheme.minimum_level, m.scheme.maximum_level), (2, 3));
    }

    /// The trap. Our y counts from the north, TMS counts from the south.
    #[test]
    fn the_y_axis_is_flipped_against_ours() {
        let r = TileMapResource::parse(SENTINEL.as_bytes()).expect("parses");
        // Geodetic level 1: 4 columns × 2 rows. Our row 0 is the NORTHERN
        // one, so it must ask TMS for its row 1.
        let (cols, rows) = r.scheme.tiles_at(1);
        assert_eq!((cols, rows), (4, 2));
        assert_eq!(
            r.tile_path(ImageryCoord {
                level: 1,
                x: 3,
                y: 0
            })
            .as_deref(),
            Some("1/3/1.jpg")
        );
        assert_eq!(
            r.tile_path(ImageryCoord {
                level: 1,
                x: 3,
                y: 1
            })
            .as_deref(),
            Some("1/3/0.jpg")
        );
        // Level 0 has a single row, which is its own mirror.
        assert_eq!(
            r.tile_path(ImageryCoord {
                level: 0,
                x: 0,
                y: 0
            })
            .as_deref(),
            Some("0/0/0.jpg")
        );
    }

    /// A level the descriptor does not list has no address at all. Answering
    /// with a plausible URL would fetch a 404 per tile and report it as a
    /// network fault rather than as "this imagery stops here".
    #[test]
    fn a_level_the_descriptor_does_not_list_has_no_url() {
        let r = TileMapResource::parse(SENTINEL.as_bytes()).expect("parses");
        assert!(r
            .tile_path(ImageryCoord {
                level: 14,
                x: 0,
                y: 0
            })
            .is_none());
    }

    /// ion serves this document gzipped, and a transport that does not
    /// decompress hands the magic bytes through.
    #[test]
    fn a_gzipped_descriptor_reads_the_same() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(SENTINEL.as_bytes()).expect("compresses");
        let gz = enc.finish().expect("finishes");
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        assert_eq!(
            TileMapResource::parse(&gz).expect("parses"),
            TileMapResource::parse(SENTINEL.as_bytes()).expect("parses")
        );
    }

    /// Neither geodetic nor mercator is a refusal, not a guess: placing a
    /// world in the wrong projection is invisible until someone recognises a
    /// coastline.
    #[test]
    fn an_unknown_projection_is_refused_by_name() {
        let odd = SENTINEL
            .replace("EPSG:4326", "EPSG:27700")
            .replace(r#"profile="global-geodetic""#, r#"profile="british-national-grid""#);
        let err = TileMapResource::parse(odd.as_bytes()).expect_err("refuses");
        let text = err.to_string();
        assert!(text.contains("british-national-grid"), "{text}");
        assert!(text.contains("27700"), "{text}");
    }
}
