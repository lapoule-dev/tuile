// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the engine is doing, as numbers that can be watched rather than
//! reasoned about.
//!
//! Every hard bug in this engine so far has been found by a counter and lost
//! time to a hypothesis. A request count frozen at thirty-nine said "loads are
//! being killed and respawned" days before anyone worked that out from the code;
//! an optical depth of six hundred said "this formula is wrong from orbit"; a
//! selection spanning the globe said "it is not the traversal". None of those
//! were visible in a render, and all of them were one number away.
//!
//! # Shape
//!
//! Prometheus's model, because it is the one that survives contact with a real
//! session: **counters** only go up, **gauges** are read at an instant, and a
//! **timer** is a sum and a count so a mean falls out without keeping samples.
//! Nothing here allocates, nothing locks, and nothing is behind a feature — a
//! metric that is only there when someone remembered to turn it on is not there
//! when it is needed.
//!
//! [`Metrics::one_line`] renders the whole set as a single line and
//! [`Metrics::by_level_line`] breaks a per-level counter out, so this can be
//! dumped to a log, put in a status bar, or diffed between two runs.

use std::sync::atomic::{AtomicU64, Ordering};

/// Relaxed throughout: these are observations, not synchronisation. A count
/// read a few nanoseconds stale is a count; a fence on every tile load is a
/// measurable cost paid to make no difference to anyone.
const ORDER: Ordering = Ordering::Relaxed;

/// A monotonic count of things that happened.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, ORDER);
    }
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn get(&self) -> u64 {
        self.0.load(ORDER)
    }
}

/// A value read at an instant: how many, how much, right now.
#[derive(Debug, Default)]
pub struct Gauge(AtomicU64);

impl Gauge {
    pub fn set(&self, v: u64) {
        self.0.store(v, ORDER);
    }
    pub fn get(&self) -> u64 {
        self.0.load(ORDER)
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, ORDER);
    }
    /// Saturating, because a gauge is an observation and no observation is
    /// worth a panic. An unbalanced decrement is a bug, but wrapping to
    /// eighteen quintillion turns that bug into a permanently unreadable
    /// number — [`InFlight`] exists so it cannot happen at all.
    // `fetch_update` is `try_update` since 1.95, above the workspace's MSRV.
    #[allow(deprecated)]
    pub fn dec(&self) {
        let _ = self
            .0
            .fetch_update(ORDER, ORDER, |v| Some(v.saturating_sub(1)));
    }
}

/// How long something takes, as a total and a count.
///
/// Deliberately not a histogram. Buckets need to be chosen in advance and the
/// choice is always wrong the first time; a mean plus a peak answers "is this
/// getting worse", which is the question a session actually asks.
#[derive(Debug, Default)]
pub struct Timer {
    nanos: AtomicU64,
    count: AtomicU64,
    peak_nanos: AtomicU64,
}

impl Timer {
    pub fn record(&self, elapsed: std::time::Duration) {
        self.record_sample(elapsed, 1);
    }

    /// Records one duration standing for `weight` of them, as a caller that
    /// times one call in `weight` does: the total grows by `weight` times
    /// it, the count by `weight`, and the peak is the slowest sampled.
    pub fn record_sample(&self, elapsed: std::time::Duration, weight: u64) {
        let nanos = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.nanos.fetch_add(nanos.saturating_mul(weight), ORDER);
        self.count.fetch_add(weight, ORDER);
        self.peak_nanos.fetch_max(nanos, ORDER);
    }

    /// Times `body`, records it, and hands back whatever it returned.
    pub fn time<T>(&self, body: impl FnOnce() -> T) -> T {
        let started = stamp();
        let out = body();
        self.record(started.elapsed());
        out
    }

    pub fn count(&self) -> u64 {
        self.count.load(ORDER)
    }

    pub fn mean_millis(&self) -> f64 {
        let count = self.count();
        if count == 0 {
            return 0.0;
        }
        self.nanos.load(ORDER) as f64 / count as f64 / 1.0e6
    }

    pub fn peak_millis(&self) -> f64 {
        self.peak_nanos.load(ORDER) as f64 / 1.0e6
    }

    /// Everything recorded, added up: with [`Self::count`], the two halves of
    /// a mean that a scraper can take the rate of.
    pub fn total_seconds(&self) -> f64 {
        self.nanos.load(ORDER) as f64 / 1.0e9
    }

    /// The peak since this was last asked, and then forgotten.
    ///
    /// A lifetime peak answers "has this ever been bad", which a session answers
    /// yes to within seconds and never takes back. Stutter is not a lifetime
    /// property: it is *this* second being worse than the last, and reading the
    /// peak destructively is what turns the same counter into that question.
    pub fn take_peak_millis(&self) -> f64 {
        self.peak_nanos.swap(0, ORDER) as f64 / 1.0e6
    }
}

