// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The host's terrain and imagery, with those a configuration file named in
//! their place (`crate::config_file`): the `terrain` asset and the
//! `[[imagery]]` layers. The token and the API address are the host's to
//! read, at each connection (`tuile_viewer::credential`,
//! `tuile_viewer::embed::configured_api`).

use std::sync::Arc;

use crate::config_file::Settings;
use crate::embed::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};

/// `inner`, with what `settings` names in place of its own.
pub(crate) struct Configured {
    inner: Arc<dyn ViewerHost>,
    settings: Settings,
}

impl Configured {
    /// `inner` itself when the settings name no terrain and no layer.
    pub(crate) fn wrap(inner: Arc<dyn ViewerHost>, settings: Settings) -> Arc<dyn ViewerHost> {
        if settings.terrain.is_none() && settings.imagery.is_empty() {
            inner
        } else {
            Arc::new(Self { inner, settings })
        }
    }
}

/// The layers a file names, each stored where the host's layer of the same
/// asset is — so a layer the host already had starts as warm as before.
pub(crate) fn layers(host: &[ImageryChoice], named: &[ImageryChoice]) -> Vec<ImageryChoice> {
    named
        .iter()
        .map(|layer| match host.iter().find(|h| h.asset == layer.asset) {
            Some(same) => layer.clone().cached_as(&same.cache),
            None => layer.clone(),
        })
        .collect()
}

#[async_trait::async_trait]
impl ViewerHost for Configured {
    fn identity(&self) -> Identity {
        self.inner.identity()
    }

    fn terrain(&self) -> TerrainChoice {
        let host = self.inner.terrain();
        match self.settings.terrain {
            Some(asset) if asset != host.asset => TerrainChoice {
                asset,
                cache: format!("terrain-{asset}"),
            },
            _ => host,
        }
    }

    fn imagery(&self) -> Vec<ImageryChoice> {
        let host = self.inner.imagery();
        if self.settings.imagery.is_empty() {
            host
        } else {
            layers(&host, &self.settings.imagery)
        }
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        self.inner.connect().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_layer_of_a_known_asset_keeps_its_store() {
        let host = vec![ImageryChoice::new("aerial", "Aerial", 2, "a").cached_as("old-namespace")];
        let named = vec![
            ImageryChoice::new("photo", "Photo", 2, "a"),
            ImageryChoice::new("mine", "Mine", 99, "b"),
        ];
        let out = layers(&host, &named);
        assert_eq!(out[0].cache, "old-namespace");
        assert_eq!(out[0].key, "photo");
        assert_eq!(out[1].cache, "imagery-mine");
    }
}
