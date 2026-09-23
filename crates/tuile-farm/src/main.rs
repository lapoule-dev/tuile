// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A farm job's only way in and out of object storage.
//!
//! ```text
//! tuile-farm get <key> <dest>                  parallel ranged download
//! tuile-farm put <src> <key>                   multipart upload
//! tuile-farm exists <key>                      exit 0 present, 1 absent
//! tuile-farm list <prefix>                     "<size> <key>" per line
//! tuile-farm receipt <run> <task> <first> <last> <seg>[,<seg>…]
//! tuile-farm assemble <run> <task-count> <workdir> [fps]
//! tuile-farm concat <out> <segment>…           join locally, print the frames
//! tuile-farm render                            the render job itself (`job`)
//! ```
//!
//! The store comes from the environment: `TUILE_STORE_DIR` for a directory,
//! otherwise `TUILE_STORE_ENDPOINT`, `TUILE_STORE_BUCKET`,
//! `TUILE_STORE_ACCESS_KEY_ID` and `TUILE_STORE_SECRET_ACCESS_KEY` for a
//! bucket. Credentials never appear on a command line, where `ps` and a job's
//! own trace would show them.
//!
//! `receipt` and `assemble` act on ONE render of the run: its identity is
//! `TUILE_RENDER_ID`, or else `CLOUD_RUN_EXECUTION`, which Cloud Run sets on
//! every task of an execution — so the tasks of one render agree on it without
//! being told, and an earlier render's receipts are never counted.
//!
//! Exit codes: 0 done, 1 absent (`exists` only) or failed, 2 usage. The lines
//! a job script greps for — `FILM-UP`, `FILM-WAITING` — go to stdout; progress
//! goes to stderr.

use std::path::Path;
use std::process::ExitCode;

use tuile_farm::assemble::{self, Outcome, Receipt, RunKeys};
use tuile_farm::{ObjectRunStore, RunStore};

const USAGE: &str = "usage: tuile-farm get <key> <dest> | put <src> <key> | exists <key> | \
list <prefix> | receipt <run> <task> <first> <last> <seg,…> | assemble <run> <task-count> <workdir> [fps] | \
concat <out> <segment>… | render";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tuile-farm: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(&args))
}

async fn run(args: &[String]) -> ExitCode {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    // The render job: everything it needs is in its environment.
    if a.as_slice() == ["render"] {
        return ExitCode::from(tuile_farm::job::main(&|k| std::env::var(k).ok()).await);
    }
    // The one command that touches no store, so it needs no credentials.
    if let ["concat", out, segments @ ..] = a.as_slice() {
        if segments.is_empty() {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        let inputs: Vec<std::path::PathBuf> = segments.iter().map(Into::into).collect();
        return match tuile_farm::concat::concat(&inputs, Path::new(out))
            .and_then(|_| tuile_farm::concat::count_frames(Path::new(out)))
        {
            Ok(frames) => {
                println!("FILM {out} {frames} frames");
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("FILM-FAILED {e}");
                ExitCode::FAILURE
            }
        };
    }
    // Usage is checked before the store is built, so a typo is reported as a
    // typo and not as a missing credential.
    let known = matches!(
        a.as_slice(),
        ["get", _, _] | ["put", _, _] | ["exists", _] | ["list", _] | ["receipt", _, _, _, _, _]
            | ["assemble", _, _, _]
            | ["assemble", _, _, _, _]
    );
    if !known {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let store = match ObjectRunStore::from_env() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tuile-farm: {e}");
            return ExitCode::FAILURE;
        }
    };
    let outcome: Result<ExitCode, String> = match a.as_slice() {
        ["get", key, dest] => store.get(key, Path::new(dest)).await.map(|_| ExitCode::SUCCESS),
        ["put", src, key] => store.put(Path::new(src), key).await.map(|_| ExitCode::SUCCESS),
        ["exists", key] => store
            .exists(key)
            .await
            .map(|there| if there { ExitCode::SUCCESS } else { ExitCode::from(1) }),
        ["list", prefix] => store.list(prefix).await.map(|entries| {
            for e in entries {
                println!("{} {}", e.size, e.key);
            }
            ExitCode::SUCCESS
        }),
        ["receipt", run, task, first, last, segs] => {
            return receipt(&store, run, task, first, last, segs).await;
        }
        ["assemble", run, count, work] => {
            return film(&store, run, count, Path::new(work), None).await;
        }
        ["assemble", run, count, work, fps] => {
            return film(&store, run, count, Path::new(work), Some(fps)).await;
        }
        _ => return ExitCode::from(2),
    }
    .map_err(|e| e.to_string());
    match outcome {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tuile-farm: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Which render of the run this process belongs to.
fn render_id() -> String {
    tuile_farm::job::render_id(&|k| std::env::var(k).ok())
}

async fn receipt(
    store: &ObjectRunStore,
    run: &str,
    task: &str,
    first: &str,
    last: &str,
    segs: &str,
) -> ExitCode {
    let parsed = (|| {
        Some(Receipt {
            task: task.parse().ok()?,
            first: first.parse().ok()?,
            last: last.parse().ok()?,
            segments: segs.split(',').map(|s| s.trim().parse().ok()).collect::<Option<_>>()?,
        })
    })();
    let Some(receipt) = parsed else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let keys = RunKeys::new(run, &render_id());
    let result = async {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let file = dir.path().join("receipt.json");
        let body = serde_json::to_vec(&receipt).map_err(|e| e.to_string())?;
        std::fs::write(&file, body).map_err(|e| e.to_string())?;
        store.put(&file, &keys.receipt(receipt.task)).await.map_err(|e| e.to_string())
    }
    .await;
    match result {
        Ok(_) => {
            println!(
                "RECEIPT {} frames {}:{} segments {:?}",
                receipt.task, receipt.first, receipt.last, receipt.segments
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tuile-farm: receipt: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn film(
    store: &ObjectRunStore,
    run: &str,
    count: &str,
    work: &Path,
    fps: Option<&str>,
) -> ExitCode {
    let fps = match fps.map(str::parse::<f64>) {
        None => None,
        Some(Ok(f)) if f > 0.0 => Some(f),
        Some(_) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let Ok(count) = count.parse::<u32>() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match assemble::assemble(store, &RunKeys::new(run, &render_id()), count, work, fps).await {
        Ok(Outcome::Waiting { done, of }) => {
            println!("FILM-WAITING {done}/{of} receipts");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Assembled { key, frames, bytes }) => {
            println!("FILM-UP {key} {frames} frames {bytes} bytes");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("FILM-FAILED {e}");
            ExitCode::FAILURE
        }
    }
}