/// The largest a quantity has been since it was last read.
///
/// For the things a stutter is actually made of — the longest frame, the
/// biggest jump the camera made in one — where the mean is not merely
/// uninformative but actively misleading: a hitch every second averages away to
/// nothing while being the entire complaint.
#[derive(Debug, Default)]
pub struct Peak(AtomicU64);

impl Peak {
    /// Records an observation, keeping it only if it is the worst so far.
    ///
    /// Takes an `f64` and keeps it in a fixed-point thousandth so the whole
    /// thing stays one lock-free `fetch_max`. Negative values are clamped to
    /// zero: every quantity this is used for is a magnitude.
    pub fn record(&self, value: f64) {
        let scaled = (value.max(0.0) * 1000.0).min(u64::MAX as f64) as u64;
        self.0.fetch_max(scaled, ORDER);
    }

    /// The peak since this was last asked, and then forgotten.
    ///
    /// Exactly one reader should call this — the one that defines the window.
    /// Everything else reads [`Self::get`] and sees the worst so far within that
    /// same window.
    pub fn take(&self) -> f64 {
        self.0.swap(0, ORDER) as f64 / 1000.0
    }

    /// The peak so far, left in place.
    pub fn get(&self) -> f64 {
        self.0.load(ORDER) as f64 / 1000.0
    }
}

/// How many of something each occurrence of something else carried: a sum, a
/// count and a peak, so the mean and the worst both fall out.
///
/// A [`Timer`] for quantities that are not durations. "How many arrivals did
/// one pass answer" is the question it was made for: a mean of one says every
/// arrival bought its own pass, and a peak says how large the largest burst
/// folded into one was.
#[derive(Debug, Default)]
pub struct Tally {
    sum: AtomicU64,
    count: AtomicU64,
    peak: AtomicU64,
}

impl Tally {
    pub fn record(&self, n: u64) {
        self.sum.fetch_add(n, ORDER);
        self.count.fetch_add(1, ORDER);
        self.peak.fetch_max(n, ORDER);
    }

    /// How many occurrences were recorded.
    pub fn count(&self) -> u64 {
        self.count.load(ORDER)
    }

    /// What they carried, all together.
    pub fn sum(&self) -> u64 {
        self.sum.load(ORDER)
    }

    /// The most any one occurrence carried.
    pub fn peak(&self) -> u64 {
        self.peak.load(ORDER)
    }

    pub fn mean(&self) -> f64 {
        let count = self.count();
        if count == 0 {
            return 0.0;
        }
        self.sum() as f64 / count as f64
    }
}

/// One outstanding piece of work, counted for exactly as long as it is
/// outstanding.
///
/// A fetch ends four ways — served, absent, failed, cancelled — and three of
/// them are early returns. Bracketing a gauge by hand leaks the count on
/// whichever path someone forgets, and a leaked in-flight count is worse than
/// no count: it reads as a download stuck for the rest of the session, which is
/// precisely the symptom these gauges exist to distinguish from a real one.
/// Dropping the guard is the only way to stop counting, so cancellation — where
/// the future is dropped mid-await and no line of ours runs — is handled by the
/// same code as success.
pub struct InFlight<'a> {
    gauge: &'a Gauge,
    timer: &'a Timer,
    started: Stamp,
}

impl<'a> InFlight<'a> {
    pub fn new(gauge: &'a Gauge, timer: &'a Timer) -> Self {
        gauge.inc();
        Self {
            gauge,
            timer,
            started: stamp(),
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.gauge.dec();
        self.timer.record(self.started.elapsed());
    }
}

/// How many quadtree levels a per-level series covers.
///
/// Twenty-five is past anything a source serves and past anything the traversal
/// will divide to, so a level never falls off the end and quietly stops being
/// counted — which is the failure mode that makes a breakdown worse than no
/// breakdown at all.
pub const LEVELS: usize = 25;

/// One series broken down by quadtree level.
///
/// The totals say *how much*; only the breakdown says *where*. Memory that
/// climbs all session looks identical whether it is a thousand coarse tiles or
/// twenty thousand deep ones, and those have opposite fixes.
#[derive(Debug)]
pub struct ByLevel([AtomicU64; LEVELS]);

impl Default for ByLevel {
    fn default() -> Self {
        Self([const { AtomicU64::new(0) }; LEVELS])
    }
}

impl ByLevel {
    /// Levels past the end are folded into the last one rather than dropped: a
    /// miscounted level is a bug, a silently missing one is a mystery.
    fn slot(level: u32) -> usize {
        (level as usize).min(LEVELS - 1)
    }

