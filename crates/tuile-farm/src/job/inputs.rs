// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a render reads: the pack, the stage, the OptiX cache.

use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use tokio::process::Command;

use super::archive::tarball;
use super::config::Config;
use super::log::{human, Log};
use super::Stop;
use crate::RunStore;

fn size(path: &Path) -> String {
    human(std::fs::metadata(path).map_or(0, |m| m.len()))
}

/// The pack, downloaded ONCE for the whole pod: every process then reads the
/// same file the page cache already holds — against sixteen cold globes of
/// ~500 s each. A pack already on disk (`TUILE_PACK`) is used as it is.
pub async fn pack(c: &Config, store: Option<&dyn RunStore>, log: &Log) -> Result<Option<PathBuf>, Stop> {
    if let Some(ready) = &c.pack_ready {
        return Ok(Some(ready.clone()));
    }
    let Some(key) = &c.pack_key else { return Ok(None) };
    let store = store.ok_or_else(|| Stop::fatal("PACK-FETCH-FAILED (no store configured)"))?;
    if let Err(e) = store.get(key, &c.pack_path).await {
        log.line(format!("  {e}"));
        return Err(Stop::fatal("PACK-FETCH-FAILED"));
    }
    log.line(format!("pack: {} -> {}", size(&c.pack_path), c.pack_path.display()));
    Ok(Some(c.pack_path.clone()))
}

/// The stage: inline, as an object, or flown from a tape.
///
/// A supplied tape wins over a generated one, and here it is a correctness
/// condition: a pack answers a camera by pose to a metre and a milliradian,
/// and the generators move — regenerating from the same argument string
/// flew a different flight on 20 September 2026 (0.349 rad off), every task
/// dead in nine seconds. The bake leaves its tape beside the pack for this.
pub async fn stage(c: &Config, store: Option<&dyn RunStore>, log: &Log) -> Result<PathBuf, Stop> {
    let out = c.stage_path.clone();
    if let Some(b64) = &c.stage_b64_gz {
        let gz = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| Stop::fatal(format!("STAGE-DECODE-FAILED base64: {e}")))?;
        let mut usda = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut usda)
            .map_err(|e| Stop::fatal(format!("STAGE-DECODE-FAILED gzip: {e}")))?;
        std::fs::write(&out, usda).map_err(|e| Stop::fatal(format!("STAGE-WRITE-FAILED {e}")))?;
    } else if let Some(key) = &c.stage_key {
        let store = store.ok_or_else(|| Stop::fatal("STAGE-FETCH-FAILED (no store configured)"))?;
        store.get(key, &out).await.map_err(|e| {
            log.line(format!("  {e}"));
            Stop::fatal("STAGE-FETCH-FAILED")
        })?;
    } else if c.tape_key.is_some() || c.trajectory.is_some() {
        let tape = PathBuf::from("/tmp/traj.mcap");
        if let Some(key) = &c.tape_key {
            let store = store.ok_or_else(|| Stop::fatal("TAPE-DOWNLOAD-FAILED (no store configured)"))?;
            store.get(key, &tape).await.map_err(|e| {
                log.line(format!("  {e}"));
                Stop::fatal("TAPE-DOWNLOAD-FAILED")
            })?;
            log.line(format!("bande: fournie ({}), trajectoire ignorée", size(&tape)));
        } else {
            let t = c.trajectory.as_deref().unwrap_or("");
            let (program, args) = generator(t, &c.fps).ok_or_else(|| {
                Stop::fatal(format!("TRAJECTORY-UNKNOWN: {}", t.split(':').next().unwrap_or("")))
            })?;
            run(&c.tools.join(program), &[&[tape.display().to_string()][..], &args].concat(), log)
                .await
                .map_err(|_| Stop::fatal("TAPE-GEN-FAILED"))?;
        }
        let args = [
            tape.display().to_string(),
            out.display().to_string(),
            "--viewport".into(),
            c.viewport.clone(),
            "--sse".into(),
            c.sse.clone(),
            "--fps".into(),
            c.fps.clone(),
        ];
        run(&c.tools.join("tape-to-stage"), &args, log).await.map_err(|_| Stop::fatal("MANIFEST-GEN-FAILED"))?;
    }
    if std::fs::metadata(&out).map_or(true, |m| m.len() == 0) {
        return Err(Stop::fatal(format!("stage absente: {}", out.display())));
    }
    Ok(out)
}

