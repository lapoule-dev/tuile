// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a render service says of a segment: its answer to the caller, and
//! the receipt it keeps beside the segment in the run store.
//!
//! Both sides read these types — the service that writes them and whoever
//! asked for the segment — so they live here, in the farm's client, and not
//! in the renderer, which pulls a GPU stack a caller has no use for.
//!
//! # Versions, in both directions
//!
//! A receipt is read by callers older and newer than the service that wrote
//! it, since the two are deployed one after the other. So:
//!
//! - every field added after the first receipt is `#[serde(default)]`: a
//!   newer reader takes an older receipt, the missing parts at zero;
//! - no type here refuses a field it does not know: an older reader takes a
//!   newer receipt, the new parts ignored;
//! - [`SegmentDone::version`] says which receipt it is — absent, and so 0,
//!   on every receipt written before [`RECEIPT_VERSION`] 2.
//!
//! # Where a segment's time goes
//!
//! [`SegmentDone::phases`] splits the service's time into parts that do not
//! overlap and add up to [`SegmentDone::service_seconds`]: the caller's
//! wall time less that is what happened outside the service — waiting for an
//! instance, starting one, the network, putting the receipt itself.

use serde::{Deserialize, Serialize};

/// The receipt written now. 0 (absent) is the first, which said only the
/// wait, the render and its steps.
pub const RECEIPT_VERSION: u32 = 2;

/// What a segment came to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SegmentDone {
    /// [`RECEIPT_VERSION`] when written; 0 for a receipt from before it.
    #[serde(default)]
    pub version: u32,
    pub out: String,
    pub frames: u64,
    pub bytes: u64,
    /// From the order read to the card: its overlays made, its pack waited
    /// for, its film opened, its extracts fetched, the card's turn — each
    /// apart in [`Self::stages`].
    pub waited_seconds: f64,
    /// The render on its own thread, the encoder drained.
    pub render_seconds: f64,
    /// The extracts fetched before the first frame, if the store is a
    /// server's.
    #[serde(default)]
    pub extract: Option<Fetched>,
    #[serde(default)]
    pub render: RenderTimes,
    /// When the request arrived, by the service's clock (Unix seconds).
    #[serde(default)]
    pub received_at_unix: f64,
    /// From the request's arrival to the receipt written: everything but
    /// the receipt's own put.
    #[serde(default)]
    pub service_seconds: f64,
    #[serde(default)]
    pub stages: Stages,
    #[serde(default)]
    pub instance: Instance,
    #[serde(default)]
    pub io: Io,
    /// The segment's CPU profile, put beside it; none where the service
    /// could not take one.
    #[serde(default)]
    pub profile: Option<Profile>,
}

/// The parts of a segment around its render, in seconds, in the order they
/// happen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stages {
    /// Between the request's arrival, as the host's outermost layer saw
    /// it, and the service reading the order: the host's own layers (a
    /// host may complete the order first) and the body read. 0 where the
    /// host marks no arrival.
    #[serde(default)]
    pub before_seconds: f64,
    /// The host's overlays made from the order.
    #[serde(default)]
    pub overlays_seconds: f64,
    /// The generation of caches the segment renders in: kept, or opened.
    #[serde(default)]
    pub generation_seconds: f64,
    /// Waiting for the pack to be in the store.
    #[serde(default)]
    pub pack_wait_seconds: f64,
    /// The film opened from its pack, and its sun placed.
    #[serde(default)]
    pub film_seconds: f64,
    /// The extracts fetched and the store pointed at them: the transfer
    /// alone is [`Fetched::seconds`].
    #[serde(default)]
    pub fetch_seconds: f64,
    /// Waiting for the card, behind the renders already on it.
    #[serde(default)]
    pub card_wait_seconds: f64,
    /// Putting the segment in the store.
    #[serde(default)]
    pub put_seconds: f64,
}

