// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The engine's own launcher: a scene described by a trajectory and bake
//! settings, baked into a pack when none exists, then rendered.
//!
//! Ported from `integrations/blender/launch_job.py`, Cloud Run path only — the
//! renders run there, and the pod path lives on in git history.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{fmt_float, fmt_number, redacted, CommonArgs, Farm, Launch, LaunchError, Result};

/// Where the engine's runs are filed in the bucket.
pub const RUN_PREFIX: &str = "renders";
pub const RENDER_JOB: &str = "tuile-render";
pub const BAKE_JOB: &str = "tuile-bake";

/// How a tuile scene is described — the flags only this launcher takes.
#[derive(clap::Args, Debug, Clone)]
pub struct SceneArgs {
    /// The generative road: `orbit:frames:lon:lat:radius_m:alt_m`,
    /// `pyrenees:minutes:fps:alt_m:offset_deg`, `zoom:frames`.
    #[arg(long, default_value = "")]
    pub trajectory: String,
    #[arg(long, default_value = "1280x960")]
    pub viewport: String,
    #[arg(long, default_value_t = 3.0)]
    pub sse: f64,
    /// Imagery asset id (0: the engine's default).
    #[arg(long, default_value_t = 0)]
    pub imagery: i64,
    /// Terrain asset id (0: the engine's default).
    #[arg(long, default_value_t = 0)]
    pub terrain: i64,
    #[arg(long, default_value_t = 1)]
    pub imagery_boost: i64,
    /// A tape to bake as is, instead of generating one from --trajectory.
    #[arg(long)]
    pub tape: Option<PathBuf>,
    /// A pre-baked pack's key; without it a hydra render bakes first.
    #[arg(long, default_value = "")]
    pub pack: String,
    /// The scene digest the pack must answer.
    #[arg(long, default_value = "")]
    pub scene: String,
    /// A local .usda to render, embedded gzip+base64.
    #[arg(long)]
    pub stage: Option<PathBuf>,
    /// A .usda already in the bucket, for stages too large for the env.
    #[arg(long, default_value = "")]
    pub stage_key: String,
    #[arg(long, default_value = "native", value_parser = ["native", "hydra"])]
    pub engine: String,
    #[arg(long, default_value = "cycles", value_parser = ["cycles", "eevee"])]
    pub tier: String,
    #[arg(long, default_value = "cycles", value_parser = ["storm", "cycles"])]
    pub delegate: String,
    #[arg(long, default_value_t = 1920)]
    pub width: u32,
    #[arg(long, default_value_t = 12)]
    pub resident_gb: u32,
    /// TUILE_PROFILE for the job (e.g. `cpu,heap`).
    #[arg(long, default_value = "")]
    pub profile: String,
}

/// A supplied tape's content, as sixteen hex digits.
///
/// Only present when there IS a tape: adding the term to the ordinary case
/// would rename every pack already baked (`03eb228f774ab939` became
/// `6488c3831e9c5445` when tried), and every film in `videos/` would point at
/// an object that does not exist.
pub fn tape_digest(path: Option<&Path>) -> std::io::Result<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    let bytes = std::fs::read(path)?;
    Ok(Some(hex16(&Sha256::digest(&bytes))))
}

fn hex16(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
}

/// Where the pack will be filed, computed BEFORE anything is launched, from
/// the bake parameters and nothing else — so the launcher and the job agree on
/// it without either telling the other, and a launcher that dies mid-bake
/// leaves the pack where the next one looks.
///
/// The scene digest is NOT the name: it covers resolved settings only the job
/// knows. It stays inside the pack, where `--scene` checks it at open.
pub fn pack_key(scene: &SceneArgs, frames: &str) -> std::io::Result<String> {
    let (first, last) = frames.split_once(':').unwrap_or((frames, frames));
    let mut canonical = [
        format!("trajectory={}", scene.trajectory),
        format!("frames={frames}"),
        format!("viewport={}", scene.viewport),
        format!("sse={}", fmt_float(scene.sse)),
        format!("imagery_boost={}", scene.imagery_boost),
        format!("imagery={}", scene.imagery),
        format!("terrain={}", scene.terrain),
    ]
    .join("\n");
    if let Some(tape) = tape_digest(scene.tape.as_deref())? {
        canonical.push_str(&format!("\ntape={tape}"));
    }
    Ok(format!("packs/{}/{first}-{last}.tuilepack", hex16(&Sha256::digest(canonical.as_bytes()))))
}

