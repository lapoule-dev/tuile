// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The engine's farm launcher.
//!
//! ```text
//! launch-job bake   --trajectory orbit:4:2.17:42.52:8000:5000 --frames 1:4 --viewport 960x540 --sse 16
//! launch-job render --engine hydra --trajectory … --frames 1:4 --tasks 2 --width 960 --extra '--height 540'
//! ```
//!
//! The same verbs and shared flags as every other launcher built on
//! `tuile_farm::launch`; the scene flags are the engine's. It starts an
//! execution, follows it to completion and brings home the film the job
//! assembled. It assembles nothing itself.
//!
//! Environment: `GOOGLE_APPLICATION_CREDENTIALS` (a service account key) or a
//! `gcloud` login for the job service; `TUILE_STORE_*` for the bucket;
//! `TUILE_GCP_PROJECT` / `TUILE_GCP_REGION` to point elsewhere.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tuile_farm::jobs::{default_tokens, HttpTransport, JobsApi};
use tuile_farm::launch::{tuile, CommonArgs, Farm};
use tuile_farm::ObjectRunStore;

#[derive(Parser)]
#[command(name = "launch-job", about = "Bake and render the engine's scenes on the farm")]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Bake a trajectory into a pack (no GPU).
    Bake {
        #[command(flatten)]
        common: CommonArgs,
        #[command(flatten)]
        scene: tuile::SceneArgs,
    },
    /// Render a scene, baking its pack first when a hydra render has none.
    Render {
        #[command(flatten)]
        common: CommonArgs,
        #[command(flatten)]
        scene: tuile::SceneArgs,
    },
}

fn farm() -> Result<Farm, String> {
    let project = std::env::var("TUILE_GCP_PROJECT").unwrap_or_else(|_| "first-parser-498510-a4".into());
    let region = std::env::var("TUILE_GCP_REGION").unwrap_or_else(|_| "europe-west1".into());
    let transport = Arc::new(HttpTransport::new().map_err(|e| e.to_string())?);
    let tokens = default_tokens().map_err(|e| e.to_string())?;
    let jobs = JobsApi::new(transport, tokens, "https://run.googleapis.com/v2", &project, &region);
    let store = ObjectRunStore::from_env().map_err(|e| e.to_string())?;
    Ok(Farm { jobs, store: Arc::new(store) })
}

/// `videos/` at the root of the checkout: a film that only lives in `/tmp` is
/// one reboot away from gone.
fn videos() -> PathBuf {
    let here = std::env::current_dir().unwrap_or_default();
    here.ancestors()
        .find(|d| d.join("Cargo.lock").exists())
        .unwrap_or(&here)
        .join("videos")
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let farm = match farm() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("launch-job: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match &cli.verb {
        Verb::Bake { common, scene } => tuile::bake(&farm, scene, common).await.map(|_| ()),
        Verb::Render { common, scene } => tuile::render(&farm, scene, common, &videos()).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("launch-job: {e}");
            ExitCode::FAILURE
        }
    }
}
