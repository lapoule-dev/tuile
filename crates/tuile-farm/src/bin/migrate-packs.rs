// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Re-bakes every pack of a pack API's projects as references, in place.
//!
//! ```text
//! migrate-packs <api> <ledger.jsonl> [--max-gb N] [--only <substring>]
//!               [--bake <tuile-bake>] [--work <dir>] [--tiles-bucket <name>]
//!               [--dry-run]
//! ```
//!
//! One pack at a time, smallest first: read its table, choose the
//! screen-space error that best reproduces the old pack's first frame, re-bake
//! every frame, check the new pack refers to the store for every tile, replace
//! the old object. A ledger records each outcome, so a second run picks up
//! where the first stopped.
//!
//! **Only the table of the old pack is read**, and in lots. A re-bake flies
//! the old pack's cameras and compares selections: both are in the table,
//! which a pack puts in front of its payloads. So the table is fetched by
//! byte ranges, a lot at a time, straight onto disk — and the payloads, which
//! are all of a pack's gigabytes, are neither downloaded nor held in memory.
//! What lands on disk is the head of the pack, which is all `tuile-bake
//! --rebake` opens.
//!
//! Environment: `TUILE_ION_TOKEN` for the bake; `TUILE_STORE_ENDPOINT`,
//! `TUILE_STORE_ACCESS_KEY_ID` and `TUILE_STORE_SECRET_ACCESS_KEY` for the
//! buckets (each project names its own bucket, through the API);
//! `TUILE_TILES_BUCKET` for the tile store the new packs refer to.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tuile_farm::{BucketConfig, ObjectRunStore, RunStore, Tuning};

/// Screen-space errors tried on the first frame, likeliest first.
const CANDIDATES: [u32; 5] = [6, 16, 3, 8, 4];
/// A first frame this close to the old one, in tiles, is the setting.
const CLOSE_ENOUGH: u64 = 12;
/// Bytes of a table asked for at once.
const LOT: u64 = 16 << 20;
/// What a bake on a workstation may hold, whatever the environment says: one
/// heavy job at a time, on a machine that has other things open.
const BAKE_KNOBS: [(&str, &str); 4] = [
    ("TUILE_CACHE_MEMORY_MB", "512"),
    ("TUILE_CACHE_DISK_MB", "1024"),
    ("TUILE_TEXTURE_MEMO_MB", "256"),
    ("TUILE_MEMORY_EVERY", "600"),
];

#[derive(Parser)]
#[command(name = "migrate-packs", about = "Re-bake packs as references, in place")]
struct Cli {
    /// The pack API that lists the projects, their films and their packs.
    api: String,
    /// One JSON line per pack outcome; a pack with an outcome is not redone.
    ledger: PathBuf,
    /// Leave larger packs alone.
    #[arg(long)]
    max_gb: Option<f64>,
    /// Only the packs whose key contains this.
    #[arg(long, default_value = "")]
    only: String,
    /// The baker. Default: `tuile-bake` next to this binary.
    #[arg(long)]
    bake: Option<PathBuf>,
    /// Where the table, the new pack and the tile cache live. Default: the
    /// ledger's directory.
    #[arg(long)]
    work: Option<PathBuf>,
    /// The tile store the new packs refer to. Default: `TUILE_TILES_BUCKET`.
    #[arg(long)]
    tiles_bucket: Option<String>,
    /// List what would be migrated, and stop.
    #[arg(long)]
    dry_run: bool,
}

/// One pack of one project, as the API lists it.
#[derive(Debug, Clone, PartialEq, PartialOrd)]
struct Chunk {
    bytes: u64,
    project: String,
    key: String,
    first: u32,
    last: u32,
}

/// A ledger line. Fields in the order they are learnt.
#[derive(Serialize, Default, Clone)]
struct Entry {
    project: String,
    key: String,
    old_bytes: u64,
    frames: [u32; 2],
    /// Bytes of the old pack actually read: its table.
    #[serde(skip_serializing_if = "Option::is_none")]
    table_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sse: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_frame_apart: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tiles: Option<usize>,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seconds: Option<u64>,
}