/// The cadence a trajectory describes: `pyrenees` carries it second, the
/// others count frames and take the default.
pub fn fps_of(trajectory: &str) -> f64 {
    let parts: Vec<&str> = trajectory.split(':').collect();
    if parts.first() != Some(&"pyrenees") {
        return 24.0;
    }
    match parts.get(2).and_then(|p| p.parse::<f64>().ok()) {
        Some(fps) if fps > 0.0 => fps,
        _ => 24.0,
    }
}

/// How many frames a trajectory describes, when it says.
pub fn frames_of(trajectory: &str) -> Option<u64> {
    let parts: Vec<&str> = trajectory.split(':').collect();
    match *parts.first()? {
        "pyrenees" => {
            let minutes = match parts.get(1) {
                Some(m) if !m.is_empty() => m.parse::<f64>().ok()?,
                _ => 2.0,
            };
            Some((minutes * 60.0 * fps_of(trajectory)).round() as u64)
        }
        "orbit" => match parts.get(1) {
            Some(n) if !n.is_empty() => n.parse().ok(),
            _ => Some(1440),
        },
        _ => None,
    }
}

/// `--frames` against what the trajectory describes: too far is an error (the
/// tape has no such poses), short is legitimate but said — it is the only
/// difference between a slice on purpose and an amputated film.
pub fn check_frames(trajectory: &str, frames: &str) -> Result<Option<String>> {
    let Some(expected) = frames_of(trajectory) else { return Ok(None) };
    let last = frames.split_once(':').map(|(_, b)| b).unwrap_or(frames);
    let Ok(last) = last.parse::<u64>() else { return Ok(None) };
    if last > expected {
        return Err(LaunchError::Refused(format!(
            "--frames goes to {last} and the trajectory only describes {expected}: the tape has no such poses"
        )));
    }
    if last < expected {
        let fps = fps_of(trajectory);
        return Ok(Some(format!(
            "partial: {last} frames of {expected} ({:.1} s of film of {:.1} s)",
            last as f64 / fps,
            expected as f64 / fps
        )));
    }
    Ok(None)
}

