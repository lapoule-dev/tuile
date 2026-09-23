// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The GPUs: woken, counted, probed, and watched while the render runs.
//!
//! Exact or die: the probe must find `JOB_GPUS` devices of a Cycles backend,
//! or the job bails without rendering a single CPU frame.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;

use super::log::Log;

/// Is `bin` on the PATH?
pub fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Runs a command and returns its stdout and stderr together, as `2>&1` did;
/// `None` if it could not start.
pub async fn capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().await.ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

/// Wakes the driver before asking anything of it: `nvidia_uvm` is not
/// initialised in a container until some NVIDIA application touches it, and
/// until then `cuInit` returns CUDA_ERROR_UNKNOWN. Which GPUs the machine had
/// is also the first question of any post-mortem.
pub async fn wake(log: &Log) {
    log.line("--- driver wake-up ---");
    match capture("nvidia-smi", &["-L"]).await {
        Some(s) => s.lines().take(8).for_each(|l| log.line(l)),
        None => log.line("  nvidia-smi absent (CPU host?)"),
    }
}

/// GPUs the driver advertises against device nodes this container holds.
/// Every broken host so far advertised more than it handed out: UVM refuses
/// to open across GPUs it cannot reach and `cuInit` returns 999. Four hosts
/// did it (8/1, 5/4, 5/1, 5/1): how a share of a machine looks, not a broken
/// one. Counted, not computed — no CUDA, no interpreter.
pub fn gpu_counts(proc_dir: &Path, dev_dir: &Path) -> (usize, usize) {
    let in_proc = std::fs::read_dir(proc_dir).map_or(0, |d| d.count());
    let in_dev = std::fs::read_dir(dev_dir).map_or(0, |d| {
        d.flatten()
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.strip_prefix("nvidia").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
            })
            .count()
    });
    (in_proc, in_dev)
}

/// The Python tuple of backends, in order: `('OPTIX','CUDA',)`.
pub fn backends_tuple(backends: &[String]) -> String {
    let inner: Vec<String> = backends.iter().map(|b| format!("'{b}'")).collect();
    format!("({},)", inner.join(","))
}

/// Asks Blender which backend it has and how many devices on it — preferring
/// the first of `backends`, and saying which. `compute_device_type` answers a
/// backend this build lacks with a TypeError naming what it offers: the only
/// reliable source (enum_items answers []).
pub fn probe_script(backends: &[String]) -> String {
    format!(
        "
import bpy
prefs = bpy.context.preferences.addons['cycles'].preferences
why = []
for backend in {tuple}:
    try:
        prefs.compute_device_type = backend
    except TypeError as e:
        why.append(str(e).split('not found in')[-1].strip())
        continue
    prefs.get_devices()
    n = sum(1 for d in prefs.devices if d.type == backend)
    why.append('%s:%d' % (backend, n))
    if n:
        print('NGPU', n, backend)
        break
else:
    print('NGPU 0 none')
print('BACKENDS', ' '.join(why))
",
        tuple = backends_tuple(backends)
    )
}

/// `(BACKENDS line, devices, backend)` from the probe's output.
pub fn parse_probe(out: &str) -> (Option<String>, u32, String) {
    let backends = out.lines().find(|l| l.starts_with("BACKENDS ")).map(str::to_string);
    let found = out.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        match (w.next(), w.next(), w.next()) {
            (Some("NGPU"), Some(n), Some(b)) => Some((n.parse().ok()?, b.to_string())),
            _ => None,
        }
    });
    let (n, b) = found.unwrap_or((0, "none".to_string()));
    (backends, n, b)
}

/// The GPU count and the backend to hand Cycles. Storm draws through GL and
/// never calls CUDA: for it the cards are counted.
pub async fn probe(blender: &str, engine: &str, delegate: &str, backends: &[String], log: &Log) -> (u32, String) {
    if engine == "hydra" && delegate == "storm" {
        let n = capture("nvidia-smi", &["-L"]).await.map_or(0, |s| s.lines().filter(|l| l.starts_with("GPU")).count());
        return (n as u32, "none".into());
    }
    let out = capture(blender, &["-b", "--python-expr", &probe_script(backends)]).await.unwrap_or_default();
    let (line, n, backend) = parse_probe(&out);
    if let Some(l) = line {
        log.line(l);
    }
    (n, backend)
}

/// Everything a person would ask next, gathered before the pod is gone: the
/// build side and the driver side fail identically from outside.
pub async fn bail_diagnostics(blender: &str, log: &Log) {
    log.line("--- what the driver sees ---");
    for args in [&["-L"][..], &["--query-gpu=name,driver_version", "--format=csv,noheader"][..]] {
        match capture("nvidia-smi", args).await {
            Some(s) => s.lines().take(8).for_each(|l| log.line(l)),
            None => log.line("nvidia-smi absent"),
        }
    }
    log.line("--- driver libraries in the container ---");
    let libs = capture("ldconfig", &["-p"]).await.unwrap_or_default();
    let found: Vec<&str> = libs
        .lines()
        .filter(|l| ["libcuda.so", "libnvoptix", "libnvidia-ml"].iter().any(|n| l.contains(n)))
        .take(6)
        .collect();
    if found.is_empty() {
        log.line("  none");
    }
    found.iter().for_each(|l| log.line(format!("  {}", l.trim())));
    for v in ["NVIDIA_VISIBLE_DEVICES", "NVIDIA_DRIVER_CAPABILITIES", "CUDA_VISIBLE_DEVICES"] {
        log.line(format!("  {v}={}", std::env::var(v).unwrap_or_else(|_| "<unset>".into())));
    }
    log.line("--- device nodes ---");
    let nodes: Vec<String> = std::fs::read_dir("/dev").map_or(Vec::new(), |d| {
        d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("nvidia")).collect()
    });
    log.line(format!("  {}", nodes.join(" ")));
    log.line("--- cuInit, with its number ---");
    let cuinit = "
