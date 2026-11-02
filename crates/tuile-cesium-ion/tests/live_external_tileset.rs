// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A 3D Tiles asset ion points elsewhere, opened live: the endpoint, the
//! root, an external tileset beneath it and a content under that — every
//! request answered, which only happens when the key and the session reach
//! each one.
//!
//! Ignored: it needs the network and an ion token (`CESIUM_ION_TOKEN`, never
//! printed), and the asset (`TUILE_LIVE_TILESET`, 2275207 unless said) must be
//! in that token's account.
//!
//! ```text
//! CESIUM_ION_TOKEN=… cargo test -p tuile-cesium-ion --features reqwest \
//!     --test live_external_tileset -- --ignored
//! ```

#![cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]

use std::sync::Arc;

use tuile_cesium_ion::{IonClient, IonTileFetcher, ReqwestHttp};
use tuile_core::fetch::TileFetcher;
use tuile_core::tileset::{ContentKind, Tileset};

#[tokio::test]
#[ignore = "needs the network and an ion token in CESIUM_ION_TOKEN"]
async fn an_external_tileset_answers_all_the_way_down() {
    let token = std::env::var("CESIUM_ION_TOKEN").expect("CESIUM_ION_TOKEN");
    let asset: u64 = std::env::var("TUILE_LIVE_TILESET")
        .ok()
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_275_207);
    let fetcher = IonTileFetcher::new(
        IonClient::new(Arc::new(ReqwestHttp::default()), token),
        asset,
    );
    let url = fetcher.tileset_url().await.expect("the endpoint");
    assert!(!fetcher.attributions().await.expect("credits").is_empty());
    let root = fetcher.fetch(&url).await.expect("the root");
    let mut tileset = Tileset::from_json_bytes(&root, &url).expect("the root parses");

    // Down the first external tileset, and to a content beneath it.
    let content = |tileset: &Tileset, kind: ContentKind| {
        (0..tileset.len() as u64)
            .map(tuile_core::source::TileId)
            .find_map(|id| {
                let c = tileset.tile(id).content.clone()?;
                (c.kind == kind).then_some((id, c.url))
            })
    };
    let (host, external) = content(&tileset, ContentKind::ExternalTileset).expect("a subtree");
    let bytes = fetcher.fetch(&external).await.expect("the subtree answers");
    let json = serde_json::from_slice(&bytes).expect("the subtree parses");
    let grafted = tileset.graft(host, json, &external).expect("grafted");
    let below = (grafted.0..tileset.len() as u64)
        .map(tuile_core::source::TileId)
        .find_map(|id| tileset.tile(id).content.clone())
        .expect("content beneath");
    let body = fetcher
        .fetch(&below.url)
        .await
        .expect("content beneath answers");
    assert!(!body.is_empty());
}
