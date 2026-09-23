// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render job: one frame range of one OpenUSD stage, split across every
//! GPU of the host, into segments, into a film. `tuile-farm render` is the
//! image's entry point; everything is a parameter in the environment
//! ([`config::Config`]) — no constant here deserves an image rebuild.
//!
//! ```text
//! JOB_*  →  identity and slice  →  GPUs woken, counted, probed (exact or die)
//!        →  pack, stage, OptiX cache from the store
//!        →  N Blender processes, one GPU each, one segment each
//!        →  segments up as they exist, receipt, the film by whoever is last
//! ```
//!
//! Whatever happens, the archive is shipped: on success, on every failure, on
//! SIGTERM (a cancelled or timed-out Cloud Run task), and every
//! `JOB_ARCHIVE_EVERY` seconds while it runs, because a reclaimed machine runs
//! nothing at the end. The lines the launchers and the logs are read for —
//! `NO-GPU-BAIL`, `SEG-UP`, `TASK-DONE`, `FILM-UP`, `RENDER-DONE`,
//! `VIDEO-MISSING`… — are the shell script's, word for word.

pub mod archive;
pub mod config;
pub mod gpu;
pub mod inputs;
pub mod log;
pub mod plan;
pub mod render;

use std::path::Path;
use std::time::{Duration, Instant};

use archive::{Shipper, LOGS};
use config::Config;
use log::{human, Log};
use plan::Plan;

use crate::assemble::{self, Outcome, Receipt, RunKeys};
use crate::{ObjectRunStore, RunStore};

/// Why the job stopped short.
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    /// The line that says so (`VIDEO-MISSING`, `NO-GPU-BAIL`…).
    pub marker: String,
    /// Wait `JOB_BAIL_SLEEP` before exiting: a bail keeps the machine a while
    /// for someone to look at it.
    pub bail: bool,
}

impl Stop {
    pub fn fatal(marker: impl Into<String>) -> Stop {
        Stop { marker: marker.into(), bail: false }
    }
    pub fn bail(marker: impl Into<String>) -> Stop {
        Stop { marker: marker.into(), bail: true }
    }
}

/// Which render of the run this is: `TUILE_RENDER_ID`, else Cloud Run's
/// execution, else `local`.
pub fn render_id(get: &dyn Fn(&str) -> Option<String>) -> String {
    ["TUILE_RENDER_ID", "CLOUD_RUN_EXECUTION"]
        .iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "local".into())
}

fn needs_store(c: &Config) -> bool {
    c.run_prefix.is_some()
        || (c.pack_key.is_some() && c.pack_ready.is_none())
        || c.stage_key.is_some()
        || c.tape_key.is_some()
        || c.optix_cache_key.is_some()
}

/// Runs the job; the process exit code.
pub async fn main(get: &dyn Fn(&str) -> Option<String>) -> u8 {
    let c = match Config::from_env(get) {
        Ok(c) => c,
        Err(e) => {
            println!("JOB-CONFIG-INVALID {e}");
            return 2;
        }
    };
    let outdir = c.outdir();
    if let Err(e) = std::fs::create_dir_all(&outdir) {
        println!("JOB-OUTDIR-FAILED {}: {e}", outdir.display());
        return 1;
    }
    let log = Log::open(&outdir.join("job.log"));
    let store: Option<ObjectRunStore> = match ObjectRunStore::from_env() {
        Ok(s) => Some(s),
        Err(e) if needs_store(&c) => {
            log.line(format!("STORE-UNAVAILABLE {e}"));
            return 1;
        }
        Err(_) => None,
    };
    let store_ref: Option<&dyn RunStore> = store.as_ref().map(|s| s as &dyn RunStore);
    let shipper = match (store_ref, c.run_prefix.as_deref()) {
        (Some(store), Some(prefix)) => Some(Shipper { store, prefix, tag: c.task_tag(), dir: outdir.clone() }),
        _ => None,
    };
    let render = render_id(get);

    let body = async {
        match work(&c, store_ref, &render, &log, shipper.as_ref()).await {
            Ok(()) => 0u8,
            Err(stop) => {
                log.line(&stop.marker);
                if stop.bail {
                    tokio::time::sleep(Duration::from_secs(c.bail_sleep_s)).await;
                }
                1
            }
        }
    };
    let code = tokio::select! {
        code = body => code,
        () = terminated() => {
            log.line("SIGTERM — the renders are stopped, the logs go up before the container does");
            143
        }
    };
    if let Some(s) = &shipper {
        s.ship_all(&log).await;
    }
    // A courtesy window for an ssh session, opened only when one was asked
    // for: Cloud Run bills the GPU by the second.
    if code == 0 && c.ssh_pubkey.is_some() {
        tokio::time::sleep(Duration::from_secs(c.done_sleep_s)).await;
    }
    code
}

