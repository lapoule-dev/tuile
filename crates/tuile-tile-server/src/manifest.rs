// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A zone's list of archives, and the one write that makes a change visible.
//!
//! Archives are immutable; what changes is which of them a zone is made of.
//! That list is one small JSON object per zone, replaced by a **conditional**
//! write: the new version is accepted only if the stored one is still the one
//! it was derived from. Two writers that race both read version N; one writes
//! N+1, the other is refused, re-reads N+1 and tries again on top of it. No
//! lock, no coordinator, and no update lost.
//!
//! The object store must honour the condition. An in-memory store does; an S3
//! bucket does only when its client is built with conditional puts (for R2,
//! ETag matching) — the integration test checks that on the real bucket.

use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// One immutable archive of a zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRef {
    /// The object key.
    pub key: String,
    pub epoch: String,
    /// Seconds since the Unix epoch, at publication.
    pub created: u64,
    pub tiles: u64,
}

/// An archive no longer part of the zone, kept until readers holding an older
/// manifest are done with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub key: String,
    pub retired: u64,
}

/// What a zone is made of. Oldest archive first: on a tile present in several
/// archives, the **last** one wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Bumped by every publication.
    pub generation: u64,
    pub archives: Vec<ArchiveRef>,
    #[serde(default)]
    pub retired: Vec<Retired>,
}

/// A manifest as read, with what a conditional write needs to replace it.
#[derive(Debug, Clone)]
pub struct Versioned {
    pub manifest: Manifest,
    /// `None` when the zone has no manifest yet.
    pub version: Option<UpdateVersion>,
}

pub(crate) fn now_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub(crate) fn manifest_path(zone_prefix: &str) -> Path {
    Path::from(format!("{zone_prefix}/manifest.json"))
}

/// Reads a zone's manifest; an absent one is an empty zone.
pub async fn read(store: &dyn ObjectStore, zone_prefix: &str) -> Result<Versioned, StoreError> {
    match store.get(&manifest_path(zone_prefix)).await {
        Ok(got) => {
            let version = UpdateVersion { e_tag: got.meta.e_tag.clone(), version: got.meta.version.clone() };
            let bytes = got.bytes().await?;
            let manifest = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("{zone_prefix}/manifest.json: {e}")))?;
            Ok(Versioned { manifest, version: Some(version) })
        }
        Err(object_store::Error::NotFound { .. }) => Ok(Versioned { manifest: Manifest::default(), version: None }),
        Err(e) => Err(e.into()),
    }
}

/// Replaces the manifest `from` was read as. `Ok(false)`: someone else
/// published first; re-read and derive again.
pub async fn replace(
    store: &dyn ObjectStore,
    zone_prefix: &str,
    from: &Versioned,
    next: &Manifest,
) -> Result<bool, StoreError> {
    let body = serde_json::to_vec(next).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let mode = match &from.version {
        Some(v) => PutMode::Update(v.clone()),
        None => PutMode::Create,
    };
    let opts = PutOptions { mode, ..Default::default() };
    match store.put_opts(&manifest_path(zone_prefix), PutPayload::from(Bytes::from(body)), opts).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::Precondition { .. }) | Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