/// The instance that served a segment, as far as its own process can tell:
/// the start of its container, and any wait for one, are seen only from
/// outside.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    /// When the service began opening, by its clock (Unix seconds).
    #[serde(default)]
    pub started_at_unix: f64,
    /// Opening the service: its stores, its tile store's blocks.
    #[serde(default)]
    pub opened_seconds: f64,
    /// From the service beginning to open to this segment's arrival.
    #[serde(default)]
    pub uptime_seconds: f64,
    /// Segments the instance began before this one.
    #[serde(default)]
    pub segments_before: u64,
    /// The first segment the instance serves: the one its start was for.
    #[serde(default)]
    pub cold: bool,
    /// The generation of caches was the one kept from a segment before,
    /// and how old it was; a new one reads every source anew.
    #[serde(default)]
    pub generation_reused: bool,
    #[serde(default)]
    pub generation_age_seconds: f64,
    /// The service's revision, where the platform names it.
    #[serde(default)]
    pub revision: String,
}

/// Bytes a segment moved, apart from its extracts ([`Fetched::bytes`]) and
/// the segment itself ([`SegmentDone::bytes`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Io {
    /// Of its packs: asked by the render, and of those fetched from the
    /// bucket — the rest came from the instance's cache.
    #[serde(default)]
    pub pack_asked_bytes: u64,
    #[serde(default)]
    pub pack_fetched_bytes: u64,
    /// The same of the tile store's archives.
    #[serde(default)]
    pub archive_asked_bytes: u64,
    #[serde(default)]
    pub archive_fetched_bytes: u64,
    /// The store's catalog, manifests and tables, as the bucket answered.
    #[serde(default)]
    pub live_bytes: u64,
    /// Put in the run store: the segment and its profile, the receipt
    /// aside.
    #[serde(default)]
    pub put_bytes: u64,
}

/// A segment's CPU profile, beside it in the run store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Its key: a gzipped pprof.
    pub key: String,
    #[serde(default)]
    pub bytes: u64,
    /// Samples a second it was taken at.
    #[serde(default)]
    pub hz: u32,
    /// Making it — its stacks named — and putting it.
    #[serde(default)]
    pub seconds: f64,
}

/// Where a render's time went, as it measured it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RenderTimes {
    /// Opening the adapter and the sink.
    pub setup_seconds: f64,
    /// The first frame, which enters every tile it draws…
    pub opening: Timings,
    /// …and every frame after it, together.
    pub frames: Timings,
    pub tiles_from_store: u64,
    pub tiles_from_pack: u64,
    /// The render's own wall time, from its first step to its sink closed.
    #[serde(default)]
    pub seconds: f64,
    /// Every store tile of the segment asked for once, before its first
    /// frame.
    #[serde(default)]
    pub ahead: Ahead,
    /// The encoder's thread: its seconds, beside the render's and not
    /// within them, and what it was.
    #[serde(default)]
    pub encode_seconds: f64,
    #[serde(default)]
    pub encoder: String,
    /// The card's own clock on each kind of pass.
    #[serde(default)]
    pub gpu: GpuTimes,
    /// The adapter the render ran on, as its driver names it.
    #[serde(default)]
    pub adapter: String,
}

/// The read ahead: packs it ran for, store tiles asked, seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Ahead {
    #[serde(default)]
    pub packs: u64,
    #[serde(default)]
    pub tiles: u64,
    #[serde(default)]
    pub seconds: f64,
}

/// GPU time by kind of pass, from timestamp queries: none where the adapter
/// has none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuTimes {
    /// Whether the adapter writes timestamps at all.
    #[serde(default)]
    pub timestamps: bool,
    #[serde(default)]
    pub passes: Vec<GpuPass>,
}

impl GpuTimes {
    /// Every pass together.
    pub fn seconds(&self) -> f64 {
        self.passes.iter().map(|p| p.seconds).sum()
    }
}

/// One kind of pass: its seconds over the segment, and how many there were.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuPass {
    pub pass: String,
    pub seconds: f64,
    pub count: u64,
}

