<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-cesium-ion

Source connector for Cesium ion: resolves asset endpoints
(`/v1/assets/{id}/endpoint`), carries bearer tokens, refreshes them on
401/403, and exposes ion-hosted **3D Tiles** as a plain
[`tuile_core::fetch::TileFetcher`] — the core never knows ion exists.

**Imagery** assets resolve the same way ([`ImageryEndpoint`]); this
crate is their *transport* (tile URLs + auth). Draping imagery onto
geometry is the core's raster-overlay module (see roadmap) — not this
crate's business.

A 3D Tiles asset ion does not host itself — its endpoint has an
`externalType` and names the tileset in `options.url`, with the other
service's key on it — resolves the same way: no ion bearer is sent there,
the root document is asked fresh every time ([`IonHttp::get_fresh`]) since
it hands out the session the content is fetched under, and the core carries
the key and the session to every request beneath. [`IonSources`] answers
`Sources::tileset` with the root, the fetcher and the credits
([`Attribution::credit`]: the words, the logo and the link out of each
attribution's HTML).

Tokens are never stored in any repository; pass them in (CLI flag, env
var read by the caller). Attribution strings returned by ion must be
displayed by consumers ([`Attribution`]).

## Where it sits

Depends on `tuile-core`, `tuile-bing`, `tuile-terrain` and `tuile-planetary`.
Used by `tuile-native-fetchers` (which implements its `IonHttp`), by the hosts
(`tuile-viewer`, `tuile-web`, `tuile-bake`) and by the examples that open an
ion asset.

## Example

```rust,no_run
# #[cfg(feature = "reqwest")]
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use std::sync::Arc;
use tuile_cesium_ion::{IonClient, IonTileFetcher, ReqwestHttp};

let token = std::env::var("ION_TOKEN")?;
let client = IonClient::new(Arc::new(ReqwestHttp::default()), token);
let fetcher = IonTileFetcher::new(client, 96188);
let tileset_url = fetcher.tileset_url().await?;
println!("{tileset_url}");
# Ok(())
# }
```

## Features

- `reqwest` — `ReqwestHttp`, an `IonHttp` over `reqwest`.
- `proxy` — the catalog proxy on axum (see the `proxy` module and the
  `catalog-proxy` example). Implies `reqwest`.

## Documentation

- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
- [`docs/03-roadmap.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/03-roadmap.md)
