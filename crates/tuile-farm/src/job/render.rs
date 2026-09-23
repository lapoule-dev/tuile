// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render processes: one Blender per share of the frames, each shown one
//! GPU, each writing its own segment, logs, trace and crash report.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::config::Config;
use super::gpu::on_path;
use super::log::Log;
use super::plan::Plan;

/// Blender's progress chatter, kept out of the job's log.
pub fn is_chatter(line: &str) -> bool {
    ["Fra:", "Saved:", "Time:", "Append frame"].iter().any(|p| line.starts_with(p))
}

/// A stderr line worth raising to the job's log while the task runs: the
/// full trace only arrives at the end, in an archive, and on 16 September a
/// count made on the live logs concluded "zero errors" over a trace that
/// held a hundred and ninety.
pub fn is_error(line: &str) -> bool {
    ["ERROR", "Error", "error:", "Could not", "FATAL", "Warning:"].iter().any(|p| line.contains(p))
}

/// Variables no render process needs, and which it could leak into a trace:
/// the job's own credentials.
pub const WITHHELD: &[&str] =
    &["TUILE_STORE_ACCESS_KEY_ID", "TUILE_STORE_SECRET_ACCESS_KEY", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY"];

/// The command line of process `i` (after the program itself).
pub fn blender_args(c: &Config, stage: &Path, i: usize, (a, b): (u64, u64)) -> Vec<String> {
    let outdir = c.outdir();
    let mut v: Vec<String> = vec!["-b".into()];
    v.extend(c.blender_args.iter().cloned());
    v.extend(["-P".into(), c.render_script.clone(), "--".into()]);
    let pairs = [
        ("--stage", stage.display().to_string()),
        ("--engine", c.engine.clone()),
        ("--tier", c.tier.clone()),
        ("--delegate", c.delegate.clone()),
        ("--frames", format!("{a}:{b}")),
        ("--width", c.width.clone()),
        ("--fps", c.fps.clone()),
        ("--samples", c.samples.clone()),
        ("--adaptive-threshold", c.threshold.clone()),
        ("--batch-frames", c.batch_frames.clone()),
    ];
    for (k, val) in pairs {
        v.push(k.into());
        v.push(val);
    }
    v.extend(c.extra_args.iter().cloned());
    v.extend([
        "--out".into(),
        outdir.join(format!("s{i}")).display().to_string(),
        "--video".into(),
        outdir.join(format!("seg{i}.mp4")).display().to_string(),
    ]);
    v
}

/// One tile cache per process, seeded from the shared one: the store is
/// single-writer (two writers corrupt it quietly), so sharing is copy on
/// start — a warm seed starts each process near a full hit rate.
fn seed_cache(seed: &Path, own: &Path) {
    if !seed.is_dir() || own.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(seed) else { return };
    let foyers: Vec<PathBuf> =
        entries.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("foyer-"))).collect();
    if foyers.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(own);
    for f in foyers {
        let _ = copy_tree(&f, &own.join(f.file_name().unwrap_or_default()));
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Renders every share of the plan and waits for the renders — and only for
/// them. Returns each process's exit code (`None`: killed by a signal).
///
/// `CYCLES_DEVICE` is not decoration: the Cycles Hydra delegate falls back to
/// the CPU when nothing names a device, and four GPUs would sit idle through
/// the night. With one `CUDA_VISIBLE_DEVICES` per process it also places the
/// work. Each process gets its own `TMPDIR`, where Blender writes its crash
/// report: on 23 September two of four crashed and all that came back was
/// "Writing: /tmp/blender.crash.txt".
pub async fn render_all(
    c: &Config,
    plan: &Plan,
    stage: &Path,
    backend: &str,
    child_env: &[(String, String)],
    unset: &[&str],
    log: &Log,
) -> Vec<Option<i32>> {
    let outdir = c.outdir();
    // Blender's stdout is block-buffered into a pipe: `stdbuf -oL` makes it
    // speak a line at a time, as it would to a terminal.
    let stdbuf = on_path("stdbuf");
    let mut running = Vec::new();
    for (i, (gpu, range)) in plan.processes.iter().enumerate() {
        let own_cache = c.cache_seed.join(format!("r{i}"));
        seed_cache(&c.cache_seed, &own_cache);
        let tmp = outdir.join(format!("tmp-s{i}"));
        let _ = std::fs::create_dir_all(&tmp);
        let mut cmd = if stdbuf {
            let mut cmd = Command::new("stdbuf");
            cmd.arg("-oL").arg(&c.blender);
            cmd
        } else {
            Command::new(&c.blender)
        };
        cmd.args(blender_args(c, stage, i, *range))
            .envs(child_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("CUDA_VISIBLE_DEVICES", gpu.to_string())
            .env("TUILE_CACHE_DIR", &own_cache)
            .env("TMPDIR", &tmp)
            .env("CYCLES_DEVICE", backend)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A task that is cancelled takes its renders with it.
            .kill_on_drop(true);
        for k in WITHHELD.iter().chain(unset) {
            cmd.env_remove(k);
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                log.line(format!("RENDER-SPAWN-FAILED j{i}: {e}"));
                running.push(None);
                continue;
            }
        };
        let tag = format!("[gpu{gpu}-j{i}] ");
        let err_tag = format!("[err{i}] ");
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (out_log, err_log) = (log.clone(), log.clone());
        let per_proc = outdir.join(format!("log-s{i}.txt"));
        let trace = outdir.join(format!("trace-s{i}.jsonl"));
        let out_task = tokio::spawn(async move {
            let Some(stdout) = stdout else { return };
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&per_proc).ok();
            let mut lines = BufReader::new(stdout);
            let mut buf = Vec::new();
            while matches!(lines.read_until(b'\n', &mut buf).await, Ok(n) if n > 0) {
                let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                buf.clear();
                if is_chatter(&line) {
                    continue;
                }
                let tagged = format!("{tag}{line}");
                out_log.line(&tagged);
                if let Some(f) = file.as_mut() {
                    let _ = writeln!(f, "{tagged}");
                }
            }
        });
        let err_task = tokio::spawn(async move {
            let Some(stderr) = stderr else { return };
            let mut file = std::fs::File::create(&trace).ok();
            let mut lines = BufReader::new(stderr);
            let mut buf = Vec::new();
            while matches!(lines.read_until(b'\n', &mut buf).await, Ok(n) if n > 0) {
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(&buf);
                }
                let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                buf.clear();
                if is_error(&line) {
                    err_log.line(format!("{err_tag}{line}"));
                }
            }
        });
        running.push(Some((i, child, out_task, err_task)));
    }
    let mut codes = Vec::with_capacity(running.len());
    for r in running {
        let Some((i, mut child, out_task, err_task)) = r else {
            codes.push(Some(127));
            continue;
        };
        let status = child.wait().await;
        let _ = out_task.await;
        let _ = err_task.await;
        let code = status.ok().and_then(|s| s.code());
        if code != Some(0) {
            log.line(format!("RENDER-EXIT j{i} {}", code.map_or("signal".to_string(), |c| c.to_string())));
        }
        codes.push(code);
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatter_is_dropped_and_errors_are_raised() {
        assert!(is_chatter("Fra:12 Mem:1.2G"));
        assert!(is_chatter("Append frame 3990"));
        assert!(!is_chatter("frame 3990: début"));
        assert!(is_error("ERROR (cycles): device lost"));
        assert!(is_error("Warning: in _GetResolved..."));
        assert!(!is_error("WAIT[1] converged"));
    }

    #[test]
    fn the_command_line_is_the_scripts() {
        let get = |k: &str| match k {
            "JOB_FRAMES" => Some("1:8".into()),
            "JOB_ENGINE" => Some("hydra".into()),
            "JOB_EXTRA_ARGS" => Some("--no-dof".into()),
            "JOB_BLENDER_ARGS" => Some("--debug-cycles".into()),
            "JOB_OUT" => Some("/out/render.mp4".into()),
            _ => None,
        };
        let c = Config::from_env(&get).expect("test");
        let args = blender_args(&c, Path::new("/tmp/job-stage.usda"), 2, (5, 6)).join(" ");
        assert_eq!(
            args,
            "-b --debug-cycles -P /opt/render/render_usd.py -- --stage /tmp/job-stage.usda --engine hydra \
             --tier cycles --delegate cycles --frames 5:6 --width 1920 --fps 24 --samples 128 \
             --adaptive-threshold 0.05 --batch-frames 60 --no-dof --out /out/s2 --video /out/seg2.mp4"
        );
    }

    #[test]
    fn a_process_cache_is_seeded_from_the_shared_one() {
        let seed = tempfile::tempdir().expect("test");
        std::fs::create_dir_all(seed.path().join("foyer-0/blocks")).expect("test");
        std::fs::write(seed.path().join("foyer-0/blocks/a"), b"A").expect("test");
        std::fs::write(seed.path().join("other"), b"x").expect("test");
        let own = seed.path().join("r0");
        seed_cache(seed.path(), &own);
        assert_eq!(std::fs::read(own.join("foyer-0/blocks/a")).expect("test"), b"A");
        assert!(!own.join("other").exists());
    }
}