/// The generative road, with the defaults after the kind:
/// `orbit:frames:lon:lat:radius_m:alt_m`, `pyrenees:minutes:fps:alt_m:offset_deg`,
/// `zoom:frames_each_way`.
pub fn generator(trajectory: &str, fps: &str) -> Option<(&'static str, Vec<String>)> {
    let p: Vec<&str> = trajectory.split(':').collect();
    let at = |i: usize, d: &str| p.get(i).filter(|s| !s.is_empty()).map_or(d.to_string(), |s| s.to_string());
    match p.first().copied() {
        Some("orbit") => Some(("orbit-tape", vec![at(1, "1440"), at(2, "2.17"), at(3, "42.52"), at(4, "8000"), at(5, "5000")])),
        Some("pyrenees") => Some(("pyrenees-tape", vec![at(1, "2"), fps.to_string(), at(3, "50000"), at(4, "0.40")])),
        Some("zoom") => Some(("zoom-tape", vec![at(1, "64")])),
        _ => None,
    }
}

async fn run(program: &Path, args: &[String], log: &Log) -> Result<(), ()> {
    match Command::new(program).args(args).output().await {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            String::from_utf8_lossy(&out.stderr).lines().rev().take(5).for_each(|l| log.line(format!("  {l}")));
            Err(())
        }
        Err(e) => {
            log.line(format!("  {}: {e}", program.display()));
            Err(())
        }
    }
}

/// The OptiX cache, before the first render. The driver compiles PTX to
/// machine code on first load — 338 s on an L4, 16 September 2026, against
/// 0.17 s for the frame after — and a container keeps nothing. Optional by
/// construction: a missing key is the first run, an unreadable cache an
/// empty one; neither fails a render.
pub async fn optix_get(c: &Config, store: Option<&dyn RunStore>, log: &Log) {
    let _ = std::fs::create_dir_all(&c.optix_cache_path);
    let (Some(key), Some(store)) = (&c.optix_cache_key, store) else { return };
    let archive = c.outdir().join("optix-cache.tar.gz");
    let unpacked = async {
        store.get(key, &archive).await.ok()?;
        let file = std::fs::File::open(&archive).ok()?;
        tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(&c.optix_cache_path).ok()
    }
    .await;
    match unpacked {
        Some(()) => log.line(format!("OPTIX-CACHE-HIT ({})", human(dir_size(&c.optix_cache_path)))),
        None => log.line("OPTIX-CACHE-MISS — la première frame paiera la compilation"),
    }
}