    pub fn add(&self, level: u32, n: u64) {
        self.0[Self::slot(level)].fetch_add(n, ORDER);
    }
    pub fn inc(&self, level: u32) {
        self.add(level, 1);
    }
    pub fn sub(&self, level: u32, n: u64) {
        self.0[Self::slot(level)].fetch_sub(n, ORDER);
    }
    pub fn set(&self, level: u32, v: u64) {
        self.0[Self::slot(level)].store(v, ORDER);
    }
    pub fn get(&self, level: u32) -> u64 {
        self.0[Self::slot(level)].load(ORDER)
    }
    pub fn clear(&self) {
        for slot in &self.0 {
            slot.store(0, ORDER);
        }
    }
    pub fn total(&self) -> u64 {
        self.0.iter().map(|s| s.load(ORDER)).sum()
    }

    /// Only the levels that are actually carrying something, so a breakdown of
    /// twenty-five rows does not bury the three that matter.
    pub fn occupied(&self) -> Vec<(u32, u64)> {
        (0..LEVELS as u32)
            .map(|l| (l, self.get(l)))
            .filter(|(_, v)| *v > 0)
            .collect()
    }
}

/// Everything the engine reports about itself.
///
/// One instance per process, reached through [`metrics`]. A global rather than
/// something threaded through every call because the interesting numbers come
/// from four crates that do not know about each other, and threading a handle
/// through all of them to count a tile would cost more than it tells.
#[derive(Debug, Default)]
pub struct Metrics {
    // --- Traversal ---
    /// Passes run. One per camera update **and one per load completion**, which
    /// is why this climbs far faster than the frame rate.
    pub traversals: Counter,
    /// The walk itself, [`crate::traversal::traverse`], and nothing of the
    /// bookkeeping around it.
    pub traversal_seconds: Timer,
    /// Building the [`crate::protocol::ServerMessage::Select`] a pass ends
    /// with and handing it to the consumer: the selection and the ancestry of
    /// its whole protected closure, cloned every pass.
    pub select_seconds: Timer,
    /// Load completions handled by each pass a completion provoked. One per
    /// pass while every arrival runs its own traversal; the mean is how many
    /// arrivals one pass answers, the peak the largest burst it folded in.
    /// Passes provoked by the camera are not counted here.
    pub arrivals_per_pass: Tally,
    /// From the wake that let a load finish to the loop handling its result.
    ///
    /// The time a tile that has already arrived spends waiting for the loop to
    /// notice — behind a traversal, behind the camera, behind other arrivals.
    /// Zero would be a loop that is never busy; a peak in the hundreds of
    /// milliseconds is ground held back by the server rather than the network.
    /// Measured from the wake, not from the future resolving: the loads are
    /// polled by the loop alone, so a load resolves only once the loop gets
    /// to it, and stamping there would always read nothing.
    pub arrival_wait_seconds: Timer,
    /// Every wake of a load, to the poll that answered it — one sample per
    /// wake, not per load. Loads advance only when the loop polls them, so
    /// this is the time body reads, decode hand-offs and store lookups spend
    /// waiting on the loop at every step, not only the last one.
    pub load_wake_wait_seconds: Timer,
    pub tiles_visited: Gauge,
    pub tiles_culled: Gauge,
    pub tiles_selected: Gauge,
    /// Ground the traversal drew nothing on while releasing what covered it.
    /// Should be zero on a globe; above zero is a hole.
    pub gaps: Gauge,

    // --- Loading ---
    pub loads_started: Counter,
    pub loads_completed: Counter,
    pub loads_failed: Counter,
    /// Loads killed before they finished. A traversal runs on every arrival, so
    /// cancelling on every traversal livelocks — this is the counter that says
    /// whether that is happening.
    pub loads_cancelled: Counter,
    pub load_seconds: Timer,
    pub loads_in_flight: Gauge,
    /// Tiles built from an ancestor's surface because the source had none.
    pub tiles_upsampled: Counter,
    /// Tiles drawn with no imagery covering them at all — painted in marker
    /// green so the fault is visible rather than argued about.
    pub tiles_without_imagery: Counter,
    /// Drapes the consumer already held, so their layers were never requested:
    /// no download, no decode, no reprojection, no compose.
    ///
    /// Its own counter rather than a store hit. A store hit means the bytes
    /// came from the cache instead of the network, which still pays the decode;
    /// this means nothing happened at all. Adding the two together would make
    /// the hit rate say the cache is working when the work was simply skipped.
    pub drapes_withheld: Counter,
    /// Imagery the provider does not have at all. Expected over ocean and past
    /// the poles at coarse levels, and remembered rather than re-asked — see
    /// [`crate::storage::ABSENT`].
    pub imagery_absent: Counter,
    /// Terrain the source does not have at all.
    pub terrain_absent: Counter,
    /// Imagery that decoded to a fully opaque black image. A provider's way of
    /// saying "no data here" is often a black tile, and a black tile draped on
    /// good geometry is indistinguishable, on screen, from a rendering bug.
    pub black_textures: Counter,

