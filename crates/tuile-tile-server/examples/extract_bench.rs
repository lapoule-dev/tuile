// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Measures an extract against a real store: the tiles an existing extract
//! holds asked for again, gathered from the store as it stands now, and
//! written to a file whose bytes can be compared from one build to the next.
//!
//! An extract names its layers and their bands in its metadata, so the asks
//! it was made from come back from its tile ids alone. Each run opens the
//! store afresh (a converge in a process that has read nothing), then asks
//! again of the same store (one whose bake has listed the zones already).
//! Every request the store makes is counted. Reads only: nothing is put.
//!
//! ```text
//! TUILE_BENCH_BUCKET=… CLOUDFLARE_ACCOUNT_ID=… R2_ACCESS_KEY_ID=… R2_SECRET_ACCESS_KEY=… \
//!   cargo run --release -p tuile-tile-server --example extract_bench -- \
//!   <extract.pmtiles> <out-dir> [runs]
//! ```
//!
//! Prints one JSON line per extract: `run`, `warm`, `seconds`, `requests`,
//! `bytes` (what the bucket gave), the report's fields, and the file it
//! wrote (`<out-dir>/<run>-<cold|warm>.pmtiles`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};
use tuile_tile_server::{ExtractAsk, TileStore};

/// The bucket, every request counted.
#[derive(Debug)]
struct Counted {
    inner: Arc<dyn ObjectStore>,
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl fmt::Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counted({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for Counted {
    async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.put_opts(l, p, o).await
    }

    async fn put_multipart_opts(
        &self,
        l: &Path,
        o: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.put_multipart_opts(l, o).await
    }

    async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let got = self.inner.get_opts(l, o).await?;
        self.bytes
            .fetch_add(got.range.end - got.range.start, Ordering::Relaxed);
        Ok(got)
    }

    fn delete_stream(
        &self,
        l: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(l)
    }

    fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.list(p)
    }

    async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.list_with_delimiter(p).await
    }

    async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.inner.copy_opts(f, t, o).await
    }
}

fn bucket() -> Arc<dyn ObjectStore> {
    let var = |n: &str| {
        std::env::var(n).unwrap_or_else(|_| {
            eprintln!("{n} is not set");
            std::process::exit(2)
        })
    };
    Arc::new(
        AmazonS3Builder::new()
            .with_bucket_name(var("TUILE_BENCH_BUCKET"))
            .with_endpoint(format!(
                "https://{}.r2.cloudflarestorage.com",
                var("CLOUDFLARE_ACCOUNT_ID")
            ))
            .with_region("auto")
            .with_access_key_id(var("R2_ACCESS_KEY_ID"))
            .with_secret_access_key(var("R2_SECRET_ACCESS_KEY"))
            .build()
            .expect("bucket client"),
    )
}

/// The asks an extract was made from: each of its layers, the tiles of its
/// band, as `(level, x, y)` on the layer's grid.
async fn asks_of(path: &std::path::Path, store: &TileStore) -> Vec<ExtractAsk> {
    let reader = Arc::new(
        tuile_tile_server::archive::open_local(path)
            .await
            .expect("the extract"),
    );
    let metadata: serde_json::Value =
        serde_json::from_str(&reader.get_metadata().await.expect("its metadata"))
            .expect("its metadata as JSON");
    let layers = metadata["tuile:extract"]
        .as_array()
        .expect("an extract's layers")
        .iter()
        .map(|l| {
            (
                l["band"].as_u64().expect("a band"),
                l["name"].as_str().expect("a name").to_string(),
            )
        })
        .collect::<BTreeMap<u64, String>>();
    let mut tiles: BTreeMap<String, Vec<(u8, u32, u32)>> = BTreeMap::new();
    let ids = tuile_tile_server::archive::ids(reader)
        .await
        .expect("its ids");
    for id in ids {
        let (band, name) = layers
            .range(..=id)
            .next_back()
            .expect("a band below every id");
        let grid = store.layer(name).expect("a layer of the store").grid;
        let at = grid.from_id(id - band).expect("an id of the grid");
        tiles.entry(name.clone()).or_default().push(at);
    }
    // In the order the extract named them.
    layers
        .values()
        .map(|name| ExtractAsk {
            layer: name.clone(),
            tiles: tiles.remove(name).unwrap_or_default(),
            ..Default::default()
        })
        .collect()
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(extract), Some(out)) = (args.next(), args.next()) else {
        eprintln!("usage: extract_bench <extract.pmtiles> <out-dir> [runs]");
        std::process::exit(2)
    };
    let runs: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(3);
    std::fs::create_dir_all(&out).expect("the out directory");
    let origin = bucket();
    let mut asks = None;
    for run in 0..runs {
        let counted = Arc::new(Counted {
            inner: origin.clone(),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        let store = TileStore::open(counted.clone(), Default::default())
            .await
            .expect("opening the store");
        let asks = match &asks {
            Some(asks) => asks,
            None => asks.insert(asks_of(std::path::Path::new(&extract), &store).await),
        };
        for warm in [false, true] {
            let (requests, bytes) = (
                counted.requests.load(Ordering::Relaxed),
                counted.bytes.load(Ordering::Relaxed),
            );
            let began = Instant::now();
            let (file, report) = store.extract(asks).await.expect("the extract");
            let seconds = began.elapsed().as_secs_f64();
            let to = format!("{out}/{run}-{}.pmtiles", if warm { "warm" } else { "cold" });
            std::fs::copy(file.path(), &to).expect("the extract kept");
            println!(
                "{}",
                serde_json::json!({
                    "run": run,
                    "warm": warm,
                    "seconds": seconds,
                    "requests": counted.requests.load(Ordering::Relaxed) - requests,
                    "bytes": counted.bytes.load(Ordering::Relaxed) - bytes,
                    "report": report,
                    "file": to,
                })
            );
        }
    }
}
