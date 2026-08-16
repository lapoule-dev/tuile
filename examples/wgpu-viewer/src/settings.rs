// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The knobs a session is started with, and the one it must be shut down by.
//!
//! Every one of these is an environment variable rather than a flag, because
//! they are all things one wants to change *between two runs of the same
//! command* while comparing — and a comparison whose two halves were launched
//! differently is not a comparison.

use tuile_storage_foyer::FoyerStore;

/// Logs to stderr; `RUST_LOG` overrides. Default shows tile streaming
/// (`tuile_planetary=debug`) plus app-level info.
pub(crate) fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tuile_planetary=debug".into()),
        )
        .without_time()
        .with_target(false)
        .init();
}

/// The instant the scene is lit for, UTC seconds since the Unix epoch.
///
/// `TUILE_LIT_AT` overrides it, so a session can be pinned to a stated moment —
/// which is the only way two runs, or two machines, can be compared. Without it
/// the answer is "now", and "now" is never the same twice.
///
/// Seconds rather than a formatted date because this crate has no calendar in
/// it and adding one to parse a debugging knob would be the wrong trade. `date
/// -u -d '2024-06-21 06:00' +%s` produces the number.
pub(crate) fn lit_at() -> anyhow::Result<f64> {
    if let Ok(pinned) = std::env::var("TUILE_LIT_AT") {
        let seconds: f64 = pinned
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("TUILE_LIT_AT must be UTC seconds, got {pinned:?}"))?;
        tracing::info!("scene lit for the instant TUILE_LIT_AT={seconds}");
        return Ok(seconds);
    }
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64())
}

/// Which level is held in memory for the whole session, from `TUILE_PIN_LEVEL`.
///
/// The count is `4^level`, so the choice is a memory decision and a steep one:
/// level 5 is 1024 tiles over the globe and about 341 MiB of imagery if every
/// one were held; level 8 is 65 536 tiles and 21 GiB, which this machine does
/// not have. Unset means level 4; `none` disables the pin.
///
/// Level 4 rather than 5 because the window is held shut until the whole pinned
/// pyramid is on the GPU, and the pyramid is a quarter of the size at each step
/// up: 2730 tiles through level 5 against 682 through level 4. What is bought
/// with the other three quarters is a fallback texel twice as wide — a blurrier
/// stand-in for the moment before real data lands, not a gap.
pub fn pinned_level() -> Option<u32> {
    let Ok(setting) = std::env::var("TUILE_PIN_LEVEL") else {
        return Some(4);
    };
    if setting.trim().eq_ignore_ascii_case("none") {
        tracing::info!("no pinned level: every tile is evictable");
        return None;
    }
    match setting.trim().parse::<u32>() {
        Ok(level) => {
            let tiles = 4u64.saturating_pow(level);
            tracing::info!(
                level,
                tiles_over_the_globe = tiles,
                imagery_mib = tiles * 341 / 1024,
                "pinning a level in memory"
            );
            Some(level)
        }
        Err(_) => {
            tracing::warn!("TUILE_PIN_LEVEL={setting:?} is not a level; keeping the default");
            Some(4)
        }
    }
}

/// Flushes the tile store, and says what it cost if it could not.
///
/// Foyer buffers its disk writes, so **a store that is merely dropped leaves
/// the disk exactly as cold as it found it** — a session killed without this
/// keeps nothing, however long it ran. That is not a hypothetical: several
/// sessions were killed today, and the next one re-fetched everything they had
/// downloaded, at megabytes a second, which reads from the outside as a cache
/// that does not work.
///
/// So it is reported either way, with the numbers that say whether the cache
/// earned its keep this session. `SIGKILL` still takes the buffer with it and
/// nothing can be done about that from inside the process.
pub(crate) fn close_the_store(store: Option<std::sync::Arc<FoyerStore>>, handle: &tokio::runtime::Handle) {
    let m = tuile_core::metrics::metrics();
    let (hits, misses) = (m.store_hits.get(), m.store_misses.get());
    let served = m.store_bytes_served.get();
    let fetched = m.store_bytes_fetched.get();
    let asked = hits + misses;
    let hit_rate = if asked == 0 {
        0.0
    } else {
        100.0 * hits as f64 / asked as f64
    };
    let Some(store) = store else {
        tracing::warn!("no tile store this session; every tile came from the network");
        return;
    };
    match handle.block_on(store.close()) {
        Ok(()) => tracing::info!(
            "tile store flushed: {hits}/{asked} served from cache ({hit_rate:.0}%), \
             {:.0} MiB served, {:.0} MiB fetched",
            served as f64 / (1024.0 * 1024.0),
            fetched as f64 / (1024.0 * 1024.0),
        ),
        Err(e) => tracing::error!(
            "TILE STORE NOT FLUSHED ({e}): the {:.0} MiB fetched this session are lost \
             and the next run starts cold",
            fetched as f64 / (1024.0 * 1024.0),
        ),
    }
}