    // --- In flight, by kind ---
    //
    // One tile load is one mesh fetch and up to sixteen texture fetches, over
    // two different services with two different latencies. A single outstanding
    // count averages those together and so cannot answer the question that
    // actually comes up — "is the ground late because the terrain is late, or
    // because the imagery is?" — which have nothing to do with each other.
    /// Terrain fetches outstanding.
    pub meshes_in_flight: Gauge,
    /// Imagery fetches outstanding. Runs an order of magnitude above the mesh
    /// count by construction; what matters is whether it drains.
    pub textures_in_flight: Gauge,
    pub mesh_fetches: Counter,
    pub texture_fetches: Counter,
    /// Time from asking the terrain source to holding the bytes.
    pub mesh_fetch_seconds: Timer,
    /// Time from asking the imagery provider to holding the pixels.
    pub texture_fetch_seconds: Timer,

    // --- Startup ---
    /// How many coarse tiles the session primed onto the GPU before drawing.
    /// Zero until the server has decided, and zero for ever if nothing is
    /// pinned — so a consumer waiting on it must check both this and
    /// [`Metrics::priming_done`].
    pub priming_total: Gauge,
    /// How many coarse tiles have not been answered yet — still queued, or in
    /// flight. Falls to zero, always: a tile the source does not serve is
    /// counted in [`Metrics::priming_unavailable`] instead, not left here.
    ///
    /// It used to count only the queue, which read zero while sixty-four loads
    /// were still outstanding and another sixty-four had already failed — a
    /// number that said "nothing left to do" beside a window that never opened.
    pub priming_pending: Gauge,
    /// Coarse tiles the session has given up on: the source has none, the load
    /// failed, or the content was topology rather than geometry. Expected to be
    /// non-zero on a globe — a geographic grid covers ocean and poles that no
    /// terrain source serves — and the reason a host must not wait for every
    /// primed tile to reach the GPU.
    pub priming_unavailable: Gauge,
    /// One once the coarse pyramid has been asked for and there is nothing left
    /// to ask for. Not "arrived" — that is the consumer's own count of what it
    /// holds.
    pub priming_done: Gauge,

    // --- Content store ---
    //
    // Whether the disk cache is doing anything is not a question anyone could
    // answer from the outside: a store that serves nothing and a store that is
    // never asked look identical from the network, and both read as "the cache
    // exists". Measured, one session held 2.6 GiB on disk while re-fetching at
    // 3 MB/s, which is the shape of a cache that is written and never read.
    /// Content the store answered without touching the network.
    pub store_hits: Counter,
    /// Content the store did not have, so the origin was asked.
    pub store_misses: Counter,
    /// Bytes the store served — what the network did *not* have to carry.
    pub store_bytes_served: Counter,
    /// Bytes fetched from the origin and written to the store.
    pub store_bytes_fetched: Counter,

    // --- Residency ---
    pub tiles_evicted: Counter,
    pub resident_bytes: Gauge,
    pub imagery_bytes: Gauge,
    pub imagery_textures: Gauge,

    // --- Consumer ---
    pub frames: Counter,
    /// Wall time from the top of one frame to the top of the next.
    ///
    /// The mean is the frame rate and says almost nothing; the *peak* is the
    /// stutter. Under vsync a frame that misses its deadline waits a whole
    /// refresh, so the number does not drift — it doubles, and the eye reads
    /// that doubling directly.
    pub frame_seconds: Timer,
    /// The longest frame since the last report, in milliseconds.
    pub worst_frame_millis: Peak,
    /// How far the camera moved in the single worst frame since the last
    /// report, as a fraction of the distance it *should* have moved at a
    /// steady rate.
    ///
    /// One is perfect pacing. Two means a frame moved the eye twice as far as
    /// its neighbours did, which is what a jerk is — and it is the only number
    /// here that measures what is actually complained about, rather than a
    /// cause someone guessed at.
    pub worst_camera_step: Peak,
    /// Rewriting every resident tile's model matrix when the render origin
    /// moves — work that exists only while the camera is moving, and grows with
    /// how many tiles the session has accumulated.
    pub rebase_seconds: Timer,
    /// Waiting for the compositor to hand back a drawable.
    ///
    /// Separated from the rest of the frame because it is the one wait that is
    /// *supposed* to happen: under vsync this is where a frame with time to
    /// spare parks. A small figure here with a large frame time means the work
    /// is the problem; a large figure here means the work already fits.
    pub present_seconds: Timer,
    pub uploads: Counter,
    pub upload_seconds: Timer,
    /// Stitching (`crate::stitch`), on a consumer that plans when what it
    /// draws changes: plans worked out and the time they took, tiles given
    /// new strips, and tiles left unstitched because their plan had more
    /// knots than the consumer holds.
    pub stitch_plans: Counter,
    pub stitch_seconds: Timer,
    pub restitched: Counter,
    pub stitch_overflows: Counter,
    /// Decoded tiles waiting for a slot in the frame's upload budget. A queue
    /// that keeps growing is content arriving later than it was asked for, and
    /// it looks from the outside exactly like a slow network.
    pub pending_uploads: Gauge,
    pub prepared_tiles: Gauge,
    /// Selected tiles currently drawn by a stand-in rather than their own
    /// geometry.
    ///
    /// A session where this stays high is one whose real tiles are not
    /// arriving — and it looks plausible throughout, because a stand-in is a
    /// surface at roughly the right height. This count is the only way to
    /// notice.
    pub tiles_filled: Gauge,

