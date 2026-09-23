// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Launching a farm run: the service every launcher and workflow shares.
//!
//! A launcher is a thin face over three steps — start an execution of a job
//! definition, follow it to completion, bring the film home and open it. The
//! film itself is assembled BY THE JOB (the last task to finish, from the
//! receipts); nothing here assembles, concatenates or repairs. Launchers differ
//! only in how they describe a scene (tuile: a trajectory and bake settings;
//! a product: a planned run), so the verbs and the shared flags live here, in
//! [`CommonArgs`], and every launcher's command line reads the same.

pub mod tuile;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::jobs::{Execution, JobsApi, JobsError};
use crate::{RunStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Jobs(#[from] JobsError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, LaunchError>;

/// The flags every launcher takes, with the same names everywhere.
///
/// Defaults that differ between products (how many tasks, how many samples)
/// are `Option`s here and settled by each launcher.
///
/// Serializable too: a workflow carries them from the command line that
/// submitted it to the worker that runs it.
#[derive(clap::Args, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CommonArgs {
    /// Frame range, inclusive: `A:B`.
    #[arg(long)]
    pub frames: Option<String>,
    /// Parallel tasks, one GPU each. Above the service's concurrency the extra
    /// tasks wait their turn inside the same execution.
    #[arg(long)]
    pub tasks: Option<u32>,
    /// Concurrent renders per GPU.
    #[arg(long, default_value_t = 4)]
    pub procs_per_gpu: u32,
    /// Maximum samples per pixel.
    #[arg(long)]
    pub samples: Option<u32>,
    /// Adaptive noise threshold.
    #[arg(long)]
    pub threshold: Option<f64>,
    /// Progress log granularity, in frames.
    #[arg(long)]
    pub batch_frames: Option<u32>,
    /// Extra arguments for the render driver.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub extra: String,
    /// Extra arguments for Blender itself, before `-P`.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub blender_args: String,
    /// Extra environment for the job, `K=V`, repeatable.
    #[arg(long = "env", value_name = "K=V")]
    pub env: Vec<String>,
    /// Structured determinism traces in the job's archive.
    #[arg(long)]
    pub trace: bool,
    /// Ask the service to validate the request without running anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Launch and return without following the execution.
    #[arg(long)]
    pub no_watch: bool,
    /// Follow an execution already running, by name, instead of launching.
    #[arg(long)]
    pub attach: Option<String>,
    /// Where the film lands locally, to be opened.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

impl CommonArgs {
    /// `--env K=V` pairs, refused when malformed.
    pub fn extra_env(&self) -> Result<Vec<(String, String)>> {
        self.env
            .iter()
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) if !k.is_empty() && !v.is_empty() => Ok((k.to_string(), v.to_string())),
                _ => Err(LaunchError::Refused(format!("--env wants K=V, got: {pair}"))),
            })
            .collect()
    }

    /// The render settings every engine job reads, the same for every product.
    /// The defaults that differ by product are passed in.
    pub fn render_env(&self, samples_default: u32, batch_default: u32) -> Vec<(String, String)> {
        let mut env = vec![
            ("JOB_SAMPLES".into(), self.samples.unwrap_or(samples_default).to_string()),
            ("JOB_THRESHOLD".into(), fmt_float(self.threshold.unwrap_or(0.05))),
            // One task is one GPU, by the service's contract; the job's probe
            // must agree or it bails without rendering a CPU frame.
            ("JOB_GPUS".into(), "1".into()),
            ("JOB_PROCS_PER_GPU".into(), self.procs_per_gpu.to_string()),
            ("JOB_BATCH_FRAMES".into(), self.batch_frames.unwrap_or(batch_default).to_string()),
            ("JOB_EXTRA_ARGS".into(), self.extra.clone()),
            ("JOB_BLENDER_ARGS".into(), self.blender_args.clone()),
        ];
        if self.trace {
            env.push(("TUILE_LOG".into(), "tuile_det=info".into()));
            env.push(("TUILE_LOG_FORMAT".into(), "json".into()));
        }
        env
    }
}

/// A float the way the launchers have always printed it: `16.0`, `0.05`,
/// `2.17` — shortest round-trip with a `.0` on whole numbers. Pack keys are
/// hashes of such strings, so this is a compatibility contract, not taste.
pub fn fmt_float(x: f64) -> String {
    format!("{x:?}")
}

