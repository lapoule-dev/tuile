// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A whole film on the farm, as a one-off job: `tuile-film-produce` run on a
//! card in the cloud, followed from here, the film brought home and opened.
//!
//! ```text
//! tuile-film-job <tape.mcap> [--spatial] [--tasks <n>] [--name <film>]
//!                [--videos <dir>] [--dry-run] [--no-watch] [--attach <execution>]
//!                -- <tuile-film-produce flags>
//! ```
//!
//! The tape is put in the run store (`tapes/<digest>.mcap`) and the job is
//! started with `tuile-film-produce store:tapes/<digest>.mcap <flags>` as its
//! arguments — the same flags as on a workstation. A tape made by
//! `tuile-tape timelapse` carries its clock beside it (`<tape>.clock`): it is
//! passed as `--clock` unless the flags give one. The production writes a
//! receipt (`--receipt runs/films/<run>.json`); the film it names is brought
//! to `<videos>/<name>.mp4` and opened.
//!
//! The jobs are declared by infrastructure code
//! (`integrations/film/infra`, `docs/22-film-jobs.md`), apart from any
//! service: a mono film runs on `TUILE_FILM_JOB` (default
//! `tuile-film-oneoff`, an L4 in `europe-west1`), a spatial one (`--spatial`,
//! MV-HEVC, which the L4's encoder cannot write) on `TUILE_FILM_SPATIAL_JOB`
//! (default `tuile-film-oneoff-spatial`, an RTX PRO 6000 in
//! `europe-west4`). A job is a process: when the film is made it exits, and
//! nothing is left running or billing.
//!
//! Environment: `TUILE_GCP_PROJECT`; `TUILE_FILM_JOB[_REGION]`,
//! `TUILE_FILM_SPATIAL_JOB[_REGION]` to point elsewhere;
//! `GOOGLE_APPLICATION_CREDENTIALS` or a `gcloud` login for the job service;
//! `TUILE_STORE_*` for the run store the job also reads.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tuile_farm::jobs::{default_tokens, HttpTransport, JobsApi};
use tuile_farm::launch::Farm;
use tuile_farm::{ObjectRunStore, RunStore};

#[derive(Parser)]
#[command(
    name = "tuile-film-job",
    about = "Produce a film on the farm as a one-off job, and bring it home"
)]
struct Args {
    /// The tape of cameras.
    tape: PathBuf,
    /// A film for a headset (MV-HEVC): on the job whose card encodes it.
    #[arg(long)]
    spatial: bool,
    /// Tasks, one card each; the last to finish joins the film.
    #[arg(long, default_value_t = 1)]
    tasks: u32,
    /// The film's name in `--videos`. Default: the run's.
    #[arg(long)]
    name: Option<String>,
    /// Where the film is brought.
    #[arg(long, default_value = "videos")]
    videos: PathBuf,
    /// Have the service check the request, and run nothing.
    #[arg(long)]
    dry_run: bool,
    /// Start it and return.
    #[arg(long)]
    no_watch: bool,
    /// Follow an execution already started, by name, instead of starting one;
    /// with `--run` naming its receipt.
    #[arg(long, requires = "run")]
    attach: Option<String>,
    /// The run an attached execution writes its receipt for.
    #[arg(long)]
    run: Option<String>,
    /// `tuile-film-produce`'s flags, after `--`.
    #[arg(last = true)]
    produce: Vec<String>,
}

/// Which job, where.
fn job(spatial: bool) -> (String, String) {
    let var = |name: &str, or: &str| std::env::var(name).unwrap_or_else(|_| or.to_string());
    if spatial {
        (
            var("TUILE_FILM_SPATIAL_JOB", "tuile-film-oneoff-spatial"),
            var("TUILE_FILM_SPATIAL_JOB_REGION", "europe-west4"),
        )
    } else {
        (
            var("TUILE_FILM_JOB", "tuile-film-oneoff"),
            var("TUILE_FILM_JOB_REGION", "europe-west1"),
        )
    }
}

/// The production's arguments: the tape in the store, the flags as given,
/// the tape's clock unless they give one, and the receipt.
fn produce_args(
    tape_key: &str,
    flags: &[String],
    clock: Option<&str>,
    receipt: &str,
) -> Vec<String> {
    let mut args = vec![format!("store:{tape_key}")];
    args.extend(flags.iter().cloned());
    if let Some(clock) = clock {
        if !flags
            .iter()
            .any(|f| f == "--clock" || f.starts_with("--clock="))
        {
            args.push("--clock".into());
            args.push(clock.trim().to_string());
        }
    }
    args.push("--receipt".into());
    args.push(receipt.to_string());
    args
}