    // --- The frame, as drawn ---
    //
    // What the consumer's last resolve made of the selection. The server's
    // `tiles_selected` says what was asked for; these say what the frame could
    // draw of it, which is the half a person sees. `frame_lost` is the one that
    // matters most: ground the traversal asked for with nothing on its
    // ancestor chain to stand in — held up only by the backstop shell, and
    // black where there is none.
    /// Selected tiles the last frame drew with their own surface.
    pub frame_exact: Gauge,
    /// Selected tiles the last frame drew with a coarser ancestor standing in:
    /// soft ground, still covered.
    pub frame_coarser: Gauge,
    /// Selected tiles the last frame drew with nothing at all: a hole.
    pub frame_lost: Gauge,
    /// Selected tiles the last frame drew with a stand-in rather than their
    /// own content — counted as exact by every other number here.
    pub frame_stand_ins: Gauge,
    /// A digest of what the last frame drew: the tiles drawn exactly and the
    /// ancestors drawn in fallback, each set sorted
    /// ([`crate::determinism::digest`]). Constant while the picture is; two
    /// runs that draw the same ground read the same number.
    pub frame_digest: Gauge,
    /// Per eye of the last frame read back, the pixels whose ray meets the
    /// globe, and how many of those came out the clear colour. Set by whoever
    /// reads a frame back — a headless run, a traced session — and left alone
    /// by a session that never does.
    pub eye_ground_pixels: ByEye,
    pub eye_black_pixels: ByEye,

    // --- Bake ---
    //
    // A bake is a loop over frames on one thread: ask for a pose, resolve the
    // frame, write it. The log line it prints per frame timed only the middle
    // and the end, and read a millisecond beside frames thirty apart — the rest
    // was nowhere. These are the whole loop, piece by piece, so the missing
    // time has a name.
    /// One frame resolved and written to the pack, from its pose being known
    /// to its last tile being pushed. The pose is not in it: that is
    /// [`Metrics::pose_seconds`].
    pub bake_frame_seconds: Timer,
    /// Deciding one frame's pose. For a tape it is a lookup; for a source that
    /// places the camera against the ground, it is convergences of its own,
    /// the ground built and queried — on the bake's thread, before the frame.
    pub pose_seconds: Timer,
    /// Finishing one tile as it arrives — composed and encoded, or only named
    /// for a pack by reference — on the finish pool.
    pub finish_seconds: Timer,
    /// Building the ground a frame's selection draws, as heights a camera is
    /// kept over.
    pub ground_seconds: Timer,
    /// Tiles cut into cells for a ground to read heights off: once a tile,
    /// and again each time its stitching moves its vertices. What building a
    /// ground costs beyond a pointer a tile.
    pub ground_tiles: Counter,
    /// Height queries against a frame's ground, counted by samples standing
    /// for many (`tuile_film::ground::SAMPLED`).
    pub height_queries: Counter,
    /// Frames a bake's scouts converged ahead of it, to fetch their tiles
    /// into the store before the bake reads them (`tuile_bake::scout`).
    pub scout_frames: Counter,
    /// Of those, frames a scout gave up: their tiles the bake fetches itself.
    pub scout_frames_given_up: Counter,
    /// What a height query costs, timed by the same samples.
    pub height_query_seconds: Timer,

