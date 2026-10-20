// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A render's tiles, fetched before its first frame as extracts.
//!
//! As a bake ends, its host has the tile store's server prepare one archive
//! of every store tile the pack refers to — terrain and imagery, each layer
//! in its band ([`tuile_repository::EXTRACT_BANDS`]) — kept on the server's
//! disk under a key. The render is handed the key and size; it fetches the
//! extract in parts of [`PART_MIB`] MiB, all at once, into a local file, and
//! reads the store from that file alone.
//!
//! ```text
//! GET <root>/store/extract/<key>/32/<n>   part n, 32 MiB each but the last
//! ```
//!
//! The key is in every path: the network routes a key's requests to the
//! server that prepared it.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures_util::{StreamExt, TryStreamExt};
use tuile_repository::Get;

use crate::Error;

/// Part size, in MiB, as the paths say it.
pub const PART_MIB: u64 = 32;
const PART_BYTES: u64 = PART_MIB << 20;
/// Parts fetched at once, over every extract of a render.
pub const PARTS_AT_ONCE: usize = 16;

/// An extract prepared for a render: its key on the server, its size, and
/// the layers it holds, in the order of their bands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractRef {
    pub key: String,
    pub bytes: u64,
    pub layers: Vec<String>,
}

impl ExtractRef {
    /// What is added to a tile's id of `layer` in this extract, if it holds
    /// that layer.
    pub fn band_of(&self, layer: &str) -> Option<u64> {
        let i = self.layers.iter().position(|l| l == layer)?;
        tuile_repository::EXTRACT_BANDS.get(i).copied()
    }
}

/// What fetching a render's extracts took: the receipt's own type.
pub use tuile_farm::receipt::Fetched;

/// The local key an extract is written to, under the render's directory.
pub fn local_key(extract: &ExtractRef) -> String {
    let flat: String = extract
        .key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("extracts/{flat}.pmtiles")
}

/// The number of parts of an extract of `bytes`.
pub fn parts(bytes: u64) -> u64 {
    bytes.div_ceil(PART_BYTES).max(1)
}

/// Fetches `extracts` under `dir`, every part of every one of them
/// [`PARTS_AT_ONCE`] at a time.
pub async fn fetch(
    get: Arc<dyn Get>,
    extracts: &[ExtractRef],
    dir: &Path,
) -> Result<Fetched, Error> {
    let began = Instant::now();
    let mut asked = Vec::new();
    for extract in extracts {
        let path = dir.join(local_key(extract));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = Arc::new(std::fs::File::create(&path)?);
        file.set_len(extract.bytes)?;
        asked.extend(
            (0..parts(extract.bytes))
                .map(|n| (extract.key.clone(), extract.bytes, file.clone(), n)),
        );
    }
    let count = asked.len() as u64;
    futures_util::stream::iter(asked)
        .map(|(key, bytes, file, n)| {
            let get = get.clone();
            async move {
                let got = get
                    .get(&format!("store/extract/{key}/{PART_MIB}/{n}"))
                    .await?;
                let expected = PART_BYTES.min(bytes - n * PART_BYTES);
                if got.status != 200 || got.body.len() as u64 != expected {
                    return Err::<(), Error>(
                        format!(
                            "extract {key}, part {n}: HTTP {}, {} bytes of {expected}",
                            got.status,
                            got.body.len()
                        )
                        .into(),
                    );
                }
                // Each part at its place in the file, from a thread of its own.
                tokio::task::spawn_blocking(move || {
                    std::os::unix::fs::FileExt::write_all_at(
                        file.as_ref(),
                        &got.body,
                        n * PART_BYTES,
                    )
                })
                .await
                .map_err(|e| e.to_string())??;
                Ok(())
            }
        })
        .buffer_unordered(PARTS_AT_ONCE)
        .try_collect::<()>()
        .await?;
    Ok(Fetched {
        extracts: extracts.len() as u64,
        parts: count,
        bytes: extracts.iter().map(|e| e.bytes).sum(),
        seconds: began.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tuile_repository::Got;

    /// A server holding extracts, answering parts as the route does.
    struct Server(Vec<(String, Vec<u8>)>);

    #[async_trait]
    impl Get for Server {
        async fn get(&self, path: &str) -> Result<Got, String> {
            let found = self.0.iter().find_map(|(key, bytes)| {
                let rest = path.strip_prefix(&format!("store/extract/{key}/32/"))?;
                let n: u64 = rest.parse().ok()?;
                let from = (n * PART_BYTES) as usize;
                (from < bytes.len())
                    .then(|| bytes[from..(from + PART_BYTES as usize).min(bytes.len())].to_vec())
            });
            Ok(match found {
                Some(body) => Got {
                    status: 200,
                    object_size: None,
                    etag: None,
                    body,
                },
                None => Got {
                    status: 404,
                    object_size: None,
                    etag: None,
                    body: vec![],
                },
            })
        }
    }

    #[tokio::test]
    async fn an_extract_comes_down_in_parts_into_one_file_whole() {
        let big: Vec<u8> = (0..(PART_BYTES as usize + 1000))
            .map(|i| (i % 251) as u8)
            .collect();
        let small = b"terrain".to_vec();
        let server = Server(vec![
            ("r/c0001/imagery".into(), big.clone()),
            ("r/c0001/terrain".into(), small.clone()),
        ]);
        let refs = [
            ExtractRef {
                layers: vec!["imagery".into()],
                key: "r/c0001/imagery".into(),
                bytes: big.len() as u64,
            },
            ExtractRef {
                layers: vec!["terrain".into()],
                key: "r/c0001/terrain".into(),
                bytes: small.len() as u64,
            },
        ];
        let dir = tempfile::tempdir().expect("dir");
        let server: Arc<dyn Get> = Arc::new(server);
        let fetched = fetch(server.clone(), &refs, dir.path())
            .await
            .expect("fetched");
        assert_eq!((fetched.extracts, fetched.parts), (2, 3));
        assert_eq!(fetched.bytes, big.len() as u64 + small.len() as u64);
        assert_eq!(
            std::fs::read(dir.path().join(local_key(&refs[0]))).expect("read"),
            big
        );
        assert_eq!(
            std::fs::read(dir.path().join(local_key(&refs[1]))).expect("read"),
            small
        );

        // A size that does not match what the server holds is an error, not
        // a file with a hole in it.
        let wrong = [ExtractRef {
            bytes: small.len() as u64 + 1,
            ..refs[1].clone()
        }];
        assert!(fetch(server, &wrong, dir.path()).await.is_err());
    }
}