/// What every pack's migration is handed.
struct Setup {
    bake: PathBuf,
    old: PathBuf,
    new: PathBuf,
    cache: PathBuf,
    tiles_bucket: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("migrate-packs: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let work = match &cli.work {
        Some(dir) => dir.clone(),
        None => cli.ledger.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf(),
    };
    let bake = match &cli.bake {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("tuile-bake"),
    };
    let setup = Setup {
        bake,
        old: work.join("work.old.tuilepack"),
        new: work.join("work.new.tuilepack"),
        cache: work.join("cache"),
        tiles_bucket: cli
            .tiles_bucket
            .clone()
            .or_else(|| std::env::var("TUILE_TILES_BUCKET").ok())
            .filter(|b| !b.is_empty()),
    };

    let (stores, mut packs) = listing(&cli.api).await?;
    packs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let seen = done(&cli.ledger)?;
    let limit = cli.max_gb.map_or(f64::INFINITY, |gb| gb * 1e9);
    let wanted: Vec<Chunk> = packs
        .into_iter()
        .filter(|p| {
            !seen.contains(&(p.project.clone(), p.key.clone()))
                && p.key.contains(&cli.only)
                && p.bytes as f64 <= limit
        })
        .collect();
    if cli.dry_run {
        for p in &wanted {
            println!("{} {} {} frames {}:{}", p.bytes, p.project, p.key, p.first, p.last);
        }
        println!("{} packs to migrate", wanted.len());
        return Ok(());
    }
    // Checked once there is something to bake, and before anything is.
    if !wanted.is_empty() && std::env::var("TUILE_ION_TOKEN").map_or(true, |t| t.is_empty()) {
        return Err("TUILE_ION_TOKEN is not set; a bake is the one job that needs it".into());
    }
    if !wanted.is_empty() && !setup.bake.exists() {
        return Err(format!("{}: no such baker (see --bake)", setup.bake.display()));
    }