    // --- Per level ---
    //
    // Where the totals above are actually going. A session that slows down as
    // it runs looks the same in aggregate whether it is holding a thousand
    // coarse tiles or twenty thousand deep ones; these tell them apart.
    /// Tile meshes resident on the GPU, by quadtree level.
    pub meshes_by_level: ByLevel,
    /// Their memory, by quadtree level.
    pub mesh_bytes_by_level: ByLevel,
    /// Distinct imagery textures held, by **imagery** level — which is not the
    /// terrain level, and the gap between the two is the whole question of
    /// whether the ground is as sharp as the source allows.
    pub textures_by_level: ByLevel,
    /// Their memory, by imagery level, counted once per texture however many
    /// tiles drape it.
    pub texture_bytes_by_level: ByLevel,
    /// Tiles waiting to be fetched, by level. A queue that fills at one level
    /// while the others drain is a refinement that has run away.
    pub queued_by_level: ByLevel,
    /// Loads begun, by level.
    pub loads_by_level: ByLevel,
    /// Tiles built from an ancestor's surface, by level.
    pub upsampled_by_level: ByLevel,
    /// What the last pass chose to draw, by level.
    pub selected_by_level: ByLevel,
}

impl Metrics {
    /// The per-level breakdown, compact enough for a log line.
    pub fn by_level_line(&self) -> String {
        let show = |name: &str, series: &ByLevel| {
            let rows: Vec<String> = series
                .occupied()
                .into_iter()
                .map(|(l, v)| format!("{l}:{v}"))
                .collect();
            format!("{name} {{{}}}", rows.join(" "))
        };
        format!(
            "{} | {} | {} | {} | {}",
            show("meshes", &self.meshes_by_level),
            show("mesh MiB", &self.mesh_bytes_by_level),
            show("textures", &self.textures_by_level),
            show("queued", &self.queued_by_level),
            show("selected", &self.selected_by_level),
        )
    }

    /// The handful of numbers worth putting on one line every second.
    ///
    /// Chosen for what they *rule out* rather than for completeness: a
    /// cancellation count that tracks the start count says loads are being
    /// killed; an upload queue that only grows says content is arriving late; a
    /// traversal peak in the tens of milliseconds says the stutter is here and
    /// not in the renderer.
    pub fn one_line(&self) -> String {
        format!(
            "traversal {:.1}/{:.1} ms (mean/peak, {} passes) | arrivals {:.1}/{} per pass, \
             waited {:.1}/{:.1} ms | loads {} started {} done {} failed \
             {} cancelled, {:.0}/{:.0} ms, {} in flight | in flight {} mesh ({:.0} ms), {} texture \
             ({:.0} ms) | {} upsampled | uploads {} done, {} queued, {:.1}/{:.1} ms | {} evicted",
            self.traversal_seconds.mean_millis(),
            self.traversal_seconds.peak_millis(),
            self.traversals.get(),
            self.arrivals_per_pass.mean(),
            self.arrivals_per_pass.peak(),
            self.arrival_wait_seconds.mean_millis(),
            self.arrival_wait_seconds.peak_millis(),
            self.loads_started.get(),
            self.loads_completed.get(),
            self.loads_failed.get(),
            self.loads_cancelled.get(),
            self.load_seconds.mean_millis(),
            self.load_seconds.peak_millis(),
            self.loads_in_flight.get(),
            self.meshes_in_flight.get(),
            self.mesh_fetch_seconds.mean_millis(),
            self.textures_in_flight.get(),
            self.texture_fetch_seconds.mean_millis(),
            self.tiles_upsampled.get(),
            self.uploads.get(),
            self.pending_uploads.get(),
            self.upload_seconds.mean_millis(),
            self.upload_seconds.peak_millis(),
            self.tiles_evicted.get(),
        )
    }
}

/// The buckets of `tuile_tiles_fetch_seconds`, the histogram of what reading
/// a tile costs by where it was found (recorded by the native tile store and
/// the native HTTP transport, through the `metrics` facade): half a
/// millisecond to half a minute, doubling — a projection's read is a
/// fraction of a millisecond, a cold zone's first range read from a bucket
/// seconds. A recorder sets them; the facade cannot.
pub const TILE_FETCH_SECONDS_BUCKETS: [f64; 17] = [
    0.0005, 0.001, 0.002, 0.004, 0.008, 0.016, 0.032, 0.064, 0.128, 0.256, 0.512, 1.024, 2.048,
    4.096, 8.192, 16.384, 32.768,
];

/// How many eyes a per-eye series covers: a stereo pair, with room for a
/// rig of four.
pub const EYES: usize = 4;

/// One series broken down by eye — the views a frame is drawn for, one for a
/// window and two for a headset.
///
/// An eye past the end is folded into the last one, as [`ByLevel`] folds a
/// level: a miscounted eye is a bug, a silently missing one is a mystery.
#[derive(Debug)]
pub struct ByEye([AtomicU64; EYES]);

impl Default for ByEye {
    fn default() -> Self {
        Self([const { AtomicU64::new(0) }; EYES])
    }
}

impl ByEye {
    fn slot(eye: usize) -> usize {
        eye.min(EYES - 1)
    }

