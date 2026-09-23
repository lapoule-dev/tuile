// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a render job is told, read once from its environment.
//!
//! Every variable, its default and the reason for it are the contract the
//! launchers write to (`launch::render_env`, `stl-render`). Read through a
//! closure rather than `std::env`, so a test states the environment it means
//! and nothing leaks in from the machine running it.

use std::path::PathBuf;

/// A missing or malformed variable.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required (e.g. JOB_FRAMES=1:1440)")]
    Missing(&'static str),
    #[error("{name}={value:?}: {why}")]
    Bad { name: &'static str, value: String, why: &'static str },
}

/// The render job's configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The inclusive frame range handed to this job (the whole render, or the
    /// chunk an orchestrator cut — see [`Config::chunk_given`]).
    pub frames: (u64, u64),
    pub engine: String,
    pub delegate: String,
    pub tier: String,
    pub width: String,
    pub samples: String,
    pub threshold: String,
    pub gpus: u32,
    pub procs_per_gpu: u32,
    pub batch_frames: String,
    /// The cadence, one value for the whole job — see [`fps_of`].
    pub fps: String,
    /// `render_usd.py` arguments, after `--`.
    pub extra_args: Vec<String>,
    /// Blender's own flags, before `-P`.
    pub blender_args: Vec<String>,
    pub tf_debug: Option<String>,
    pub out: PathBuf,
    pub task_index: u32,
    pub task_count: u32,
    /// The orchestrator cut the range: `frames` is this task's already.
    pub chunk_given: bool,
    /// Where this run's outputs go, without a trailing slash; `None` for a
    /// local run, where nothing leaves the machine.
    pub run_prefix: Option<String>,
    pub archive_every_s: u64,
    pub gpu_sample_s: u64,
    pub bail_sleep_s: u64,
    pub done_sleep_s: u64,
    pub pack_key: Option<String>,
    pub pack_path: PathBuf,
    /// A pack already on disk (`TUILE_PACK`): STL's job fetches it during the
    /// OptiX warm-up and hands it over.
    pub pack_ready: Option<PathBuf>,
    pub scene: Option<String>,
    pub ion_token: bool,
    pub stage_b64_gz: Option<String>,
    pub stage_path: PathBuf,
    pub stage_key: Option<String>,
    pub tape_key: Option<String>,
    pub trajectory: Option<String>,
    pub viewport: String,
    pub sse: String,
    pub optix_cache_key: Option<String>,
    pub optix_cache_path: PathBuf,
    pub ssh_pubkey: Option<String>,
    /// Cycles backends in order of preference.
    pub cycles_backends: Vec<String>,
    /// The tile cache every process is seeded from.
    pub cache_seed: PathBuf,
    pub blender: String,
    pub render_script: String,
    /// Where the engine's own binaries live (`tape-to-stage`, the tape
    /// generators).
    pub tools: PathBuf,
    /// The batch configuration, each overridable (see [`batch_env`]).
    pub batch_env: Vec<(String, String)>,
}

/// Blender's logging, on unless `JOB_TRACE=0`. A render that stopped dead on
/// 22 September 2026 left nothing between "frame 1: start" and silence: the
/// flag existed and was not set. Without wildcards: `--log cycles` already
/// takes every category that starts with "cycles".
pub const TRACE_BLENDER_ARGS: &str =
    "--debug-cycles --log cycles,render,usd,hydra,depsgraph,wm --log-level 2 --log-show-source";

/// The batch-render configuration, as defaults any job can override.
/// Measured 22 September 2026 on one pack, eight renders a side:
///
/// * `CYCLES_BACKGROUND=1` — hdCycles hardcodes an interactive session, in
///   which the render thread parks still flagged as rendering.
/// * `CYCLES_AUTO_TILE=0` — above one 2048² tile Cycles renders to disk and
///   hands the frame back through a callback hdCycles never wires: every
///   render above 4.19 Mpx looped forever.
/// * `TUILE_WAIT_MODE=command` — one blocking wait per frame instead of dozens
///   of 50 ms polls, same images, same times.
///
/// Here, and not in each launcher, because tuile's jobs and STL's must not
/// drift apart on what decides whether a render ends at all.
pub fn batch_env(get: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    [("CYCLES_BACKGROUND", "1"), ("CYCLES_AUTO_TILE", "0"), ("TUILE_WAIT_MODE", "command")]
        .iter()
        .map(|(k, d)| (k.to_string(), get(k).unwrap_or_else(|| d.to_string())))
        .collect()
}