import ctypes, os
try:
    lib = ctypes.CDLL('libcuda.so.1')
except OSError as e:
    print('libcuda.so.1 will not load:', e)
else:
    for label, env in (('as the job runs', None), ('with one device', '0')):
        if env is not None:
            os.environ['CUDA_VISIBLE_DEVICES'] = env
        rc = lib.cuInit(0)
        n = ctypes.c_int(-1)
        rc2 = lib.cuDeviceGetCount(ctypes.byref(n))
        print(f'{label}: cuInit={rc} cuDeviceGetCount={rc2} devices={n.value}')
";
    let out = capture(blender, &["-b", "--python-expr", cuinit]).await.unwrap_or_default();
    out.lines().filter(|l| l.contains("cuInit=") || l.contains("will not load")).for_each(|l| log.line(format!("  {l}")));
    log.line("--- what Cycles says when asked to explain itself ---");
    let devices = "
import bpy
p = bpy.context.preferences.addons['cycles'].preferences
for b in ('OPTIX', 'CUDA'):
    try:
        p.compute_device_type = b
    except TypeError:
        continue
    p.get_devices()
    print('DEVICES', b, [(d.type, d.name) for d in p.devices])
";
    let out = capture(blender, &["-b", "--debug-cycles", "--python-expr", devices]).await.unwrap_or_default();
    out.lines()
        .filter(|l| {
            let l = l.to_lowercase();
            ["devices", "cuda", "optix", "device"].iter().any(|k| l.contains(k))
        })
        .take(20)
        .for_each(|l| log.line(l));
}

/// One `GPU-USE` line from `nvidia-smi --query-gpu=index,utilization.gpu,
/// memory.used --format=csv,noheader,nounits`: how many are busy (> 5 %), and
/// each card's use.
pub fn gpu_use_line(csv: &str, expected: u32) -> Option<(u32, String)> {
    let mut busy = 0;
    let mut cards = String::new();
    for row in csv.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = row.split(',').map(str::trim).collect();
        let [index, util, mem] = f[..] else { continue };
        if util.parse::<u32>().is_ok_and(|u| u > 5) {
            busy += 1;
        }
        cards.push_str(&format!("gpu{index}={util}%/{mem}MiB "));
    }
    (!cards.is_empty()).then(|| (busy, format!("GPU-USE {busy}/{expected} actifs  {cards}")))
}

/// Samples GPU use while the render runs, so the log says whether every card
/// worked whether or not anyone watched — four processes were once measured
/// all on GPU 2, and nothing in the job said so.
pub struct Sampler {
    pub peak: Arc<AtomicU32>,
    pub seen: Arc<AtomicU32>,
    task: tokio::task::JoinHandle<()>,
}

impl Sampler {
    pub fn spawn(every_s: u64, expected: u32, log: Log) -> Option<Sampler> {
        if !on_path("nvidia-smi") {
            return None;
        }
        let peak = Arc::new(AtomicU32::new(0));
        let seen = Arc::new(AtomicU32::new(0));
        let (p, s) = (peak.clone(), seen.clone());
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(every_s.max(1))).await;
                let args = ["--query-gpu=index,utilization.gpu,memory.used", "--format=csv,noheader,nounits"];
                let Some(csv) = capture("nvidia-smi", &args).await else { break };
                let Some((busy, line)) = gpu_use_line(&csv, expected) else { break };
                log.line(line);
                p.fetch_max(busy, Ordering::Relaxed);
                s.store(1, Ordering::Relaxed);
            }
        });
        Some(Sampler { peak, seen, task })
    }

    pub fn stop(&self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_prefers_in_order_and_says_what_it_found() {
        assert_eq!(backends_tuple(&["OPTIX".into(), "CUDA".into()]), "('OPTIX','CUDA',)");
        assert!(probe_script(&["CUDA".into()]).contains("for backend in ('CUDA',):"));
        let out = "Blender 5.2\nNGPU 1 OPTIX\nBACKENDS OPTIX:1\n";
        assert_eq!(parse_probe(out), (Some("BACKENDS OPTIX:1".into()), 1, "OPTIX".into()));
        assert_eq!(parse_probe("crash").1, 0);
        assert_eq!(parse_probe("NGPU 0 none\n").2, "none");
    }

    #[test]
    fn a_partial_host_is_counted_not_guessed() {
        let proc_dir = tempfile::tempdir().expect("test");
        let dev_dir = tempfile::tempdir().expect("test");
        for g in ["0000:01:00.0", "0000:02:00.0"] {
            std::fs::create_dir(proc_dir.path().join(g)).expect("test");
        }
        for n in ["nvidia0", "nvidiactl", "nvidia-uvm"] {
            std::fs::write(dev_dir.path().join(n), "").expect("test");
        }
        assert_eq!(gpu_counts(proc_dir.path(), dev_dir.path()), (2, 1), "nvidiactl and nvidia-uvm are not cards");
    }

    #[test]
    fn a_gpu_use_line_counts_the_busy_ones() {
        let (busy, line) = gpu_use_line("0, 87, 5120\n1, 3, 400\n", 2).expect("test");
        assert_eq!(busy, 1);
        assert_eq!(line, "GPU-USE 1/2 actifs  gpu0=87%/5120MiB gpu1=3%/400MiB ");
        assert!(gpu_use_line("", 1).is_none());
    }
}