#[cfg(unix)]
async fn terminated() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn terminated() {
    std::future::pending().await
}

/// Everything between the configuration and the archive.
async fn work(
    c: &Config,
    store: Option<&dyn RunStore>,
    render: &str,
    log: &Log,
    shipper: Option<&Shipper<'_>>,
) -> Result<(), Stop> {
    let batch: Vec<String> = c.batch_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    log.line(format!("batch config: {}", batch.join(" ")));
    let mut child_env = c.batch_env.clone();
    if let Some(tf) = &c.tf_debug {
        log.line(format!("TF_DEBUG={tf}"));
        child_env.push(("TF_DEBUG".into(), tf.clone()));
    }

    // The logs, flushed while the job runs: a pod taken away runs no ending.
    let flusher = shipper.map(|s| {
        let every = Duration::from_secs(c.archive_every_s.max(1));
        let log = log.clone();
        async move {
            loop {
                tokio::time::sleep(every).await;
                let _ = s.ship("logs.tar.gz", LOGS, &log, true).await;
            }
        }
    });

    let plan = Plan::of(c);
    if c.chunk_given {
        log.line(format!("task {}/{}: frames {}:{} (chunk given)", c.task_index, c.task_count, plan.first, plan.last));
    } else if c.task_count > 1 {
        log.line(format!("task {}/{}: frames {}:{}", c.task_index, c.task_count, plan.first, plan.last));
    }
    let nominal = c.gpus * c.procs_per_gpu;
    if plan.jobs() < nominal {
        log.line(format!(
            "frames {}:{} — {} frame(s) pour {nominal} processus, donc {} processus",
            plan.first,
            plan.last,
            plan.total(),
            plan.jobs()
        ));
    }

    let steps = async {
        prepare_and_render(c, store, &plan, &mut child_env, log).await?;
        deliver(c, store, render, &plan, log).await
    };
    match flusher {
        Some(f) => {
            tokio::select! {
                r = steps => r,
                () = f => unreachable!("the flusher loops"),
            }
        }
        None => steps.await,
    }
}