/// The cadence: `JOB_FPS` if given, else the trajectory's second field for
/// `pyrenees` only — `orbit` puts a longitude there, and reading it as a
/// cadence would make a two-frame-a-second film without complaint.
pub fn fps_of(job_fps: Option<&str>, trajectory: Option<&str>) -> String {
    if let Some(f) = job_fps {
        return f.to_string();
    }
    let mut parts = trajectory.unwrap_or("").split(':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("pyrenees"), _, Some(fps)) if !fps.is_empty() => fps.to_string(),
        _ => "24".to_string(),
    }
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

impl Config {
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        // An empty variable is an unset one, as `${X:-default}` had it.
        let var = |k: &str| get(k).filter(|v| !v.is_empty());
        let or = |k: &str, d: &str| var(k).unwrap_or_else(|| d.to_string());
        let number = |name: &'static str, d: u64| -> Result<u64, ConfigError> {
            match var(name) {
                None => Ok(d),
                Some(v) => v.trim().parse().map_err(|_| ConfigError::Bad { name, value: v, why: "not a number" }),
            }
        };
        let frames_s = var("JOB_FRAMES").ok_or(ConfigError::Missing("JOB_FRAMES"))?;
        let bad_frames = || ConfigError::Bad { name: "JOB_FRAMES", value: frames_s.clone(), why: "expected A:B, A <= B" };
        let (a, b) = frames_s.split_once(':').ok_or_else(bad_frames)?;
        let frames: (u64, u64) = (a.trim().parse().map_err(|_| bad_frames())?, b.trim().parse().map_err(|_| bad_frames())?);
        if frames.0 > frames.1 {
            return Err(bad_frames());
        }
        let gpus = number("JOB_GPUS", 1)? as u32;
        let procs_per_gpu = number("JOB_PROCS_PER_GPU", 4)? as u32;
        if gpus == 0 || procs_per_gpu == 0 {
            return Err(ConfigError::Bad {
                name: "JOB_PROCS_PER_GPU",
                value: format!("{gpus} GPU × {procs_per_gpu}"),
                why: "no process to render with",
            });
        }
        let trajectory = var("JOB_TRAJECTORY");
        let blender_args = match var("JOB_BLENDER_ARGS") {
            Some(a) => words(&a),
            None if or("JOB_TRACE", "1") == "1" => words(TRACE_BLENDER_ARGS),
            None => Vec::new(),
        };
        let chunk_given = var("JOB_TASK_INDEX").is_some();
        let task_index = match var("JOB_TASK_INDEX") {
            Some(_) => number("JOB_TASK_INDEX", 0)?,
            None => number("CLOUD_RUN_TASK_INDEX", 0)?,
        } as u32;
        let task_count = match var("JOB_TASK_COUNT") {
            Some(_) => number("JOB_TASK_COUNT", 1)?,
            None => number("CLOUD_RUN_TASK_COUNT", 1)?,
        } as u32;
        if task_count == 0 || task_index >= task_count {
            return Err(ConfigError::Bad {
                name: "JOB_TASK_INDEX",
                value: format!("{task_index} of {task_count}"),
                why: "a task index is below the task count",
            });
        }
        let out = PathBuf::from(or("JOB_OUT", "/out/render.mp4"));
        let outdir = out.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        Ok(Config {
            frames,
            engine: or("JOB_ENGINE", "native"),
            delegate: or("JOB_DELEGATE", "cycles"),
            tier: or("JOB_TIER", "cycles"),
            width: or("JOB_WIDTH", "1920"),
            samples: or("JOB_SAMPLES", "128"),
            threshold: or("JOB_THRESHOLD", "0.05"),
            gpus,
            procs_per_gpu,
            batch_frames: or("JOB_BATCH_FRAMES", "60"),
            fps: fps_of(var("JOB_FPS").as_deref(), trajectory.as_deref()),
            extra_args: words(&or("JOB_EXTRA_ARGS", "")),
            blender_args,
            tf_debug: var("JOB_TF_DEBUG"),
            task_index,
            task_count,
            chunk_given,
            run_prefix: var("JOB_RUN_PREFIX").map(|p| p.trim_end_matches('/').to_string()).filter(|p| !p.is_empty()),
            archive_every_s: number("JOB_ARCHIVE_EVERY", 300)?,
            gpu_sample_s: number("JOB_GPU_SAMPLE", 30)?,
            bail_sleep_s: number("JOB_BAIL_SLEEP", 600)?,
            done_sleep_s: number("JOB_DONE_SLEEP", 60)?,
            pack_key: var("JOB_PACK_KEY"),
            pack_path: PathBuf::from(or("JOB_PACK", "/tmp/scene.tuilepack")),
            pack_ready: var("TUILE_PACK").map(PathBuf::from),
            scene: var("JOB_SCENE"),
            ion_token: var("TUILE_ION_TOKEN").is_some(),
            stage_b64_gz: var("JOB_STAGE_B64_GZ"),
            stage_path: PathBuf::from(or("JOB_STAGE", "/tmp/job-stage.usda")),
            stage_key: var("JOB_STAGE_KEY"),
            tape_key: var("JOB_TAPE_KEY"),
            trajectory,
            viewport: or("JOB_VIEWPORT", "1280x960"),
            sse: or("JOB_SSE", "3"),
            optix_cache_key: var("JOB_OPTIX_CACHE_KEY"),
            optix_cache_path: var("OPTIX_CACHE_PATH").map(PathBuf::from).unwrap_or_else(|| outdir.join("optix-cache")),
            ssh_pubkey: var("JOB_SSH_PUBKEY"),
            cycles_backends: words(&or("JOB_CYCLES_BACKENDS", "OPTIX CUDA")),
            cache_seed: PathBuf::from(or("TUILE_CACHE_DIR", "/tmp/tuile-cache")),
            blender: or("TUILE_BLENDER", "blender"),
            render_script: or("TUILE_RENDER_SCRIPT", "/opt/render/render_usd.py"),
            tools: PathBuf::from(or("TUILE_TOOLS", "/opt/tuile/bin")),
            batch_env: batch_env(&|k| var(k)),
            out,
        })
    }

    /// The directory everything local lives in: segments, logs, archives.
    pub fn outdir(&self) -> PathBuf {
        self.out.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
    }

    /// One suffix per task on everything a task archives: N tasks writing one
    /// `logs.tar.gz` overwrite each other, and the survivor is whoever
    /// finished last — never the one that failed.
    pub fn task_tag(&self) -> String {
        if self.task_count > 1 {
            format!("-t{}", self.task_index)
        } else {
            String::new()
        }
    }

    /// The pack is the source of the ground, or streaming needs a token.
    pub fn has_source(&self) -> bool {
        self.engine != "hydra" || self.pack_key.is_some() || self.pack_ready.is_some() || self.ion_token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn the_defaults_are_the_scripts_defaults() {
        let c = Config::from_env(&env(&[("JOB_FRAMES", "1:48")])).expect("test");
        assert_eq!(c.frames, (1, 48));
        assert_eq!((c.engine.as_str(), c.delegate.as_str(), c.tier.as_str()), ("native", "cycles", "cycles"));
        assert_eq!((c.width.as_str(), c.samples.as_str(), c.threshold.as_str()), ("1920", "128", "0.05"));
        assert_eq!((c.gpus, c.procs_per_gpu), (1, 4));
        assert_eq!(c.fps, "24");
        assert_eq!(c.blender_args, words(TRACE_BLENDER_ARGS), "Blender logs by default");
        assert_eq!(c.out, PathBuf::from("/out/render.mp4"));
        assert_eq!((c.task_index, c.task_count, c.chunk_given), (0, 1, false));
        assert_eq!(c.run_prefix, None);
        assert_eq!(c.cycles_backends, ["OPTIX", "CUDA"]);
        assert_eq!(c.optix_cache_path, PathBuf::from("/out/optix-cache"));
        assert_eq!(c.task_tag(), "");
        assert_eq!(
            c.batch_env,
            [("CYCLES_BACKGROUND", "1"), ("CYCLES_AUTO_TILE", "0"), ("TUILE_WAIT_MODE", "command")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }

    #[test]
    fn frames_are_required_and_ordered() {
        assert_eq!(Config::from_env(&env(&[])), Err(ConfigError::Missing("JOB_FRAMES")));
        assert!(Config::from_env(&env(&[("JOB_FRAMES", "10:2")])).is_err());
        assert!(Config::from_env(&env(&[("JOB_FRAMES", "x")])).is_err());
    }

    #[test]
    fn the_cadence_comes_from_job_fps_or_a_pyrenees_trajectory_only() {
        assert_eq!(fps_of(Some("60"), Some("pyrenees:2:30")), "60");
        assert_eq!(fps_of(None, Some("pyrenees:2:30:50000")), "30");
        assert_eq!(fps_of(None, Some("orbit:1440:2.17:42.52")), "24", "orbit's second field is a longitude");
        assert_eq!(fps_of(None, None), "24");
    }

    #[test]
    fn an_orchestrator_names_the_task_over_cloud_run() {
        let c = Config::from_env(&env(&[
            ("JOB_FRAMES", "601:1200"),
            ("JOB_TASK_INDEX", "1"),
            ("JOB_TASK_COUNT", "14"),
            ("CLOUD_RUN_TASK_INDEX", "0"),
            ("CLOUD_RUN_TASK_COUNT", "1"),
        ]))
        .expect("test");
        assert_eq!((c.task_index, c.task_count, c.chunk_given), (1, 14, true));
        assert_eq!(c.task_tag(), "-t1");
        let cr = Config::from_env(&env(&[("JOB_FRAMES", "1:90"), ("CLOUD_RUN_TASK_INDEX", "2"), ("CLOUD_RUN_TASK_COUNT", "3")])).expect("test");
        assert_eq!((cr.task_index, cr.task_count, cr.chunk_given), (2, 3, false));
        assert!(Config::from_env(&env(&[("JOB_FRAMES", "1:9"), ("JOB_TASK_INDEX", "3"), ("JOB_TASK_COUNT", "3")])).is_err());
    }

    #[test]
    fn words_split_as_the_shell_did_and_empties_are_unset() {
        let c = Config::from_env(&env(&[
            ("JOB_FRAMES", "1:2"),
            ("JOB_EXTRA_ARGS", "--demo-fixups  --no-dof"),
            ("JOB_BLENDER_ARGS", ""),
            ("JOB_TRACE", "0"),
            ("JOB_RUN_PREFIX", "renders/r1/"),
            ("JOB_CYCLES_BACKENDS", "CUDA"),
        ]))
        .expect("test");
        assert_eq!(c.extra_args, ["--demo-fixups", "--no-dof"]);
        assert!(c.blender_args.is_empty(), "JOB_TRACE=0 and nothing given");
        assert_eq!(c.run_prefix.as_deref(), Some("renders/r1"));
        assert_eq!(c.cycles_backends, ["CUDA"]);
    }

    #[test]
    fn hydra_needs_a_pack_or_a_token() {
        let base = [("JOB_FRAMES", "1:2"), ("JOB_ENGINE", "hydra")];
        assert!(!Config::from_env(&env(&base)).expect("test").has_source());
        assert!(Config::from_env(&env(&[base[0], base[1], ("JOB_PACK_KEY", "p")])).expect("test").has_source());
        assert!(Config::from_env(&env(&[base[0], base[1], ("TUILE_PACK", "/tmp/p")])).expect("test").has_source());
        assert!(Config::from_env(&env(&[("JOB_FRAMES", "1:2")])).expect("test").has_source(), "native needs neither");
    }
}