    pub fn set(&self, eye: usize, v: u64) {
        self.0[Self::slot(eye)].store(v, ORDER);
    }
    pub fn get(&self, eye: usize) -> u64 {
        self.0[Self::slot(eye)].load(ORDER)
    }
    pub fn clear(&self) {
        for slot in &self.0 {
            slot.store(0, ORDER);
        }
    }
    fn all(&self) -> [u64; EYES] {
        std::array::from_fn(|eye| self.get(eye))
    }
}

/// Declares [`Snapshot`] once, from the two lists of fields it copies, so the
/// struct, the copy and the difference cannot disagree about which field is a
/// counter.
macro_rules! snapshot {
    (counters: [$($c:ident),* $(,)?], gauges: [$($g:ident),* $(,)?] $(,)?) => {
        /// The engine's numbers at one instant, as plain values: every counter
        /// and gauge of [`Metrics`] that counts or weighs something, under the
        /// name it is published by (`tuile_<name>`), the per-level series as
        /// their totals and the per-eye series as arrays.
        ///
        /// No timer is here, deliberately: a duration is the one thing two
        /// runs of the same script never agree on, and a snapshot is made to
        /// be compared. Durations stay behind the exporter.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
        pub struct Snapshot {
            $(pub $c: u64,)*
            $(pub $g: u64,)*
            /// Mesh bytes on the GPU, all levels.
            pub mesh_bytes: u64,
            /// Imagery bytes on the GPU, all levels, each texture once.
            pub texture_bytes: u64,
            /// Meshes on the GPU, all levels.
            pub meshes: u64,
            /// Imagery textures on the GPU, all levels.
            pub textures: u64,
            pub eye_ground_pixels: [u64; EYES],
            pub eye_black_pixels: [u64; EYES],
        }

        impl Metrics {
            /// Every number a [`Snapshot`] holds, read now.
            pub fn snapshot(&self) -> Snapshot {
                Snapshot {
                    $($c: self.$c.get(),)*
                    $($g: self.$g.get(),)*
                    mesh_bytes: self.mesh_bytes_by_level.total(),
                    texture_bytes: self.texture_bytes_by_level.total(),
                    meshes: self.meshes_by_level.total(),
                    textures: self.textures_by_level.total(),
                    eye_ground_pixels: self.eye_ground_pixels.all(),
                    eye_black_pixels: self.eye_black_pixels.all(),
                }
            }
        }

        impl Snapshot {
            /// What happened since `earlier`: each counter as the difference,
            /// each gauge as it is now. Two sessions run one after the other
            /// in one process then read alike — the counters are the
            /// process's, and only their difference is the session's.
            #[must_use]
            pub fn since(&self, earlier: &Snapshot) -> Snapshot {
                Snapshot {
                    $($c: self.$c.saturating_sub(earlier.$c),)*
                    ..*self
                }
            }
        }
    };
}

snapshot! {
    counters: [
        traversals,
        loads_started,
        loads_completed,
        loads_failed,
        loads_cancelled,
        mesh_fetches,
        texture_fetches,
        tiles_upsampled,
        tiles_without_imagery,
        drapes_withheld,
        imagery_absent,
        terrain_absent,
        black_textures,
        store_hits,
        store_misses,
        store_bytes_served,
        store_bytes_fetched,
        tiles_evicted,
        frames,
        uploads,
    ],
    gauges: [
        tiles_visited,
        tiles_culled,
        tiles_selected,
        gaps,
        loads_in_flight,
        meshes_in_flight,
        textures_in_flight,
        priming_total,
        priming_pending,
        priming_unavailable,
        priming_done,
        resident_bytes,
        imagery_bytes,
        imagery_textures,
        pending_uploads,
        prepared_tiles,
        tiles_filled,
        frame_exact,
        frame_coarser,
        frame_lost,
        frame_stand_ins,
        frame_digest,
    ],
}

/// The process's metrics.
pub fn metrics() -> &'static Metrics {
    static METRICS: std::sync::OnceLock<Metrics> = std::sync::OnceLock::new();
    METRICS.get_or_init(Metrics::default)
}

/// A monotonic stamp, on every target this crate compiles for.
///
/// `Instant::now()` from the standard library **panics** on
/// `wasm32-unknown-unknown` — there is no clock there, and the panic reaches a
/// browser as a bare `RuntimeError: unreachable` with no message at all. Six
/// call sites on the hot path used it directly, so the first tile a browser
/// loaded killed the engine.
///
/// A no-op on wasm rather than a real clock, deliberately. A real one means
/// `performance.now()`, which means `web-sys`, which is a backend dependency
/// this crate may not take (rule 1). What is lost is the *duration* histograms
/// in the browser; every counter and gauge still works, because none of them
/// asks what time it is. When a browser needs those histograms the answer is a
/// clock seam injected by the host, like `offload::Offload` — not a dependency
/// smuggled in here.
#[cfg(not(target_arch = "wasm32"))]
pub type Stamp = std::time::Instant;

