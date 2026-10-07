// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::ops::Range;

use async_trait::async_trait;
use tuile_farm::{ObjectRunStore, RunStore, StoreError};

use crate::{Entry, Listing, Objects, Read, RepoError};

impl From<StoreError> for RepoError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(key) => RepoError::NotFound(key),
            e => RepoError::Store(e.to_string()),
        }
    }
}

#[async_trait]
impl Objects for ObjectRunStore {
    fn label(&self) -> String {
        ObjectRunStore::label(self).to_string()
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, RepoError> {
        Ok(RunStore::list(self, prefix.trim_matches('/'))
            .await?
            .into_iter()
            .map(|e| Entry {
                key: e.key,
                size: e.size,
            })
            .collect())
    }

    async fn browse(&self, prefix: &str) -> Result<Listing, RepoError> {
        let (dirs, files) = ObjectRunStore::browse(self, prefix).await?;
        Ok(Listing {
            dirs,
            files: files
                .into_iter()
                .map(|e| Entry {
                    key: e.key,
                    size: e.size,
                })
                .collect(),
        })
    }

    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        Ok(ObjectRunStore::size(self, key).await?)
    }

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>, RepoError> {
        let wanted = range.end - range.start;
        let bytes = self.get_range(key, range).await?;
        if bytes.len() as u64 != wanted {
            return Err(RepoError::Store(format!(
                "{key}: asked for {wanted} bytes, got {}",
                bytes.len()
            )));
        }
        Ok(bytes.to_vec())
    }

    async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
        Ok(match self.get_if_changed(key, known).await? {
            Some((bytes, etag)) => Read::Changed {
                bytes: bytes.to_vec(),
                etag,
            },
            None => Read::Unchanged,
        })
    }
}