/// Where time went, in milliseconds, step by step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Reading terrain tiles from the store (cache or network).
    pub read_terrain: f64,
    /// Reading imagery tiles from the store.
    pub read_imagery: f64,
    /// Reading payloads from the pack.
    pub read_pack: f64,
    /// Terrain tiles made into meshes.
    pub mesh: f64,
    /// Imagery decoded and laid on the terrain's spacing.
    pub decode: f64,
    /// Textures written to the GPU.
    pub upload: f64,
    /// Drapes queued for composition, meshes entered.
    pub enter: f64,
    /// The frame's tiles stitched: the plan, and what it has the GPU hold
    /// anew.
    pub stitch: f64,
    /// The frame recorded and submitted.
    pub draw: f64,
    /// Waiting for the GPU and reading the picture back.
    pub readback: f64,
    /// Tiles let go.
    pub leave: f64,
    /// The observers.
    pub observe: f64,
    /// Handing the picture to the sink (for an encoder on its own thread:
    /// the wait for room, not the encoding).
    pub sink: f64,
    /// The frame's place in the pack: what enters, what leaves, its view.
    #[serde(default)]
    pub advance: f64,
    /// The host's overlays and light for the frame, and the sun's map they
    /// are made for.
    #[serde(default)]
    pub overlays: f64,
}

impl Timings {
    /// Every step, named, in the order a frame goes through them.
    pub fn steps(&self) -> [(&'static str, f64); 15] {
        [
            ("advance", self.advance),
            ("read terrain", self.read_terrain),
            ("read imagery", self.read_imagery),
            ("read pack", self.read_pack),
            ("build meshes", self.mesh),
            ("decode imagery", self.decode),
            ("upload textures", self.upload),
            ("enter tiles", self.enter),
            ("stitch tiles", self.stitch),
            ("overlays", self.overlays),
            ("record frame", self.draw),
            ("GPU + readback", self.readback),
            ("leave tiles", self.leave),
            ("observers", self.observe),
            ("sink", self.sink),
        ]
    }

    /// Every step together, in milliseconds.
    pub fn total(&self) -> f64 {
        self.steps().iter().map(|s| s.1).sum()
    }

    pub fn add(&mut self, other: &Timings) {
        self.read_terrain += other.read_terrain;
        self.read_imagery += other.read_imagery;
        self.read_pack += other.read_pack;
        self.mesh += other.mesh;
        self.decode += other.decode;
        self.upload += other.upload;
        self.enter += other.enter;
        self.stitch += other.stitch;
        self.draw += other.draw;
        self.readback += other.readback;
        self.leave += other.leave;
        self.observe += other.observe;
        self.sink += other.sink;
        self.advance += other.advance;
        self.overlays += other.overlays;
    }
}

/// What fetching a render's extracts took.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fetched {
    pub extracts: u64,
    pub parts: u64,
    pub bytes: u64,
    pub seconds: f64,
}

impl Fetched {
    /// Megabytes a second, over the whole fetch.
    pub fn mb_per_second(&self) -> f64 {
        if self.seconds > 0.0 {
            self.bytes as f64 / 1e6 / self.seconds
        } else {
            0.0
        }
    }
}

