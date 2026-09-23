// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `tuile-farm render`, the real binary, end to end — with a stand-in for
//! Blender: a shell script that answers the GPU probe and "renders" by
//! copying a real MP4 segment for its frames. Everything else is the job's
//! own: configuration, slicing, processes, logs, segments, receipts, the film
//! and the archives, through a directory store.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tuile_farm::concat::{count_frames, write_test_segment};

const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];

/// A fake `blender`: `--python-expr` is the probe (or a diagnostic);
/// otherwise it prints like Blender, writes an error to stderr, and copies
/// `$FIXTURES/<a>-<b>.mp4` to `--video` — unless told to fail or hang.
const FAKE_BLENDER: &str = r##"#!/bin/sh
case " $* " in
  *" --python-expr "*) echo "NGPU ${FAKE_NGPU:-1} CUDA"; echo "BACKENDS CUDA:${FAKE_NGPU:-1}"; exit 0 ;;
esac
while [ $# -gt 0 ]; do
  case "$1" in
    --frames) F="$2"; shift ;;
    --video) V="$2"; shift ;;
  esac
  shift
done
echo "Fra:1 Mem:12M chatter"
echo "frame ${F%%:*}: début"
echo "ERROR (fake): a line for the live log" >&2
[ -n "$FAKE_HANG" ] && exec sleep 600
[ "$FAKE_FAIL" = "${F%%:*}" ] && { echo "Writing: $TMPDIR/blender.crash.txt"; echo "# backtrace" > "$TMPDIR/blender.crash.txt"; exit 139; }
: > "$V"
cp "$FIXTURES/$(echo "$F" | tr : -).mp4" "$V"
"##;

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Rig {
    /// Fixtures for every `(a, b)` range a process may be handed.
    fn new(ranges: &[(u64, u64)]) -> Rig {
        let dir = tempfile::tempdir().expect("test");
        let root = dir.path().to_path_buf();
        for d in ["bin", "fixtures", "store", "out"] {
            std::fs::create_dir_all(root.join(d)).expect("test");
        }
        let blender = root.join("bin/blender");
        std::fs::write(&blender, FAKE_BLENDER).expect("test");
        std::fs::set_permissions(&blender, std::fs::Permissions::from_mode(0o755)).expect("test");
        for (i, &(a, b)) in ranges.iter().enumerate() {
            let frames = (b - a + 1) as u32;
            write_test_segment(&root.join(format!("fixtures/{a}-{b}.mp4")), frames, SPS, i as u8).expect("test");
        }
        std::fs::write(root.join("stage.usda"), "#usda 1.0\n").expect("test");
        Rig { _dir: dir, root }
    }

    fn out(&self, name: &str) -> PathBuf {
        self.root.join("out").join(name)
    }

    fn command(&self, out: &str, env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tuile-farm"));
        cmd.arg("render")
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.root.join("bin").display()))
            .env("FIXTURES", self.root.join("fixtures"))
            .env("TUILE_STORE_DIR", self.root.join("store"))
            .env("JOB_STAGE", self.root.join("stage.usda"))
            .env("JOB_OUT", self.root.join("out").join(out).join("render.mp4"))
            .env("TUILE_CACHE_DIR", self.root.join("cache"))
            .env("JOB_FPS", "60")
            .env("JOB_BAIL_SLEEP", "0")
            .env("JOB_TRACE", "0");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }

    fn run(&self, out: &str, env: &[(&str, &str)]) -> Output {
        self.command(out, env).output().expect("test")
    }

    fn store(&self, key: &str) -> PathBuf {
        self.root.join("store").join(key)
    }
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn tar_names(path: &Path) -> Vec<String> {
    let f = std::fs::File::open(path).expect("test");
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let mut v: Vec<_> = a.entries().expect("test").map(|e| e.expect("test").path().expect("test").display().to_string()).collect();
    v.sort();
    v
}

#[test]
fn a_local_render_makes_its_film_and_says_so() {
    let rig = Rig::new(&[(1, 50), (51, 100), (101, 150), (151, 200)]);
    let o = rig.run("local", &[("JOB_FRAMES", "1:200")]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("probe: NGPU 1 backend=CUDA (attendu: NGPU 1)"), "{out}");
    assert!(out.contains("FILM ") && out.contains(" 200 frames"), "{out}");
    assert!(out.contains("frames: 200/200") && out.contains("RENDER-DONE"), "{out}");
    assert_eq!(count_frames(&rig.out("local/render.mp4")).expect("test"), 200);
    // What the processes said: tagged, the chatter dropped, the errors raised.
    assert!(out.contains("[gpu0-j2] frame 101: début"), "{out}");
    assert!(!out.contains("Fra:1"), "Blender's chatter stays out: {out}");
    assert!(out.contains("[err0] ERROR (fake)"), "{out}");
    let job_log = std::fs::read_to_string(rig.out("local/job.log")).expect("test");
    assert!(job_log.contains("RENDER-DONE"), "job.log holds everything");
    assert!(std::fs::read_to_string(rig.out("local/log-s1.txt")).expect("test").contains("[gpu0-j1] frame 51: début"));
    assert!(std::fs::read_to_string(rig.out("local/trace-s3.jsonl")).expect("test").contains("ERROR (fake)"));
}