    for pack in wanted {
        let began = Instant::now();
        let mut entry = Entry {
            project: pack.project.clone(),
            key: pack.key.clone(),
            old_bytes: pack.bytes,
            frames: [pack.first, pack.last],
            ..Entry::default()
        };
        let place = stores.iter().find(|(name, _)| *name == pack.project).map(|(_, s)| s.as_str());
        let outcome = match place {
            Some(place) => migrate(&setup, place, &pack, &mut entry).await,
            None => Err(format!("project {} has no store", pack.project)),
        };
        match outcome {
            Ok(outcome) => {
                entry.outcome = outcome.to_string();
                // A pack that was already references took no time worth noting.
                if outcome == "replaced" {
                    entry.seconds = Some(began.elapsed().as_secs());
                }
            }
            // One pack's failure is that pack's.
            Err(why) => {
                entry.outcome = "failed".into();
                entry.why = Some(tail(&why, 300));
                entry.seconds = Some(began.elapsed().as_secs());
            }
        }
        note(&cli.ledger, &entry)?;
        for path in [&setup.old, &setup.new] {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// The projects' stores, and every pack of every film.
async fn listing(api: &str) -> Result<(Vec<(String, String)>, Vec<Chunk>), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let get = async |segments: &[&str]| -> Result<serde_json::Value, String> {
        let mut url = url::Url::parse(api).map_err(|e| format!("{api}: {e}"))?;
        url.path_segments_mut()
            .map_err(|()| format!("{api}: not a base URL"))?
            .pop_if_empty()
            .extend(segments);
        let body = client
            .get(url.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("{url}: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        serde_json::from_slice(&body).map_err(|e| format!("{url}: {e}"))
    };
    let text = |v: &serde_json::Value, field: &str| -> Result<String, String> {
        v[field].as_str().map(str::to_string).ok_or_else(|| format!("the API gave no {field}"))
    };
    let number = |v: &serde_json::Value, field: &str| -> Result<u64, String> {
        v[field].as_u64().ok_or_else(|| format!("the API gave no {field}"))
    };

    let mut stores = Vec::new();
    for project in get(&["projects"]).await?["projects"].as_array().into_iter().flatten() {
        stores.push((text(project, "name")?, text(project, "store")?));
    }
    let mut packs = Vec::new();
    for (project, _) in &stores {
        let films = get(&["p", project, "films"]).await?;
        for film in films.as_array().into_iter().flatten() {
            let id = text(film, "id")?;
            // A film's id is a path, and each of its segments is one of the URL's.
            let mut segments = vec!["p", project.as_str(), "films"];
            segments.extend(id.split('/'));
            let film = get(&segments).await?;
            for chunk in film["chunks"].as_array().into_iter().flatten() {
                packs.push(Chunk {
                    bytes: number(chunk, "bytes")?,
                    project: project.clone(),
                    key: text(chunk, "key")?,
                    first: number(chunk, "first")? as u32,
                    last: number(chunk, "last")? as u32,
                });
            }
        }
    }
    Ok((stores, packs))
}

/// The packs the ledger has an outcome for. A failure is not an outcome: the
/// pack is tried again.
fn done(ledger: &Path) -> Result<BTreeSet<(String, String)>, String> {
    let mut seen = BTreeSet::new();
    let text = match std::fs::read_to_string(ledger) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(seen),
        Err(e) => return Err(format!("{}: {e}", ledger.display())),
    };
    for (index, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let entry: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("{} line {}: {e}", ledger.display(), index + 1))?;
        if entry["outcome"] != "failed" {
            if let (Some(project), Some(key)) = (entry["project"].as_str(), entry["key"].as_str()) {
                seen.insert((project.to_string(), key.to_string()));
            }
        }
    }
    Ok(seen)
}

fn note(ledger: &Path, entry: &Entry) -> Result<(), String> {
    let line = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
        .map_err(|e| format!("{}: {e}", ledger.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("{}: {e}", ledger.display()))?;
    println!("{} {line}", chrono::Local::now().format("%H:%M:%S"));
    Ok(())
}

/// A project's store, as the API names it: `bucket <name>` or `dir <path>`.
fn open(place: &str) -> Result<ObjectRunStore, String> {
    let tuning = Tuning::from_env();
    if let Some(dir) = place.strip_prefix("dir ") {
        return ObjectRunStore::local(Path::new(dir), tuning).map_err(|e| e.to_string());
    }
    let bucket = place.strip_prefix("bucket ").ok_or_else(|| format!("{place:?} is not a store"))?;
    let var = |name: &str| {
        std::env::var(name).ok().filter(|v| !v.is_empty()).ok_or_else(|| format!("{name} is not set"))
    };
    let config = BucketConfig {
        endpoint: var("TUILE_STORE_ENDPOINT")?,
        bucket: bucket.to_string(),
        access_key_id: var("TUILE_STORE_ACCESS_KEY_ID")?,
        secret_access_key: var("TUILE_STORE_SECRET_ACCESS_KEY")?,
        region: std::env::var("TUILE_STORE_REGION").unwrap_or_else(|_| "auto".into()),
    };
    ObjectRunStore::bucket(&config, tuning).map_err(|e| e.to_string())
}

/// Writes the head of `key` — its preamble and its table, and none of its
/// payloads — to `dest`, a lot at a time. Returns its length.
///
/// At most one lot is in memory, whatever the table's size.
async fn fetch_table(
    store: &dyn RunStore,
    key: &str,
    size: u64,
    lot: u64,
    dest: &Path,
) -> Result<u64, String> {
    let preamble = tuile_pack::PREAMBLE as u64;
    if size < preamble {
        return Err(format!("{key}: {size} bytes is not a pack"));
    }
    let head = store.get_range(key, 0..preamble).await.map_err(|e| e.to_string())?;
    let start = tuile_pack::blob_start(&head).map_err(|e| format!("{key}: {e}"))?;
    if start > size {
        return Err(format!("{key}: its table ends at {start}, past its {size} bytes"));
    }
    let mut file = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    file.write_all(&head).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut at = preamble;
    while at < start {
        let end = (at + lot.max(1)).min(start);
        let bytes = store.get_range(key, at..end).await.map_err(|e| e.to_string())?;
        if bytes.len() as u64 != end - at {
            return Err(format!("{key}: bytes {at}..{end} came back {} long", bytes.len()));
        }
        file.write_all(&bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
        at = end;
    }
    file.sync_all().map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(start)
}

/// What the old pack is, from the head [`fetch_table`] wrote.
fn old_pack(head: &Path) -> Result<(tuile_pack::Content, u32, u32), String> {
    let bytes = std::fs::read(head).map_err(|e| format!("{}: {e}", head.display()))?;
    let pack = tuile_pack::Pack::open_table(&bytes).map_err(|e| e.to_string())?;
    let (first, last) = pack.frame_range();
    Ok((pack.content(), first, last))
}

/// Fails unless the pack at `path` is references to the store for every tile
/// of frames `first..=last`. Returns how many distinct tiles it draws.
fn check_new(path: &Path, first: u32, last: u32) -> Result<usize, String> {
    let not = |what: String| format!("the new pack is not what was asked for: {what}");
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let pack = tuile_pack::Pack::open(&bytes).map_err(|e| not(e.to_string()))?;
    if pack.content() != tuile_pack::Content::References {
        return Err(not(format!("content {:?}", pack.content())));
    }
    if pack.frame_range() != (first, last) {
        let (a, b) = pack.frame_range();
        return Err(not(format!("frames {a}..={b}, wanted {first}..={last}")));
    }
    let mut seen = BTreeSet::new();
    let mut referring = 0usize;
    for frame in first..=last {
        for tile in pack.frame(frame).map_err(|e| not(format!("frame {frame}: {e}")))? {
            if seen.insert((tile.id(), tile.drape())) && tuile_pack::refs_of(&tile).is_some() {
                referring += 1;
            }
        }
    }
    if referring != seen.len() || seen.is_empty() {
        return Err(not(format!("{referring} of {} tiles refer to the store", seen.len())));
    }
    Ok(seen.len())
}

/// One pack: its table in, its re-bake out. Returns the outcome's name.
async fn migrate(
    setup: &Setup,
    place: &str,
    pack: &Chunk,
    entry: &mut Entry,
) -> Result<&'static str, String> {
    for path in [&setup.old, &setup.new] {
        let _ = std::fs::remove_file(path);
    }
    let store = open(place)?;
    let size = store.size(&pack.key).await.map_err(|e| e.to_string())?;
    let table = fetch_table(&store, &pack.key, size, LOT, &setup.old).await?;
    entry.table_bytes = Some(table);
    eprintln!("TABLE {} ← {}: {table} of {size} bytes read", pack.key, store.label());

    // A pack this already replaced, or one baked as references since.
    let (content, first, last) = old_pack(&setup.old)?;
    if content == tuile_pack::Content::References {
        return Ok("already references");
    }
    entry.frames = [first, last];

    // The setting that reproduces the old pack's first frame best.
    let mut best: Option<(u64, u32)> = None;
    for sse in CANDIDATES {
        let probe = rebake(setup, sse, Some((first, first))).await?;
        if !probe.done {
            return Err(format!("probe at sse {sse}: {}", probe.verdict));
        }
        if best.is_none_or(|(apart, _)| probe.apart < apart) {
            best = Some((probe.apart, sse));
        }
        if probe.apart <= CLOSE_ENOUGH {
            break;
        }
    }
    let (apart, sse) = best.ok_or("no setting was tried")?;
    entry.sse = Some(sse);
    entry.first_frame_apart = Some(apart);

    let whole = rebake(setup, sse, None).await?;
    entry.verdict = Some(tail(&whole.verdict, 160));
    if !whole.done {
        return Err(format!("rebake: {}", whole.verdict));
    }
    entry.tiles = Some(check_new(&setup.new, first, last)?);
    entry.new_bytes =
        Some(std::fs::metadata(&setup.new).map_err(|e| format!("{}: {e}", setup.new.display()))?.len());

    // The object this replaces is still the one whose table was read: a pack
    // re-baked by someone else in the hours this took is not overwritten.
    let now = store.size(&pack.key).await.map_err(|e| e.to_string())?;
    if now != size {
        return Err(format!("{} changed while it was re-baked: {size} bytes then, {now} now", pack.key));
    }
    store.put(&setup.new, &pack.key).await.map_err(|e| format!("put failed: {e}"))?;
    Ok("replaced")
}

/// How a re-bake ended.
struct Rebaked {
    /// The baker exited zero.
    done: bool,
    /// Tiles between the old selection and the new one; 0 when they agree.
    apart: u64,
    verdict: String,
}

/// Runs `tuile-bake --rebake` over the old pack's table, for `frames` or for
/// all of them.
async fn rebake(setup: &Setup, sse: u32, frames: Option<(u32, u32)>) -> Result<Rebaked, String> {
    let mut command = tokio::process::Command::new(&setup.bake);
    command
        .arg("--rebake")
        .arg(&setup.old)
        .arg("--out")
        .arg(&setup.new)
        .args(["--sse", &sse.to_string(), "--accept-drift"]);
    if let Some((first, last)) = frames {
        command.args(["--frames", &format!("{first}:{last}")]);
    }
    command.env("TUILE_CACHE_DIR", &setup.cache).envs(BAKE_KNOBS);
    if let Some(bucket) = &setup.tiles_bucket {
        command.env("TUILE_TILES_BUCKET", bucket);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{}: {e}", setup.bake.display()))?;
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let (mut lines, said, status) = tokio::join!(kept(out), kept(err), child.wait());
    lines.extend(said);
    let done = status.map_err(|e| e.to_string())?.success();
    Ok(read_verdict(done, &lines))
}

/// The lines of a baker's output worth keeping. The rest is read and dropped,
/// so hours of frames never pile up here; a frame line and the memory line are
/// passed on now and then, so a long bake is seen to be alive.
async fn kept(stream: Option<impl AsyncRead + Unpin>) -> Vec<String> {
    let Some(stream) = stream else {
        return Vec::new();
    };
    let mut reader = BufReader::new(stream);
    let mut lines = Vec::new();
    let mut raw = Vec::new();
    let mut shown = Instant::now();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = clean(&String::from_utf8_lossy(&raw));
        if ["REBAKE-", "BAKE-FAIL", "BAKE-DONE"].iter().any(|k| line.contains(k)) {
            lines.push(line);
        } else if line.contains("peak_gib")
            || (line.contains("BAKE-FRAME") && shown.elapsed() >= Duration::from_secs(120))
        {
            shown = Instant::now();
            eprintln!("{line}");
        }
    }
    lines
}

/// What the baker concluded, from the lines kept of it.
fn read_verdict(done: bool, lines: &[String]) -> Rebaked {
    let verdict = lines.iter().find(|l| l.contains("REBAKE-")).cloned().unwrap_or_default();
    // A baker that failed says why on its last line, whatever it said of the
    // selection before.
    let failure = lines.iter().rev().find(|l| l.contains("BAKE-FAIL")).cloned();
    if !done {
        let why = failure.unwrap_or_else(|| {
            let from = lines.len().saturating_sub(2);
            lines[from..].join(" | ")
        });
        return Rebaked { done, apart: u64::MAX, verdict: why };
    }
    if verdict.contains("REBAKE-SAME") {
        return Rebaked { done, apart: 0, verdict };
    }
    let apart = tiles_apart(&verdict).unwrap_or(u64::MAX);
    Rebaked { done, apart, verdict }
}

/// The number after `tiles apart `, in a `REBAKE-DIFFERS` line.
fn tiles_apart(verdict: &str) -> Option<u64> {
    let (_, after) = verdict.split_once("tiles apart ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// A line of a tool's output, fit to print and to keep: no colour codes, no
/// token, no newline, 300 characters at most.
fn clean(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.trim_end().chars().peekable();
    while let Some(c) = chars.next() {
        // `ESC [ … m`, the only escape a log line carries.
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for skipped in chars.by_ref() {
                if skipped == 'm' {
                    break;
                }
            }
            continue;
        }
        plain.push(c);
    }
    // A signed token starts `eyJ` and runs over these characters.
    let mut out = String::with_capacity(plain.len());
    let mut rest = plain.as_str();
    while let Some(at) = rest.find("eyJ") {
        out.push_str(&rest[..at]);
        out.push_str("<redacted>");
        let token = &rest[at..];
        let end = token
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
            .unwrap_or(token.len());
        rest = &token[end..];
    }
    out.push_str(rest);
    out.chars().take(300).collect()
}

/// The last `n` characters of `text`.
fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file shaped like a pack: the preamble, a table of `table` bytes, and
    /// payloads after it.
    fn pack_like(table: usize, payloads: usize) -> Vec<u8> {
        let mut bytes = tuile_pack::MAGIC.to_vec();
        bytes.extend((table as u64).to_le_bytes());
        bytes.extend((0..table).map(|i| (i % 251) as u8));
        bytes.extend(std::iter::repeat_n(0xEE, payloads));
        bytes
    }

    #[tokio::test]
    async fn a_table_is_fetched_in_lots_and_its_payloads_are_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let object = pack_like(1000, 5000);
        std::fs::create_dir_all(dir.path().join("store/run")).expect("mkdir");
        std::fs::write(dir.path().join("store/run/scene.tuilepack"), &object).expect("write");
        let store =
            ObjectRunStore::local(&dir.path().join("store"), Tuning::default()).expect("store");
        let dest = dir.path().join("head");

        // A lot far smaller than the table, and not a divisor of it.
        let start = fetch_table(&store, "run/scene.tuilepack", object.len() as u64, 37, &dest)
            .await
            .expect("fetch");

        assert_eq!(start as usize, tuile_pack::PREAMBLE + 1000);
        let head = std::fs::read(&dest).expect("read");
        assert_eq!(head, object[..start as usize], "the head, byte for byte, and nothing after");
    }

    #[tokio::test]
    async fn a_table_that_runs_past_its_object_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let whole = pack_like(1000, 0);
        let cut = &whole[..500];
        std::fs::create_dir_all(dir.path().join("store")).expect("mkdir");
        std::fs::write(dir.path().join("store/cut.tuilepack"), cut).expect("write");
        let store =
            ObjectRunStore::local(&dir.path().join("store"), Tuning::default()).expect("store");

        let outcome =
            fetch_table(&store, "cut.tuilepack", cut.len() as u64, 64, &dir.path().join("head")).await;

        assert!(outcome.expect_err("refused").contains("past its 500 bytes"));
    }

    #[test]
    fn a_failure_is_not_an_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = dir.path().join("ledger.jsonl");
        std::fs::write(
            &ledger,
            "{\"project\": \"a\", \"key\": \"k1\", \"outcome\": \"replaced\"}\n\
             {\"project\": \"a\", \"key\": \"k2\", \"outcome\": \"failed\", \"why\": \"x\"}\n\
             {\"project\": \"b\", \"key\": \"k3\", \"outcome\": \"already references\"}\n",
        )
        .expect("write");

        let seen = done(&ledger).expect("ledger");

        assert!(seen.contains(&("a".into(), "k1".into())));
        assert!(!seen.contains(&("a".into(), "k2".into())), "a failed pack is tried again");
        assert!(seen.contains(&("b".into(), "k3".into())));
        assert!(done(&dir.path().join("absent.jsonl")).expect("no ledger yet").is_empty());
    }