fn clock_beside(tape: &Path) -> Option<String> {
    let mut name = tape.as_os_str().to_owned();
    name.push(".clock");
    std::fs::read_to_string(PathBuf::from(name)).ok()
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let project = std::env::var("TUILE_GCP_PROJECT")
        .map_err(|_| "TUILE_GCP_PROJECT is not set: the Google Cloud project of the jobs")?;
    let (job, region) = job(args.spatial);
    let jobs = JobsApi::new(
        Arc::new(HttpTransport::new()?),
        default_tokens()?,
        "https://run.googleapis.com/v2",
        &project,
        &region,
    );
    let store: Arc<dyn RunStore> = Arc::new(ObjectRunStore::from_env()?);
    let farm = Farm { jobs, store };

    let bytes = std::fs::read(&args.tape)?;
    let digest = tuile_pack::scene_digest(&[&bytes]);
    let tape_key = format!("tapes/{digest}.mcap");
    let run = match &args.run {
        Some(run) => run.clone(),
        None => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
            format!("{}-{}", now.as_secs(), &digest[..digest.len().min(8)])
        }
    };
    let receipt = format!("runs/films/{run}.json");
    let execution = match &args.attach {
        Some(name) if name.contains('/') => name.clone(),
        Some(name) => format!("{}/executions/{name}", farm.jobs.job_path(&job)),
        None => {
            if !farm.store.exists(&tape_key).await? {
                farm.store.put(&args.tape, &tape_key).await?;
            }
            let produce = produce_args(
                &tape_key,
                &args.produce,
                clock_beside(&args.tape).as_deref(),
                &receipt,
            );
            println!("job:     {job} ({region}), run {run}");
            println!("args:    {}", produce.join(" "));
            match farm
                .jobs
                .run_with_args(&job, &[], Some(&produce), args.tasks, args.dry_run)
                .await?
            {
                Some(name) => name,
                None => {
                    println!("validateOnly: the request is accepted; nothing ran.");
                    return Ok(());
                }
            }
        }
    };
    println!("execution: {execution}");
    if args.no_watch {
        println!(
            "attach:  tuile-film-job {} --attach {execution} --run {run}",
            args.tape.display()
        );
        return Ok(());
    }
    let ex = farm.follow(&execution).await?;
    if ex.bad() > 0 {
        return Err(format!("{} task(s) failed: read the execution's logs", ex.bad()).into());
    }
    let text = farm
        .read_text(&receipt)
        .await?
        .ok_or_else(|| format!("every task succeeded and {receipt} does not exist"))?;
    let receipt: serde_json::Value = serde_json::from_str(&text)?;
    let film = receipt["film"].as_str().ok_or("a receipt without a film")?;
    std::fs::create_dir_all(&args.videos)?;
    let out = args
        .videos
        .join(format!("{}.mp4", args.name.as_deref().unwrap_or(&run)));
    farm.fetch_film(film, &out).await?;
    println!("FILM {} {}", out.display(), receipt["frames"]);
    Ok(())
}

fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tuile-film-job: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tuile-film-job: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_production_is_given_the_tape_the_flags_its_clock_and_a_receipt() {
        let flags = ["--atmosphere".to_string(), "--fps".into(), "60".into()];
        let args = produce_args(
            "tapes/abc.mcap",
            &flags,
            Some("1792368000,168\n"),
            "runs/films/r.json",
        );
        assert_eq!(
            args,
            [
                "store:tapes/abc.mcap",
                "--atmosphere",
                "--fps",
                "60",
                "--clock",
                "1792368000,168",
                "--receipt",
                "runs/films/r.json"
            ]
        );
        // A clock in the flags wins over the tape's.
        let given = ["--clock".to_string(), "0,1".into()];
        let args = produce_args("t", &given, Some("5,5"), "r");
        assert_eq!(args.iter().filter(|a| *a == "--clock").count(), 1);
        assert!(args.contains(&"0,1".to_string()));
    }

    #[test]
    fn a_spatial_film_goes_to_the_card_that_encodes_it() {
        assert_eq!(job(false).0, "tuile-film-oneoff");
        assert_eq!(
            job(true),
            ("tuile-film-oneoff-spatial".into(), "europe-west4".into())
        );
    }
}