/// The OptiX cache, back up after the render — by task 0 only: same card,
/// same driver, same kernels, so writing from several would buy nothing and
/// make the result depend on arrival order.
pub async fn optix_put(c: &Config, store: Option<&dyn RunStore>, log: &Log) {
    let (Some(key), Some(store)) = (&c.optix_cache_key, store) else { return };
    if c.task_index != 0 || !c.optix_cache_path.is_dir() {
        return;
    }
    let archive = c.outdir().join("optix-cache-out.tar.gz");
    let entries: Vec<PathBuf> = std::fs::read_dir(&c.optix_cache_path)
        .map_or(Vec::new(), |d| d.flatten().map(|e| PathBuf::from(e.file_name())).collect());
    let ok = tarball(&c.optix_cache_path, &entries, &archive).is_ok() && store.put(&archive, key).await.is_ok();
    if ok {
        log.line(format!("OPTIX-CACHE-UP ({})", size(&archive)));
    } else {
        log.line("OPTIX-CACHE-UP-FAILED");
    }
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |d| {
        d.flatten()
            .map(|e| {
                let p = e.path();
                if p.is_dir() {
                    dir_size(&p)
                } else {
                    e.metadata().map_or(0, |m| m.len())
                }
            })
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectRunStore;
    use std::io::Write;

    fn config(pairs: Vec<(&'static str, String)>) -> Config {
        let get = move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        Config::from_env(&get).expect("test")
    }

    #[test]
    fn the_generators_keep_their_defaults() {
        assert_eq!(generator("orbit", "24").expect("test"), ("orbit-tape", vec!["1440".into(), "2.17".into(), "42.52".into(), "8000".into(), "5000".into()]));
        assert_eq!(generator("pyrenees:3::60000", "30").expect("test").1, ["3", "30", "60000", "0.40"]);
        assert_eq!(generator("zoom:10", "24").expect("test"), ("zoom-tape", vec!["10".into()]));
        assert!(generator("spiral", "24").is_none());
    }

    #[tokio::test]
    async fn an_inline_stage_is_unpacked_and_an_empty_one_refused() {
        let dir = tempfile::tempdir().expect("test");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"#usda 1.0\n").expect("test");
        let b64 = base64::engine::general_purpose::STANDARD.encode(gz.finish().expect("test"));
        let path = dir.path().join("stage.usda");
        let c = config(vec![("JOB_FRAMES", "1:2".into()), ("JOB_STAGE_B64_GZ", b64), ("JOB_STAGE", path.display().to_string())]);
        let got = stage(&c, None, &Log::stdout_only()).await.expect("test");
        assert_eq!(std::fs::read_to_string(got).expect("test"), "#usda 1.0\n");
        let missing = config(vec![("JOB_FRAMES", "1:2".into()), ("JOB_STAGE", dir.path().join("none.usda").display().to_string())]);
        assert!(stage(&missing, None, &Log::stdout_only()).await.is_err());
    }

    #[tokio::test]
    async fn the_pack_and_the_stage_come_from_the_store_by_key() {
        let bucket = tempfile::tempdir().expect("test");
        let work = tempfile::tempdir().expect("test");
        std::fs::create_dir_all(bucket.path().join("runs/r1")).expect("test");
        std::fs::write(bucket.path().join("runs/r1/scene.tuilepack"), b"PACK").expect("test");
        std::fs::write(bucket.path().join("runs/r1/manifest.usda"), b"#usda 1.0\n").expect("test");
        let store = ObjectRunStore::local(bucket.path(), crate::Tuning::default()).expect("test");
        let c = config(vec![
            ("JOB_FRAMES", "1:2".into()),
            ("JOB_PACK_KEY", "runs/r1/scene.tuilepack".into()),
            ("JOB_PACK", work.path().join("p").display().to_string()),
            ("JOB_STAGE_KEY", "runs/r1/manifest.usda".into()),
            ("JOB_STAGE", work.path().join("s.usda").display().to_string()),
        ]);
        let log = Log::stdout_only();
        let p = pack(&c, Some(&store), &log).await.expect("test").expect("test");
        assert_eq!(std::fs::read(p).expect("test"), b"PACK");
        assert!(stage(&c, Some(&store), &log).await.is_ok());
        let bad = config(vec![("JOB_FRAMES", "1:2".into()), ("JOB_PACK_KEY", "runs/r1/none".into()), ("JOB_PACK", work.path().join("q").display().to_string())]);
        assert!(pack(&bad, Some(&store), &log).await.is_err(), "PACK-FETCH-FAILED");
    }

    #[tokio::test]
    async fn the_optix_cache_goes_up_from_task_zero_and_comes_back_down() {
        let bucket = tempfile::tempdir().expect("test");
        let out = tempfile::tempdir().expect("test");
        let store = ObjectRunStore::local(bucket.path(), crate::Tuning::default()).expect("test");
        let cache = out.path().join("optix-cache");
        std::fs::create_dir_all(cache.join("sub")).expect("test");
        std::fs::write(cache.join("sub/kernel.bin"), b"K").expect("test");
        let c = config(vec![
            ("JOB_FRAMES", "1:2".into()),
            ("JOB_OUT", out.path().join("render.mp4").display().to_string()),
            ("JOB_OPTIX_CACHE_KEY", "optix-cache.tar.gz".into()),
        ]);
        let log = Log::stdout_only();
        optix_put(&c, Some(&store), &log).await;
        assert!(bucket.path().join("optix-cache.tar.gz").exists());
        std::fs::remove_dir_all(&cache).expect("test");
        optix_get(&c, Some(&store), &log).await;
        assert_eq!(std::fs::read(cache.join("sub/kernel.bin")).expect("test"), b"K");
    }
}