/// The ion token, from the environment or the nearest `.env` up the tree.
/// Never printed.
pub fn ion_token() -> Result<String> {
    if let Ok(t) = std::env::var("CESIUM_ION_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let dotenv = d.join(".env");
        if let Ok(text) = std::fs::read_to_string(&dotenv) {
            if let Some(t) = text.lines().find_map(|l| l.strip_prefix("CESIUM_ION_TOKEN=")) {
                return Ok(t.trim().to_string());
            }
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Err(LaunchError::Refused("CESIUM_ION_TOKEN not found (env or .env) — hydra does not bake without it".into()))
}

/// The engine checkout's commit, and whether it was dirty — without which a
/// manifest does not say which code made the picture.
pub fn git_state() -> serde_json::Value {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    serde_json::json!({
        "commit": git(&["rev-parse", "HEAD"]),
        "short": git(&["rev-parse", "--short", "HEAD"]),
        "branch": git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        "dirty": !git(&["status", "--porcelain"]).is_empty(),
    })
}

/// Sorted alphabetically = sorted by date, commit readable in the name, and a
/// random suffix separating two runs of the same second.
pub fn run_id(git: &serde_json::Value) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let sha = git["short"].as_str().filter(|s| !s.is_empty()).unwrap_or("nogit");
    let dirty = if git["dirty"].as_bool() == Some(true) { "-dirty" } else { "" };
    format!("{stamp}-{sha}{dirty}-{:04x}", rand::random::<u16>())
}

/// Everything the bake job reads.
pub fn bake_env(
    scene: &SceneArgs,
    frames: &str,
    ion: &str,
    pack_key: &str,
    run_prefix: &str,
    tape_key: Option<&str>,
) -> Vec<(String, String)> {
    let s = |v: &str| v.to_string();
    vec![
        (s("JOB_FRAMES"), s(frames)),
        (s("JOB_TRAJECTORY"), scene.trajectory.clone()),
        (s("JOB_VIEWPORT"), scene.viewport.clone()),
        (s("JOB_SSE"), fmt_float(scene.sse)),
        (s("TUILE_IMAGERY_BOOST"), scene.imagery_boost.to_string()),
        (s("JOB_IMAGERY_ASSET"), if scene.imagery != 0 { scene.imagery.to_string() } else { String::new() }),
        (s("JOB_TERRAIN_ASSET"), if scene.terrain != 0 { scene.terrain.to_string() } else { String::new() }),
        // The pack, its tape (.mcap) and its scene digest (.scene) go under
        // this key; the job puts them there.
        (s("JOB_PACK_KEY"), s(pack_key)),
        (s("JOB_RUN_PREFIX"), s(run_prefix)),
        (s("TUILE_ION_TOKEN"), s(ion)),
        // The resident budget reached only the render once, and a bake ran at
        // the code's 4 GiB in a 32 GiB container: 134 720 loads for 234 tiles.
        (s("TUILE_RESIDENT_BUDGET_GB"), scene.resident_gb.to_string()),
        (s("JOB_FPS"), fmt_number(fps_of(&scene.trajectory))),
        (s("JOB_TAPE_KEY"), tape_key.unwrap_or("").to_string()),
    ]
}

/// The OptiX cache key: the card AND the image digest, so changed kernels or
/// another card never reuse a stale cache.
pub fn optix_key(digest: Option<&str>) -> String {
    let d: String = digest.unwrap_or("nodigest").replace(':', "-").chars().take(19).collect();
    format!("cache/optix/nvidia-l4-{d}.tar.gz")
}

fn manifest(
    run: &str,
    git: &serde_json::Value,
    kind: &str,
    env: &[(String, String)],
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut m = serde_json::json!({
        "run_id": run,
        "launched_utc": chrono::Utc::now().to_rfc3339(),
        "argv": std::env::args().skip(1).collect::<Vec<_>>(),
        "git": git,
        "kind": kind,
        "job_env": serde_json::Value::Object(redacted(env)),
    });
    if let (Some(m), Some(extra)) = (m.as_object_mut(), extra.as_object()) {
        m.extend(extra.clone());
    }
    m
}

/// Job A: bake `--trajectory` into a pack, deposited by the job itself.
pub async fn bake(farm: &Farm, scene: &SceneArgs, common: &CommonArgs) -> Result<String> {
    if scene.trajectory.is_empty() {
        return Err(LaunchError::Refused("bake wants --trajectory (e.g. orbit:1440:2.17:42.52:8000:5000)".into()));
    }
    let frames = common.frames.clone().ok_or_else(|| LaunchError::Refused("--frames A:B is required".into()))?;
    if let Some(note) = check_frames(&scene.trajectory, &frames)? {
        println!("{note}");
    }
    let ion = ion_token()?;
    let git = git_state();
    let run = run_id(&git);
    let prefix = format!("{RUN_PREFIX}/{run}");
    let key = pack_key(scene, &frames).map_err(|e| LaunchError::Refused(e.to_string()))?;
    let mut tape_key = None;
    if let Some(tape) = &scene.tape {
        // Filed with this run's archive: it is an input of THIS launch, and
        // keeping it with its logs is what makes the bake repeatable.
        let k = format!("{prefix}/traj.mcap");
        farm.store.put(tape, &k).await?;
        println!("tape:    baked as supplied");
        tape_key = Some(k);
    }
    let mut env = bake_env(scene, &frames, &ion, &key, &prefix, tape_key.as_deref());
    env.extend(common.extra_env()?);
    farm.put_json(&format!("{prefix}/config.json"), &manifest(&run, &git, "bake", &env, serde_json::json!({"pack_key": key}))).await?;
    println!("archive: {prefix}/");
    println!("pack:    {key}");
    let launch = Launch { job: BAKE_JOB.into(), env, tasks: 1 };
    farm.launch_and_follow(&launch, common, None).await?;
    println!("render with: --pack {key}");
    Ok(key)
}

/// Job B, baking first when a hydra render has no pack.
pub async fn render(farm: &Farm, scene: &SceneArgs, common: &CommonArgs, videos: &Path) -> Result<()> {
    let frames = common.frames.clone().ok_or_else(|| LaunchError::Refused("--frames A:B is required".into()))?;
    if !scene.trajectory.is_empty() {
        if let Some(note) = check_frames(&scene.trajectory, &frames)? {
            println!("{note}");
        }
    }
    let mut scene = scene.clone();
    if scene.engine == "hydra" && scene.pack.is_empty() {
        if scene.trajectory.is_empty() {
            return Err(LaunchError::Refused("a hydra render wants --trajectory (the scene) or --pack (a baked one)".into()));
        }
        let key = pack_key(&scene, &frames).map_err(|e| LaunchError::Refused(e.to_string()))?;
        if farm.store.exists(&key).await? {
            println!("pack already baked: {key}");
        } else {
            println!("no pack, baking first: {key}");
            let bake_common = CommonArgs { attach: None, no_watch: false, dry_run: false, ..common.clone() };
            bake(farm, &scene, &bake_common).await?;
        }
        scene.pack = key;
    }
    if scene.engine == "hydra" && scene.scene.is_empty() && !scene.pack.is_empty() {
        scene.scene = farm.read_text(&format!("{}.scene", scene.pack)).await?.unwrap_or_default().trim().to_string();
    }
    if scene.stage.is_none() && scene.stage_key.is_empty() && scene.trajectory.is_empty() {
        return Err(LaunchError::Refused("--stage, --stage-key or --trajectory is required".into()));
    }

    let s = |v: &str| v.to_string();
    let mut env: Vec<(String, String)> = vec![
        (s("JOB_FRAMES"), frames.clone()),
        (s("JOB_ENGINE"), scene.engine.clone()),
        (s("JOB_DELEGATE"), scene.delegate.clone()),
        (s("JOB_TIER"), scene.tier.clone()),
        (s("JOB_WIDTH"), scene.width.to_string()),
        (s("JOB_OUT"), s("/out/render.mp4")),
    ];
    env.extend(common.render_env(128, 60));
    if let Some(stage) = &scene.stage {
        let raw = std::fs::read(stage).map_err(|e| LaunchError::Refused(format!("{}: {e}", stage.display())))?;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).map_err(|e| LaunchError::Refused(e.to_string()))?;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, gz.finish().unwrap_or_default());
        if b64.len() > 48_000 {
            return Err(LaunchError::Refused(format!(
                "stage too large for an environment ({} bytes in base64) — deposit it and pass --stage-key",
                b64.len()
            )));
        }
        env.push((s("JOB_STAGE_B64_GZ"), b64));
    }
    if !scene.stage_key.is_empty() {
        env.push((s("JOB_STAGE_KEY"), scene.stage_key.clone()));
    }
    if !scene.trajectory.is_empty() {
        env.push((s("JOB_TRAJECTORY"), scene.trajectory.clone()));
        env.push((s("JOB_SSE"), fmt_float(scene.sse)));
        env.push((s("JOB_VIEWPORT"), scene.viewport.clone()));
        env.push((s("JOB_FPS"), fmt_number(fps_of(&scene.trajectory))));
    }
    if scene.engine == "hydra" {
        if !scene.pack.is_empty() {
            env.push((s("JOB_PACK_KEY"), scene.pack.clone()));
            if !scene.scene.is_empty() {
                env.push((s("JOB_SCENE"), scene.scene.clone()));
            }
            // The tape the bake left beside the pack is what gets flown: a
            // regenerated trajectory is another flight once a generator moved.
            let tape = format!("{}.mcap", scene.pack);
            if farm.store.exists(&tape).await? {
                env.push((s("JOB_TAPE_KEY"), tape));
                println!("tape:    the pack's own, flown as baked");
            }
        } else {
            env.push((s("TUILE_ION_TOKEN"), ion_token()?));
            env.push((s("TUILE_CACHE_DIR"), s("/tmp/tuile-cache")));
        }
        env.push((s("TUILE_RESIDENT_BUDGET_GB"), scene.resident_gb.to_string()));
    }
    env.extend(common.extra_env()?);

    let git = git_state();
    let run = run_id(&git);
    let prefix = format!("{RUN_PREFIX}/{run}");
    let image = farm.jobs.job_image(RENDER_JOB).await?;
    let digest = farm.jobs.image_digest(&image).await.ok();
    env.push((s("JOB_RUN_PREFIX"), prefix.clone()));
    env.push((s("JOB_OPTIX_CACHE_KEY"), optix_key(digest.as_deref())));
    if !scene.profile.is_empty() {
        env.push((s("TUILE_PROFILE"), scene.profile.clone()));
        env.push((s("TUILE_PROFILE_DIR"), s("/out/profile")));
    }
    let tasks = common.tasks.unwrap_or(1);
    farm.put_json(
        &format!("{prefix}/config.json"),
        &manifest(&run, &git, "render", &env, serde_json::json!({
            "image": image,
            "image_digest": digest,
            "backend": {"job": RENDER_JOB, "tasks": tasks},
        })),
    )
    .await?;
    println!("archive: {prefix}/");
    let out = common.out.clone().unwrap_or_else(|| videos.join(format!("{run}.mp4")));
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let film = format!("{prefix}/render.mp4");
    let launch = Launch { job: RENDER_JOB.into(), env, tasks };
    farm.launch_and_follow(&launch, common, Some((&film, &out))).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        scene: SceneArgs,
    }

    fn scene(args: &[&str]) -> SceneArgs {
        Probe::parse_from(std::iter::once("probe").chain(args.iter().copied())).scene
    }

    #[test]
    fn the_pack_key_matches_the_packs_already_baked() {
        // Baked on 23 September 2026 by the Python launcher, then rendered:
        // `packs/226d226cfaad2acb/1-4.tuilepack` is in the bucket. A key that
        // drifts by one character makes every baked pack unreachable.
        let s = scene(&["--trajectory", "orbit:4:2.17:42.52:8000:5000", "--viewport", "960x540", "--sse", "16"]);
        assert_eq!(pack_key(&s, "1:4").expect("key"), "packs/226d226cfaad2acb/1-4.tuilepack");
    }

    #[test]
    fn a_supplied_tape_renames_the_pack_and_only_then() {
        let plain = scene(&["--trajectory", "orbit:4:2.17:42.52:8000:5000", "--viewport", "960x540", "--sse", "16"]);
        let dir = tempfile::tempdir().expect("tempdir");
        let tape = dir.path().join("t.mcap");
        std::fs::write(&tape, b"poses").expect("write");
        let mut taped = plain.clone();
        taped.tape = Some(tape);
        assert_ne!(pack_key(&plain, "1:4").expect("a"), pack_key(&taped, "1:4").expect("b"));
    }

    #[test]
    fn every_baking_parameter_changes_the_key_and_nothing_else_does() {
        let base = scene(&["--trajectory", "orbit:4:2.17:42.52:8000:5000", "--viewport", "960x540", "--sse", "16"]);
        let key = |s: &SceneArgs| pack_key(s, "1:4").expect("key");
        let reference = key(&base);
        // The same parameters always give the same key.
        assert_eq!(reference, key(&base.clone()));
        let changes: [fn(&mut SceneArgs); 6] = [
            |s| s.viewport = "1280x960".into(),
            |s| s.sse = 8.0,
            |s| s.imagery_boost = 2,
            |s| s.imagery = 2,
            |s| s.terrain = 1,
            |s| s.trajectory = "orbit:4:2.17:42.52:8000:5001".into(),
        ];
        for (i, change) in changes.iter().enumerate() {
            let mut s = base.clone();
            change(&mut s);
            assert_ne!(reference, key(&s), "bake parameter #{i} did not change the key");
        }
        // Render-only settings are not bake parameters.
        let mut render_only = base.clone();
        render_only.width = 3840;
        render_only.engine = "hydra".into();
        render_only.resident_gb = 24;
        assert_eq!(reference, key(&render_only));
    }

    #[test]
    fn the_range_is_in_the_name_and_in_the_hash() {
        let s = scene(&["--trajectory", "orbit:4:2.17:42.52:8000:5000"]);
        let a = pack_key(&s, "1:4").expect("a");
        let b = pack_key(&s, "1:2").expect("b");
        assert!(a.ends_with("/1-4.tuilepack") && b.ends_with("/1-2.tuilepack"));
        assert_ne!(a.split('/').nth(1), b.split('/').nth(1), "a slice is a different bake");
    }

    #[test]
    fn two_identical_tapes_are_one_bake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (t1, t2, t3) = (dir.path().join("a.mcap"), dir.path().join("b.mcap"), dir.path().join("c.mcap"));
        std::fs::write(&t1, b"poses").expect("w");
        std::fs::write(&t2, b"poses").expect("w");
        std::fs::write(&t3, b"other").expect("w");
        let with = |t: &Path| {
            let mut s = scene(&["--trajectory", "orbit:4"]);
            s.tape = Some(t.to_path_buf());
            pack_key(&s, "1:4").expect("key")
        };
        assert_eq!(with(&t1), with(&t2), "by content, not by path");
        assert_ne!(with(&t1), with(&t3));
    }

    #[test]
    fn a_run_is_named_by_date_then_commit() {
        let clean = serde_json::json!({"short": "af536a2", "dirty": false});
        let dirty = serde_json::json!({"short": "af536a2", "dirty": true});
        let none = serde_json::json!({});
        let id = run_id(&clean);
        assert!(id.len() > 16 && id.as_bytes()[8] == b'T' && id.contains("Z-af536a2-"), "{id}");
        assert!(run_id(&dirty).contains("-af536a2-dirty-"));
        assert!(run_id(&none).contains("-nogit-"));
    }

    #[test]
    fn cadence_and_length_come_from_the_trajectory() {
        assert_eq!(fps_of("pyrenees:2:60:50000:0.40"), 60.0);
        assert_eq!(fps_of("orbit:1440:2.17:42.52:8000:5000"), 24.0);
        assert_eq!(frames_of("pyrenees:2:60:50000:0.40"), Some(7200));
        assert_eq!(frames_of("orbit:1440"), Some(1440));
        assert_eq!(frames_of("zoom:64"), None);
    }

    #[test]
    fn frames_past_the_tape_are_refused_and_a_slice_is_said() {
        assert!(check_frames("orbit:4:2.17:42.52:8000:5000", "1:5").is_err());
        assert!(check_frames("orbit:4:2.17:42.52:8000:5000", "1:2").expect("ok").is_some());
        assert!(check_frames("orbit:4:2.17:42.52:8000:5000", "1:4").expect("ok").is_none());
    }

    #[test]
    fn the_bake_is_told_where_to_put_everything() {
        let s = scene(&["--trajectory", "pyrenees:2:60:50000:0.40"]);
        let env = bake_env(&s, "1:7200", "tok", "packs/k/1-7200.tuilepack", "renders/r1", None);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("JOB_PACK_KEY"), Some("packs/k/1-7200.tuilepack"));
        assert_eq!(get("JOB_RUN_PREFIX"), Some("renders/r1"));
        assert_eq!(get("JOB_FPS"), Some("60"));
        assert_eq!(get("JOB_TAPE_KEY"), Some(""));
        assert_eq!(get("TUILE_RESIDENT_BUDGET_GB"), Some("12"));
    }

    #[test]
    fn the_optix_key_follows_the_image() {
        assert_ne!(optix_key(Some("sha256:aaaa")), optix_key(Some("sha256:bbbb")));
        assert_eq!(optix_key(Some("sha256:5e8edf878e6a708330f5")), "cache/optix/nvidia-l4-sha256-5e8edf878e6a.tar.gz");
    }
}