#[cfg(not(target_arch = "wasm32"))]
pub fn stamp() -> Stamp {
    std::time::Instant::now()
}

/// The wasm stand-in: it knows nothing and says so — every span is zero, and a
/// zero-length span reads as "not measured" rather than as a plausible lie.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stamp;

#[cfg(target_arch = "wasm32")]
impl Stamp {
    pub fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

#[cfg(target_arch = "wasm32")]
pub fn stamp() -> Stamp {
    Stamp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timer_reports_a_mean_and_a_peak() {
        let t = Timer::default();
        assert_eq!(t.count(), 0);
        assert_eq!(t.mean_millis(), 0.0, "no samples is not a divide by zero");

        t.record(std::time::Duration::from_millis(10));
        t.record(std::time::Duration::from_millis(30));
        assert_eq!(t.count(), 2);
        assert!((t.mean_millis() - 20.0).abs() < 0.001);
        assert!((t.peak_millis() - 30.0).abs() < 0.001);

        // The peak is a high-water mark, not the last sample — a stutter that
        // happened once still has to be visible a minute later.
        t.record(std::time::Duration::from_millis(1));
        assert!((t.peak_millis() - 30.0).abs() < 0.001);
    }

    /// A sample standing for many weighs as many in the total and the
    /// count — the mean is the samples' — and as one in the peak.
    #[test]
    fn a_sample_stands_for_its_weight() {
        let t = Timer::default();
        t.record_sample(std::time::Duration::from_micros(2), 61);
        t.record_sample(std::time::Duration::from_micros(4), 61);
        assert_eq!(t.count(), 122);
        assert!((t.total_seconds() - 366.0e-6).abs() < 1.0e-12);
        assert!((t.mean_millis() - 0.003).abs() < 1.0e-9);
        assert!((t.peak_millis() - 0.004).abs() < 1.0e-9);
    }

    #[test]
    fn a_tally_reports_a_mean_and_a_peak() {
        let t = Tally::default();
        assert_eq!(t.mean(), 0.0, "no samples is not a divide by zero");
        t.record(1);
        t.record(5);
        t.record(0);
        assert_eq!((t.count(), t.sum(), t.peak()), (3, 6, 5));
        assert!((t.mean() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_timer_adds_up_to_its_total() {
        let t = Timer::default();
        t.record(std::time::Duration::from_millis(250));
        t.record(std::time::Duration::from_millis(750));
        assert!((t.total_seconds() - 1.0).abs() < 1e-9);
    }

    /// A snapshot reads every series it names, the per-level ones as totals
    /// and the per-eye ones eye by eye; and what a session did is the
    /// counters' difference, with every gauge as it now reads.
    #[test]
    fn a_snapshot_reads_the_numbers_and_a_difference_keeps_the_gauges() {
        let m = Metrics::default();
        m.uploads.add(5);
        m.frame_lost.set(3);
        m.mesh_bytes_by_level.add(2, 100);
        m.mesh_bytes_by_level.add(7, 20);
        m.eye_black_pixels.set(1, 9);
        let before = m.snapshot();
        assert_eq!(before.uploads, 5);
        assert_eq!(before.frame_lost, 3);
        assert_eq!(before.mesh_bytes, 120);
        assert_eq!(before.eye_black_pixels, [0, 9, 0, 0]);

        m.uploads.add(2);
        m.frame_lost.set(1);
        let after = m.snapshot().since(&before);
        assert_eq!(after.uploads, 2, "a counter is what happened since");
        assert_eq!(after.frame_lost, 1, "a gauge is what it reads now");
        assert_eq!(after.mesh_bytes, 120);
        assert_eq!(after.eye_black_pixels, [0, 9, 0, 0]);
    }

    /// An eye past the rig's end is counted on the last one, not dropped.
    #[test]
    fn an_eye_past_the_end_folds_into_the_last() {
        let eyes = ByEye::default();
        eyes.set(EYES + 3, 4);
        assert_eq!(eyes.get(EYES - 1), 4);
        eyes.clear();
        assert_eq!(eyes.get(EYES - 1), 0);
    }

    #[test]
    fn the_process_metrics_are_one_instance() {
        let before = metrics().frames.get();
        metrics().frames.inc();
        assert_eq!(metrics().frames.get(), before + 1);
    }
}