impl SegmentDone {
    /// The service's time in parts that do not overlap, in the order they
    /// happen, in seconds; the last, `between`, is what none of the others
    /// holds. For a receipt from before [`Self::service_seconds`], the wait
    /// and the render's untimed part stand whole.
    pub fn phases(&self) -> Vec<(&'static str, f64)> {
        let r = &self.render;
        let steps = (r.opening.total() + r.frames.total()) / 1000.0;
        let in_render = r.setup_seconds + r.ahead.seconds + steps;
        let mut parts = vec![("before", self.stages.before_seconds)];
        if self.version == 0 {
            parts.push(("waited", self.waited_seconds));
        } else {
            let s = &self.stages;
            parts.extend([
                ("overlays", s.overlays_seconds),
                ("generation", s.generation_seconds),
                ("pack wait", s.pack_wait_seconds),
                ("film", s.film_seconds),
                ("fetch", s.fetch_seconds),
                ("card wait", s.card_wait_seconds),
            ]);
        }
        parts.extend([
            ("setup", r.setup_seconds),
            ("ahead", r.ahead.seconds),
            ("opening", r.opening.total() / 1000.0),
            ("frames", r.frames.total() / 1000.0),
            ("render untimed", (self.render_seconds - in_render).max(0.0)),
            ("put", self.stages.put_seconds),
            (
                "profile",
                self.profile.as_ref().map_or(0.0, |p| p.seconds),
            ),
        ]);
        let named: f64 = parts.iter().map(|p| p.1).sum();
        let whole = if self.service_seconds > 0.0 {
            self.service_seconds
        } else {
            named
        };
        parts.push(("between", (whole - named).max(0.0)));
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receipt as the first service wrote it (a segment of 416 frames of
    /// 1080p, on an L4, 19 October 2026), its key made anonymous.
    const FIRST: &str = r#"{
      "out": "films/a/seg0.mp4", "frames": 416, "bytes": 23333100,
      "waited_seconds": 7.215683264, "render_seconds": 54.48853391,
      "extract": {"extracts": 1, "parts": 3, "bytes": 77322590, "seconds": 5.940985733},
      "render": {
        "setup_seconds": 1.455108993,
        "opening": {"read_terrain": 0.904741, "read_imagery": 4.092483, "read_pack": 0.0,
          "mesh": 1.021665, "decode": 1821.830104000001, "upload": 18.067119, "enter": 7.543164,
          "stitch": 2.197417, "draw": 9461.057773, "readback": 37.924923, "leave": 0.007992,
          "observe": 0.034446000000000025, "sink": 0.8330259999999999},
        "frames": {"read_terrain": 71.529683, "read_imagery": 109.57864199999999, "read_pack": 0.0,
          "mesh": 3535.3219480000007, "decode": 12620.469874999993, "upload": 255.20569199999997,
          "enter": 602.8174199999999, "stitch": 1914.8414049999978, "draw": 10216.008580999993,
          "readback": 7039.1224889999985, "leave": 144.147554, "observe": 1.3735159999999997,
          "sink": 504.5009200000002},
        "tiles_from_store": 4143, "tiles_from_pack": 0
      }
    }"#;

    /// What a caller of the first service read of an answer: no more, and
    /// nothing refused.
    #[derive(Deserialize)]
    struct FirstReader {
        frames: u64,
        waited_seconds: f64,
        render_seconds: f64,
        render: FirstRender,
    }

    #[derive(Deserialize)]
    struct FirstRender {
        setup_seconds: f64,
        frames: FirstTimings,
    }

    #[derive(Deserialize)]
    struct FirstTimings {
        draw: f64,
    }

    fn a_new_receipt() -> SegmentDone {
        SegmentDone {
            version: RECEIPT_VERSION,
            out: "films/a/seg0.mp4".into(),
            frames: 450,
            bytes: 1 << 20,
            waited_seconds: 12.0,
            render_seconds: 60.0,
            extract: Some(Fetched {
                extracts: 1,
                parts: 3,
                bytes: 70 << 20,
                seconds: 5.0,
            }),
            render: RenderTimes {
                setup_seconds: 1.0,
                opening: Timings {
                    draw: 2000.0,
                    ..Timings::default()
                },
                frames: Timings {
                    draw: 30_000.0,
                    advance: 500.0,
                    overlays: 1500.0,
                    ..Timings::default()
                },
                seconds: 59.0,
                ahead: Ahead {
                    packs: 1,
                    tiles: 4000,
                    seconds: 20.0,
                },
                encode_seconds: 40.0,
                encoder: "encoding Av1 by NVENC, on its own thread".into(),
                gpu: GpuTimes {
                    timestamps: true,
                    passes: vec![GpuPass {
                        pass: "visibility".into(),
                        seconds: 3.0,
                        count: 450,
                    }],
                },
                adapter: "NVIDIA L4 (Vulkan)".into(),
                ..RenderTimes::default()
            },
            received_at_unix: 1_792_000_000.0,
            service_seconds: 80.0,
            stages: Stages {
                before_seconds: 0.5,
                overlays_seconds: 1.0,
                generation_seconds: 0.1,
                pack_wait_seconds: 2.0,
                film_seconds: 0.4,
                fetch_seconds: 6.0,
                card_wait_seconds: 2.5,
                put_seconds: 3.0,
            },
            instance: Instance {
                started_at_unix: 1_791_999_990.0,
                opened_seconds: 2.0,
                uptime_seconds: 10.0,
                segments_before: 0,
                cold: true,
                generation_reused: false,
                generation_age_seconds: 0.0,
                revision: "tuile-render-00042-abc".into(),
            },
            io: Io {
                pack_asked_bytes: 9,
                pack_fetched_bytes: 8,
                archive_asked_bytes: 7,
                archive_fetched_bytes: 6,
                live_bytes: 5,
                put_bytes: 4,
            },
            profile: Some(Profile {
                key: "films/a/seg0.pprof".into(),
                bytes: 200_000,
                hz: 100,
                seconds: 1.5,
            }),
        }
    }

    #[test]
    fn a_first_receipt_reads_with_its_new_parts_at_zero() {
        let first: SegmentDone = serde_json::from_str(FIRST).expect("a first receipt");
        assert_eq!(first.version, 0);
        assert_eq!(first.frames, 416);
        assert_eq!(first.render.tiles_from_store, 4143);
        assert_eq!(first.render.frames.draw, 10216.008580999993);
        // What it did not say is zero, not refused.
        assert_eq!(first.stages, Stages::default());
        assert_eq!(first.instance, Instance::default());
        assert_eq!(first.render.ahead, Ahead::default());
        assert_eq!(first.render.frames.overlays, 0.0);
        assert!(first.profile.is_none());
    }

    #[test]
    fn a_new_receipt_reads_where_the_first_was_read_and_comes_back_whole() {
        let new = a_new_receipt();
        let json = serde_json::to_string(&new).expect("json");
        // A caller that knows only the first receipt reads this one.
        let old: FirstReader = serde_json::from_str(&json).expect("read as the first");
        assert_eq!(
            (old.frames, old.waited_seconds, old.render_seconds),
            (450, 12.0, 60.0)
        );
        assert_eq!(
            (old.render.setup_seconds, old.render.frames.draw),
            (1.0, 30_000.0)
        );
        // And this side reads all of it back.
        let again: SegmentDone = serde_json::from_str(&json).expect("again");
        assert_eq!(again, new);
    }

    #[test]
    fn the_phases_of_a_segment_do_not_overlap_and_add_up_to_its_time() {
        let new = a_new_receipt();
        let phases = new.phases();
        let sum: f64 = phases.iter().map(|p| p.1).sum();
        assert!((sum - new.service_seconds).abs() < 1e-9, "{phases:?}");
        let of = |name: &str| phases.iter().find(|p| p.0 == name).expect(name).1;
        assert_eq!(of("ahead"), 20.0);
        assert_eq!(of("opening"), 2.0);
        assert_eq!(of("frames"), 32.0);
        // The render's untimed part: 60 s less setup, ahead and steps.
        assert!((of("render untimed") - 5.0).abs() < 1e-9);
        assert_eq!(of("profile"), 1.5);
        // Between the parts: 80 s less every one of them.
        assert!((of("between") - 3.0).abs() < 1e-9, "{phases:?}");

        // A first receipt: its wait whole, the rest of its render untimed.
        let first: SegmentDone = serde_json::from_str(FIRST).expect("a first receipt");
        let phases = first.phases();
        let of = |name: &str| phases.iter().find(|p| p.0 == name).map(|p| p.1);
        assert_eq!(of("waited"), Some(7.215683264));
        assert_eq!(of("pack wait"), None);
        let sum: f64 = phases.iter().map(|p| p.1).sum();
        assert!((sum - (first.waited_seconds + first.render_seconds)).abs() < 1e-6);
    }

    #[test]
    fn a_frames_steps_are_fifteen_and_add_up() {
        let t = Timings {
            advance: 1.0,
            overlays: 2.0,
            draw: 3.0,
            ..Timings::default()
        };
        assert_eq!(t.total(), 6.0);
        let mut sum = Timings::default();
        sum.add(&t);
        sum.add(&t);
        assert_eq!((sum.advance, sum.overlays, sum.draw), (2.0, 4.0, 6.0));
        assert_eq!(t.steps()[0], ("advance", 1.0));
        assert_eq!(t.steps()[9], ("overlays", 2.0));
    }
}