    #[test]
    fn a_verdict_is_read_from_the_bakers_lines() {
        let same = read_verdict(true, &["REBAKE-SAME frames=24 tiles and drapes".to_string()]);
        assert_eq!((same.done, same.apart), (true, 0));

        let differs = read_verdict(
            true,
            &["REBAKE-DIFFERS 49 of 848 frames do not draw what the old pack drew (tiles apart 83) — accepted"
                .to_string()],
        );
        assert_eq!((differs.done, differs.apart), (true, 83));

        // A baker that printed a verdict and then failed is a failure, and
        // says why.
        let failed = read_verdict(
            false,
            &[
                "REBAKE-DIFFERS 1 of 1 frames … (tiles apart 333) — accepted".to_string(),
                "BAKE-FAIL flushing the tile store: denied".to_string(),
            ],
        );
        assert!(!failed.done);
        assert_eq!(failed.verdict, "BAKE-FAIL flushing the tile store: denied");
    }

    #[test]
    fn a_kept_line_carries_no_colour_and_no_token() {
        let line = "\u{1b}[32m INFO\u{1b}[0m fetching https://x/y?access_token=eyJhbGci.OiJI-Uz_I1 done\n";
        assert_eq!(clean(line), " INFO fetching https://x/y?access_token=<redacted> done");
        assert_eq!(clean(&"é".repeat(400)).chars().count(), 300);
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
    }
}