async fn prepare_and_render(
    c: &Config,
    store: Option<&dyn RunStore>,
    plan: &Plan,
    child_env: &mut Vec<(String, String)>,
    log: &Log,
) -> Result<(), Stop> {
    // sshd first: it exists to diagnose a job that went wrong, and the probe
    // below is what goes wrong.
    if let Some(key) = &c.ssh_pubkey {
        start_sshd(key, log).await;
    }

    gpu::wake(log).await;
    if gpu::on_path("nvidia-smi") {
        let (in_proc, in_dev) = gpu::gpu_counts(Path::new("/proc/driver/nvidia/gpus"), Path::new("/dev"));
        log.line(format!("gpus: {in_proc} advertised in procfs, {in_dev} device nodes"));
        if in_proc != in_dev {
            for l in [
                format!("GPU-PARTIAL-HOST: the driver advertises {in_proc} GPUs and this"),
                format!("  container has {in_dev} device nodes. UVM initialises across every"),
                "  GPU the driver knows about, cannot reach the ones this pod was".into(),
                "  not given, and refuses to open — cuInit then returns 999.".into(),
                "  Nothing in the image can help, and retrying is not the answer:".into(),
                "  four different hosts have done this (8/1, 5/4, 5/1, 5/1), so it".into(),
                "  is how a partial machine is handed out, not a broken machine.".into(),
            ] {
                log.line(l);
            }
            return Err(Stop::bail("  Ask for --gpu-count equal to the host's full complement."));
        }
    }

    let (found, backend) = gpu::probe(&c.blender, &c.engine, &c.delegate, &c.cycles_backends, log).await;
    log.line(format!("probe: NGPU {found} backend={backend} (attendu: NGPU {})", c.gpus));
    if found != c.gpus {
        gpu::bail_diagnostics(&c.blender, log).await;
        return Err(Stop::bail("NO-GPU-BAIL"));
    }

    let mut unset: Vec<&str> = Vec::new();
    if let Some(pack) = inputs::pack(c, store, log).await? {
        child_env.push(("TUILE_PACK".into(), pack.display().to_string()));
        if let Some(scene) = &c.scene {
            child_env.push(("TUILE_SCENE".into(), scene.clone()));
        }
        // A pack carries its own imagery; a token beside it is a token that
        // can still be reached. Withheld, so that is a fact.
        unset.push("TUILE_ION_TOKEN");
    }
    if !c.has_source() {
        return Err(Stop::bail("NO-SOURCE-BAIL"));
    }
    let stage = inputs::stage(c, store, log).await?;
    inputs::optix_get(c, store, log).await;
    child_env.push(("OPTIX_CACHE_PATH".into(), c.optix_cache_path.display().to_string()));

    let t0 = Instant::now();
    let sampler = gpu::Sampler::spawn(c.gpu_sample_s, c.gpus, log.clone());
    render::render_all(c, plan, &stage, &backend, child_env, &unset, log).await;
    if let Some(s) = &sampler {
        s.stop();
    }
    log.line(format!(
        "WALL: {}s pour {} frames en {} processus / {} GPU",
        t0.elapsed().as_secs(),
        plan.total(),
        plan.jobs(),
        c.gpus
    ));
    inputs::optix_put(c, store, log).await;
    // A run that used one GPU of four is not slow, it is broken, and must not
    // need a human to notice.
    match sampler.filter(|s| s.seen.load(std::sync::atomic::Ordering::Relaxed) > 0) {
        Some(s) => {
            let peak = s.peak.load(std::sync::atomic::Ordering::Relaxed);
            if peak < c.gpus {
                log.line(format!("GPU-UNDERUSED: au mieux {peak} GPU sur {} ont travaillé", c.gpus));
            } else {
                log.line(format!("GPU-OK: {peak}/{}", c.gpus));
            }
        }
        None => log.line(format!("GPU-OK: ?/{}", c.gpus)),
    }
    Ok(())
}

