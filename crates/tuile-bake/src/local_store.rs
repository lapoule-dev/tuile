// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile store on a disk: a directory that answers as the bucket does,
//! conditional writes included.
//!
//! The store replaces its manifests by conditional writes only (`PutMode::
//! Update`, the object's e-tag as the condition), which a bucket honours and
//! a plain directory does not. Here they are honoured within the process: the
//! e-tag is checked and the object replaced under one lock. Two processes
//! writing one directory at once are not guarded against; a store on a disk
//! has one bake at a time.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

/// A directory, with conditional replacement.
#[derive(Debug)]
pub struct LocalStore {
    inner: Arc<LocalFileSystem>,
    replacing: tokio::sync::Mutex<()>,
}

impl LocalStore {
    /// The store in `dir`, made if it is not there.
    pub fn new(dir: &std::path::Path) -> std::result::Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let inner = LocalFileSystem::new_with_prefix(dir)
            .map_err(|e| format!("tile store {}: {e}", dir.display()))?;
        Ok(Self {
            inner: Arc::new(inner),
            replacing: tokio::sync::Mutex::new(()),
        })
    }
}

impl fmt::Display for LocalStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LocalStore({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for LocalStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let PutMode::Update(version) = &opts.mode else {
            return self.inner.put_opts(location, payload, opts).await;
        };
        let _held = self.replacing.lock().await;
        let now = match self.inner.head(location).await {
            Ok(meta) => meta.e_tag,
            Err(object_store::Error::NotFound { .. }) => None,
            Err(e) => return Err(e),
        };
        if now.is_none() || now != version.e_tag {
            return Err(object_store::Error::Precondition {
                path: location.to_string(),
                source: format!("its e-tag is {now:?}, not {:?}", version.e_tag).into(),
            });
        }
        let opts = PutOptions {
            mode: PutMode::Overwrite,
            ..opts
        };
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::UpdateVersion;

    #[tokio::test]
    async fn a_replacement_holds_only_against_the_version_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path()).unwrap();
        let at = Path::from("layer/manifest.json");
        let first = store.put(&at, "one".into()).await.unwrap();
        let update = |e_tag: Option<String>| PutOptions {
            mode: PutMode::Update(UpdateVersion {
                e_tag,
                version: None,
            }),
            ..PutOptions::default()
        };
        let second = store
            .put_opts(&at, "two".into(), update(first.e_tag.clone()))
            .await
            .expect("replaced against the version read");
        // The first version is gone: a writer still holding it is refused.
        let stale = store
            .put_opts(&at, "three".into(), update(first.e_tag.clone()))
            .await;
        assert!(matches!(stale, Err(object_store::Error::Precondition { .. })), "{stale:?}");
        let body = store.get(&at).await.unwrap().bytes().await.unwrap();
        assert_eq!(&body[..], b"two");
        assert_ne!(first.e_tag, second.e_tag);
        // Nothing there: nothing to replace.
        let none = store
            .put_opts(&Path::from("absent"), "x".into(), update(Some("1".into())))
            .await;
        assert!(matches!(none, Err(object_store::Error::Precondition { .. })), "{none:?}");
    }
}