#[test]
fn chunks_run_by_an_orchestrator_meet_in_one_film() {
    let rig = Rig::new(&[(1, 50), (51, 100), (101, 150), (151, 200), (201, 250), (251, 300)]);
    let common = [("JOB_RUN_PREFIX", "renders/r1/"), ("TUILE_RENDER_ID", "wf-42"), ("JOB_TASK_COUNT", "2")];
    let first = rig.run("c0", &[common[0], common[1], common[2], ("JOB_FRAMES", "1:200"), ("JOB_TASK_INDEX", "0")]);
    let out0 = text(&first);
    assert!(first.status.success(), "{out0}");
    assert!(out0.contains("task 0/2: frames 1:200 (chunk given)"), "{out0}");
    assert!(out0.contains("FILM-WAITING 1/2 receipts") && out0.contains("TASK-DONE 0/2"), "{out0}");
    // The second chunk is short: two processes, its own numbers.
    let second = rig.run("c1", &[common[0], common[1], common[2], ("JOB_FRAMES", "201:300"), ("JOB_TASK_INDEX", "1"), ("JOB_PROCS_PER_GPU", "2")]);
    let out1 = text(&second);
    assert!(second.status.success(), "{out1}");
    assert!(out1.contains("SEG-UP 1000 ") && out1.contains("SEG-UP 1001 "), "its own numbers, not chunk 0's: {out1}");
    assert!(out1.contains("FILM-UP renders/r1/render.mp4 300 frames"), "{out1}");
    assert!(out1.contains("RENDER-DONE"), "{out1}");
    assert_eq!(count_frames(&rig.store("renders/r1/render.mp4")).expect("test"), 300);
    assert!(rig.store("renders/r1/tasks/wf-42/0.json").exists() && rig.store("renders/r1/tasks/wf-42/1.json").exists());
    // Each chunk's archive under its own name.
    assert!(tar_names(&rig.store("renders/r1/logs-t0.tar.gz")).contains(&"job.log".to_string()));
    assert!(rig.store("renders/r1/logs-t1.tar.gz").exists());
}

#[test]
fn no_gpu_is_a_bail_and_the_logs_still_go_up() {
    let rig = Rig::new(&[(1, 50)]);
    let o = rig.run("bail", &[("JOB_FRAMES", "1:50"), ("FAKE_NGPU", "0"), ("JOB_RUN_PREFIX", "renders/b")]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.contains("NO-GPU-BAIL"), "{out}");
    assert!(!out.contains("RENDER-DONE"));
    assert!(tar_names(&rig.store("renders/b/logs.tar.gz")).contains(&"job.log".to_string()));
}

#[test]
fn a_dead_process_is_a_missing_video_with_its_crash_report_archived() {
    let rig = Rig::new(&[(1, 50), (51, 100), (101, 150), (151, 200)]);
    let o = rig.run("dead", &[("JOB_FRAMES", "1:200"), ("FAKE_FAIL", "101"), ("JOB_RUN_PREFIX", "renders/d")]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(out.contains("RENDER-EXIT j2 139"), "{out}");
    assert!(out.contains("SEGMENTS-MISSING: 2") && out.contains("TASK-INCOMPLETE 0/1 (3/4 segments)"), "{out}");
    assert!(out.contains("VIDEO-MISSING"), "{out}");
    assert!(!rig.store("renders/d/render.mp4").exists());
    let names = tar_names(&rig.store("renders/d/logs.tar.gz"));
    assert!(names.contains(&"tmp-s2/blender.crash.txt".to_string()), "{names:?}");
}

#[test]
fn sigterm_stops_the_renders_and_ships_the_logs() {
    let rig = Rig::new(&[(1, 2)]);
    let mut child = rig
        .command("term", &[("JOB_FRAMES", "1:2"), ("JOB_PROCS_PER_GPU", "1"), ("FAKE_HANG", "1"), ("JOB_RUN_PREFIX", "renders/t")])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("test");
    // Wait until the render is under way, then do what Cloud Run does.
    let log = rig.out("term/job.log");
    let started = std::time::Instant::now();
    while !std::fs::read_to_string(&log).unwrap_or_default().contains("frame 1: début") {
        assert!(started.elapsed().as_secs() < 30, "the render never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Command::new("kill").args(["-TERM", &child.id().to_string()]).status().expect("test");
    let status = child.wait().expect("test");
    assert_eq!(status.code(), Some(143));
    assert!(std::fs::read_to_string(&log).expect("test").contains("SIGTERM"));
    assert!(rig.store("renders/t/logs.tar.gz").exists(), "the logs went up on the way out");
}

#[test]
fn a_bad_environment_is_refused_before_anything_runs() {
    let rig = Rig::new(&[]);
    let o = rig.run("bad", &[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o).contains("JOB-CONFIG-INVALID JOB_FRAMES is required"));
}