/// Segments up, counted, and the film — or the reason there is none.
async fn deliver(c: &Config, store: Option<&dyn RunStore>, render: &str, plan: &Plan, log: &Log) -> Result<(), Stop> {
    let outdir = c.outdir();
    let seg = |i: usize| outdir.join(format!("seg{i}.mp4"));
    let size = |i: usize| std::fs::metadata(seg(i)).map_or(0, |m| m.len());
    let keys = c.run_prefix.as_deref().map(|p| RunKeys::new(p, render));
    // Each segment leaves the moment it exists: a reclaimed pod once died with
    // 1440 frames rendered and nothing deposited.
    for (i, g) in plan.segments().into_iter().enumerate() {
        if size(i) == 0 && has_png(&outdir, i) {
            log.line(format!("NO-ENCODER j{i}: frames rendered as PNG and no video — this Blender has no built-in encoder"));
        }
        if let (Some(keys), Some(store)) = (&keys, store) {
            if size(i) > 0 {
                match store.put(&seg(i), &keys.segment(g)).await {
                    Ok(_) => log.line(format!("SEG-UP {g} ({})", human(size(i)))),
                    Err(_) => log.line(format!("SEG-UP-FAILED {g}")),
                }
            }
        }
    }
    // Counted by size, not existence: a process that dies leaves a 48-byte
    // container, and a concat once stopped at the first empty file and
    // reported a film four fifths short.
    let missing: Vec<usize> = (0..plan.jobs() as usize).filter(|&i| size(i) <= 1000).collect();
    let n = plan.jobs() as usize - missing.len();
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|i| i.to_string()).collect();
        log.line(format!("SEGMENTS-MISSING: {}", list.join(" ")));
        log.line(format!("TASK-INCOMPLETE {}/{} ({n}/{} segments)", c.task_index, c.task_count, plan.jobs()));
        return Err(Stop::fatal("VIDEO-MISSING"));
    }
    let fps: Option<f64> = c.fps.parse().ok().filter(|f: &f64| *f > 0.0);
    if let (Some(keys), Some(store)) = (&keys, store) {
        let receipt = Receipt { task: c.task_index, first: plan.first, last: plan.last, segments: plan.segments() };
        put_receipt(store, keys, &receipt, &outdir).await.map_err(|e| {
            log.line(format!("  {e}"));
            Stop::fatal("RECEIPT-FAILED")
        })?;
        log.line(format!("RECEIPT {} frames {}:{} segments {:?}", receipt.task, receipt.first, receipt.last, receipt.segments));
        match assemble::assemble(store, keys, c.task_count, &outdir.join("assemble"), fps).await {
            Ok(Outcome::Waiting { done, of }) => {
                log.line(format!("FILM-WAITING {done}/{of} receipts"));
                log.line(format!(
                    "TASK-DONE {}/{} ({n} segments, frames {}:{})",
                    c.task_index, c.task_count, plan.first, plan.last
                ));
                Ok(())
            }
            Ok(Outcome::Assembled { key, frames, bytes }) => {
                log.line(format!("FILM-UP {key} {frames} frames {bytes} bytes"));
                log.line("RENDER-DONE");
                Ok(())
            }
            Err(e) => Err(Stop::fatal(format!("FILM-FAILED {e}"))),
        }
    } else if c.task_count > 1 {
        Err(Stop::fatal(format!(
            "TASK-DONE {}/{} — no JOB_RUN_PREFIX, so no film: the segments never left",
            c.task_index, c.task_count
        )))
    } else {
        // Joined in Rust — samples copied, nothing decoded — and counted from
        // the film written: a concat that drops a segment makes a shorter
        // film, not an error.
        let inputs: Vec<_> = (0..plan.jobs() as usize).map(seg).collect();
        let frames = crate::concat::concat(&inputs, &c.out)
            .and_then(|_| crate::concat::count_frames(&c.out))
            .map_err(|e| Stop::fatal(format!("FILM-FAILED {e}")))?;
        log.line(format!("FILM {} {frames} frames", c.out.display()));
        if frames != plan.total() {
            return Err(Stop::fatal(format!("FRAME-COUNT-MISMATCH: {frames} frames dans la vidéo, {} demandées", plan.total())));
        }
        log.line(format!("frames: {frames}/{}", plan.total()));
        log.line("RENDER-DONE");
        Ok(())
    }
}

fn has_png(outdir: &Path, i: usize) -> bool {
    let prefix = format!("s{i}.");
    std::fs::read_dir(outdir).is_ok_and(|d| {
        d.flatten().any(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with(&prefix) && n.ends_with(".png")
        })
    })
}

async fn put_receipt(store: &dyn RunStore, keys: &RunKeys, receipt: &Receipt, outdir: &Path) -> Result<(), String> {
    let file = outdir.join(format!("receipt-{}.json", receipt.task));
    std::fs::write(&file, serde_json::to_vec(receipt).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    store.put(&file, &keys.receipt(receipt.task)).await.map(|_| ()).map_err(|e| e.to_string())
}

async fn start_sshd(key: &str, log: &Log) {
    let script = "apt-get update -qq && apt-get install -y -qq openssh-server > /dev/null \
                  && mkdir -p /root/.ssh /run/sshd && cat > /root/.ssh/authorized_keys \
                  && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys && /usr/sbin/sshd -p 22";
    let child = tokio::process::Command::new("sh")
        .args(["-c", script])
        .stdin(std::process::Stdio::piped())
        .spawn();
    let ok = match child {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(format!("{key}\n").as_bytes()).await;
            }
            child.wait().await.is_ok_and(|s| s.success())
        }
        Err(_) => false,
    };
    log.line(if ok { "sshd up" } else { "SSHD-FAILED" });
}