/// A number the way a cadence is printed: `24`, `29.97`.
pub fn fmt_number(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// One launch: which job, with what environment, how many tasks.
#[derive(Debug, Clone)]
pub struct Launch {
    pub job: String,
    pub env: Vec<(String, String)>,
    pub tasks: u32,
}

/// Values that must never reach a manifest or a log.
pub const SECRET_KEYS: [&str; 5] = [
    "TUILE_ION_TOKEN",
    "RUNPOD_API_KEY",
    "R2_SECRET_ACCESS_KEY",
    "TUILE_STORE_SECRET_ACCESS_KEY",
    "TUILE_STORE_ACCESS_KEY_ID",
];

/// `env` with its secrets blanked, for a manifest: the archive is durable and
/// shared, and a token that goes in never comes out.
pub fn redacted(env: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    env.iter()
        .map(|(k, v)| {
            let v = if SECRET_KEYS.contains(&k.as_str()) { "<redacted>".to_string() } else { v.clone() };
            (k.clone(), serde_json::Value::String(v))
        })
        .collect()
}

/// The jobs client and the store, together: what a launch needs.
pub struct Farm {
    pub jobs: JobsApi,
    pub store: Arc<dyn RunStore>,
}

impl Farm {
    /// A small object's text, or `None` when absent.
    pub async fn read_text(&self, key: &str) -> Result<Option<String>> {
        // `list` works by directory: the object is found in its parent's listing.
        let parent = key.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        match self.store.list(parent).await?.into_iter().find(|e| e.key == key) {
            Some(e) => Ok(Some(String::from_utf8_lossy(&self.store.get_range(key, 0..e.size).await?).to_string())),
            None => Ok(None),
        }
    }

    /// A small JSON document, written to `key`.
    pub async fn put_json(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        use std::io::Write as _;
        let mut file = tempfile::NamedTempFile::new().map_err(|e| LaunchError::Refused(e.to_string()))?;
        file.write_all(serde_json::to_string_pretty(value).unwrap_or_default().as_bytes())
            .map_err(|e| LaunchError::Refused(e.to_string()))?;
        self.store.put(file.path(), key).await?;
        Ok(())
    }

    /// Starts the execution. `None` for a validated dry run.
    pub async fn start(&self, launch: &Launch, dry_run: bool) -> Result<Option<String>> {
        let name = self.jobs.run(&launch.job, &launch.env, launch.tasks, dry_run).await?;
        match &name {
            Some(n) => println!(
                "execution: {}  {} task(s)",
                n.rsplit('/').next().unwrap_or(n),
                launch.tasks
            ),
            None => println!("validateOnly: the request is accepted; nothing ran and nothing is billed."),
        }
        Ok(name)
    }

    /// Follows an execution to completion, one line per change.
    pub async fn follow(&self, execution: &str) -> Result<Execution> {
        let ex = self
            .jobs
            .wait(execution, Duration::from_secs(20), |ex, changed| {
                if changed {
                    println!("  {}", ex.line());
                }
            })
            .await?;
        if let Some(uri) = &ex.log_uri {
            println!("  logs: {uri}");
        }
        Ok(ex)
    }

    /// Brings the film the job assembled to `out` and opens it.
    ///
    /// Opened, because a render is a picture and every instrument that stands
    /// in for looking at it has lied here: `RENDER-DONE` over four-fifths of a
    /// missing film, `frames: 2/2` over a globe come out flat pink.
    pub async fn fetch_film(&self, key: &str, out: &Path) -> Result<()> {
        if !self.store.exists(key).await? {
            return Err(LaunchError::Refused(format!(
                "every task succeeded and {key} does not exist — read FILM- in the tasks' logs"
            )));
        }
        let bytes = self.store.get(key, out).await?;
        println!("film:    {} ({:.1} MB)", out.display(), bytes as f64 / 1e6);
        let _ = std::process::Command::new("open").arg(out).status();
        Ok(())
    }

    /// The whole face of a launcher: start (or attach), follow, fetch.
    pub async fn launch_and_follow(
        &self,
        launch: &Launch,
        common: &CommonArgs,
        film: Option<(&str, &Path)>,
    ) -> Result<()> {
        let execution = match &common.attach {
            Some(name) if name.contains('/') => name.clone(),
            Some(name) => format!("{}/executions/{name}", self.jobs.job_path(&launch.job)),
            None => match self.start(launch, common.dry_run).await? {
                Some(name) => name,
                None => return Ok(()),
            },
        };
        if common.no_watch {
            return Ok(());
        }
        let ex = self.follow(&execution).await?;
        println!("{} succeeded, {} failed", ex.succeeded_count, ex.bad());
        if ex.bad() > 0 {
            return Err(LaunchError::Refused(format!(
                "{} task(s) failed — their logs are logs-t*.tar.gz under the run",
                ex.bad()
            )));
        }
        if let Some((key, out)) = film {
            self.fetch_film(key, out).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_as_the_pack_keys_expect() {
        assert_eq!(fmt_float(16.0), "16.0");
        assert_eq!(fmt_float(3.0), "3.0");
        assert_eq!(fmt_float(2.17), "2.17");
        assert_eq!(fmt_float(0.05), "0.05");
        assert_eq!(fmt_number(24.0), "24");
        assert_eq!(fmt_number(29.97), "29.97");
    }

    #[test]
    fn secrets_never_reach_the_manifest() {
        let env = vec![
            ("TUILE_ION_TOKEN".to_string(), "abc".to_string()),
            ("JOB_FRAMES".to_string(), "1:4".to_string()),
        ];
        let out = redacted(&env);
        assert_eq!(out["TUILE_ION_TOKEN"], "<redacted>");
        assert_eq!(out["JOB_FRAMES"], "1:4");
    }

    #[test]
    fn a_malformed_env_pair_is_refused() {
        let args = CommonArgs { env: vec!["NOVALUE".into()], ..Default::default() };
        assert!(args.extra_env().is_err());
    }
}
