// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The guard of the O1 optimisation pass: today's `decide`, frozen
//! ([`frozen`]), driven step by step beside the real [`Budget`] on the same
//! inputs ([`Lockstep`]).
//!
//! Every input the real budget receives, the frozen one receives too, in the
//! same order; every step, the two `Decisions` must be equal — digest and
//! content, the statistics included except `work`, the cost counter the
//! optimisations exist to lower — and so must the ledgers, the audits, and
//! the residency and resource views of every tile the need names, and the
//! whole internal state (`tuile_budget::snapshot`: every tracked resource's
//! load, copies, generations, epochs, costs and eviction key, the tree
//! positions, the step-to-step sets, the epochs, the ledger, the in-flight
//! counts). The first divergence panics with the step and the fields that
//! differ.
//!
//! Wired into every `budget_sim` run (the `Sim` holds a [`Lockstep`]) and
//! into a proptest over random scripts (`budget_properties.rs`).

// Not every binary that includes the harness uses all of it.
#![allow(dead_code)]

pub mod digest;
pub mod frozen;

use tuile_budget::{
    cost::Cost, AuditError, Budget, BudgetConfig, Bytes, Class, ConfigError, Decisions, Failure,
    FrameInput, Generation, LayerId, Micros, NeedInput, NeedSet, Pressure, Released, ResourceId,
    Thermal,
};

/// The real budget and the frozen reference, in lockstep. Reads go to the
/// real one (`Deref`); every mutation goes to both, through the methods
/// below — there is no `DerefMut`, so a mutation the harness does not mirror
/// does not compile.
#[derive(Debug)]
pub struct Lockstep {
    /// The budget under test.
    pub real: Budget,
    /// Today's engine, frozen; `None` only for the bench
    /// ([`Lockstep::unchecked`]), which times the real budget alone.
    pub frozen: Option<frozen::Budget>,
    /// Steps compared so far.
    pub steps: u64,
    /// The frozen engine's per-step digests, recorded for the reference
    /// file (`recorded.txt`) under `TUILE_BLESS_REFERENCE=1`.
    record: Option<Recorder>,
}

/// A run's recorded digests: its key, its chains, its checkpoint lines.
#[derive(Debug)]
struct Recorder {
    key: String,
    chains: digest::Chains,
    lines: Vec<String>,
}

impl Recorder {
    fn new(key: String) -> Self {
        assert!(
            !key.contains('\t') && !key.contains('\n'),
            "a run's key is one field"
        );
        Recorder {
            key,
            chains: digest::Chains::default(),
            lines: Vec::new(),
        }
    }

    fn line(&mut self, at: &str) {
        let c = self.chains.0;
        self.lines.push(format!(
            "{}\t{at}\t{:016x} {:016x} {:016x} {:016x}",
            self.key, c[0], c[1], c[2], c[3]
        ));
    }
}

/// Where the recorded reference lives.
pub fn recorded_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference/recorded.txt")
}

/// Where each run writes its lines while the reference is recorded, before
/// they are assembled into [`recorded_path`] (`assemble`): a directory of
/// the system's temporary one, to empty before recording.
pub fn parts_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("tuile-budget-reference-parts")
}

/// Whether this run records the reference.
pub fn blessing() -> bool {
    std::env::var_os("TUILE_BLESS_REFERENCE").is_some()
}

/// The key of a run made on this thread: the test's name (libtest names a
/// test's thread after it), and the how-manyth run of the test it is.
pub fn thread_key(prefix: &str) -> String {
    thread_local! {
        static RUNS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    let name = std::thread::current()
        .name()
        .map(str::to_owned)
        .filter(|n| n != "main")
        .expect(
            "the recorded reference keys a run by its test's thread name: run the tests \
             with libtest's threads (not --test-threads=1)",
        );
    let n = RUNS.with(|r| {
        let n = r.get();
        r.set(n + 1);
        n
    });
    format!("{prefix}/{name}#{n}")
}

/// Joins every run's lines, by key, into the recorded reference.
pub fn assemble() -> usize {
    let mut runs: Vec<(String, String)> = std::fs::read_dir(parts_dir())
        .expect("recorded parts")
        .map(|e| {
            let text = std::fs::read_to_string(e.expect("a part").path()).expect("a part");
            let key = text.split('\t').next().unwrap_or_default().to_owned();
            (key, text)
        })
        .collect();
    runs.sort();
    let mut out = String::new();
    for (_, text) in &runs {
        out.push_str(text);
    }
    std::fs::write(recorded_path(), out).expect("the recorded reference written");
    runs.len()
}

impl Drop for Lockstep {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if let Some(mut r) = self.record.take() {
            r.line(&format!("end:{:08}", self.steps));
            let dir = parts_dir();
            std::fs::create_dir_all(&dir).expect("the parts directory");
            let name = format!("{:016x}.txt", digest::of(r.key.as_bytes()));
            let mut text = r.lines.join("\n");
            text.push('\n');
            std::fs::write(dir.join(name), text).expect("a run's part written");
        }
    }
}

impl std::ops::Deref for Lockstep {
    type Target = Budget;
    fn deref(&self) -> &Budget {
        &self.real
    }
}

impl Lockstep {
    /// Both budgets on `cfg`; they must agree on its validity too.
    pub fn new(cfg: BudgetConfig) -> Result<Self, ConfigError> {
        let real = Budget::new(cfg.clone());
        let frozen = frozen::Budget::new(cfg);
        match (real, frozen) {
            (Ok(real), Ok(frozen)) => Ok(Lockstep {
                real,
                frozen: Some(frozen),
                steps: 0,
                record: blessing().then(|| Recorder::new(thread_key("sim"))),
            }),
            (Err(a), Err(b)) => {
                assert_eq!(a, b, "lockstep: the configuration is refused differently");
                Err(a)
            }
            (a, b) => panic!(
                "lockstep: the configuration is valid for one engine only: real {:?}, frozen {:?}",
                a.err(),
                b.err()
            ),
        }
    }

    /// The real budget alone, the reference left out: for the bench
    /// (`benches/budget_decide.rs`), whose timings must not include it nor
    /// share their caches with it. Every test uses [`Lockstep::new`].
    pub fn unchecked(cfg: BudgetConfig) -> Result<Self, ConfigError> {
        Ok(Lockstep {
            real: Budget::new(cfg)?,
            frozen: None,
            steps: 0,
            record: None,
        })
    }

    /// Both budgets on `cfg`, the run recorded under `key` (not its
    /// thread's name): for a fixed corpus run from one test.
    pub fn keyed(cfg: BudgetConfig, key: String) -> Result<Self, ConfigError> {
        let mut l = Lockstep::new(cfg)?;
        if blessing() {
            l.record = Some(Recorder::new(key));
        }
        Ok(l)
    }

    /// One step on both, compared: a step given the whole need.
    pub fn decide(&mut self, input: FrameInput<'_>) -> Decisions {
        let NeedInput::Full(need) = input.need else {
            panic!("lockstep: a delta step goes through decide_delta, with its whole need");
        };
        self.decide_with(input, need, |b, i| (b.decide(i), ())).0
    }

    /// One step on both, compared: the real budget is given `input` (whole
    /// or a delta), the frozen one `need`, the whole need `input` stands for.
    pub fn decide_delta(&mut self, input: FrameInput<'_>, need: &NeedSet) -> Decisions {
        self.decide_with(input, need, |b, i| (b.decide(i), ())).0
    }

    /// One step on both, compared; `run` makes the real budget's call, so a
    /// caller can measure it (time, heap) without counting the reference's.
    /// `need` is the whole need `input` stands for, the frozen one's input.
    pub fn decide_with<R>(
        &mut self,
        input: FrameInput<'_>,
        need: &NeedSet,
        run: impl FnOnce(&mut Budget, FrameInput<'_>) -> (Decisions, R),
    ) -> (Decisions, R) {
        if let NeedInput::Full(n) = input.need {
            assert!(
                std::ptr::eq(n, need),
                "lockstep: a whole need's step compares on that need"
            );
        }
        let (d, r) = run(&mut self.real, input);
        if let Some(frozen) = &mut self.frozen {
            let f = frozen.decide(frozen::FrameInput {
                frame: input.frame,
                now: input.now,
                need,
                presence: input.presence,
            });
            Self::compare(self.steps, &self.real, frozen, need, &d, &f);
            if let Some(rec) = &mut self.record {
                rec.chains.fold(frozen_parts(frozen, need, &f));
            }
        }
        self.steps += 1;
        if let Some(rec) = &mut self.record {
            if self.steps.is_multiple_of(digest::CHECKPOINT) {
                rec.line(&format!("{:08}", self.steps));
            }
        }
        (d, r)
    }

    /// Compares one step's outputs and the state behind them; panics on the
    /// first divergence.
    fn compare(
        step: u64,
        real: &Budget,
        frozen: &frozen::Budget,
        need: &NeedSet,
        d: &Decisions,
        f: &Decisions,
    ) {
        let diff = decisions_diff(d, f);
        assert!(
            diff.is_empty() && d.digest() == f.digest(),
            "lockstep: step {step}: the decisions diverge from the frozen reference \
             (digest real {:016x}, frozen {:016x}); differing: {}\n real:   {d:?}\n frozen: {f:?}",
            d.digest(),
            f.digest(),
            diff.join(", "),
        );
        assert_eq!(
            real.ledger(),
            frozen.ledger(),
            "lockstep: step {step}: the ledgers diverge (left real, right frozen)"
        );
        assert_eq!(
            real.audit(),
            frozen.audit(),
            "lockstep: step {step}: the audits diverge (left real, right frozen)"
        );
        assert_eq!(
            real.tracked(),
            frozen.tracked(),
            "lockstep: step {step}: the tracked counts diverge (left real, right frozen)"
        );
        let (rs, fs) = (real.snapshot(), frozen.snapshot());
        if rs != fs {
            panic!(
                "lockstep: step {step}: the internal state diverges from the frozen reference; \
                 differing (left real, right frozen): {}",
                rs.diff(&fs).join("; ")
            );
        }
        let (rr, fr) = (real.residency(), frozen.residency());
        for (_, t, _) in need.entries() {
            assert_eq!(
                (rr.resident(t), rr.drawable(t), rr.loaded(t)),
                (fr.resident(t), fr.drawable(t), fr.loaded(t)),
                "lockstep: step {step}: residency of {t:?} diverges (left real, right frozen)"
            );
            Self::compare_view(step, real, frozen, ResourceId::Mesh(t));
        }
        // Every resource the step names: drawn meshes and the drapes they
        // wear, starts, deliveries, evictions, releases.
        let named = d
            .draw
            .items
            .iter()
            .flat_map(|i| std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource)))
            .chain(d.start.iter().map(|s| s.resource))
            .chain(d.deliver.iter().map(|(r, _)| *r))
            .chain(d.evict.iter().copied())
            .chain(d.release.iter().copied());
        for r in named {
            Self::compare_view(step, real, frozen, r);
        }
    }

    fn compare_view(step: u64, real: &Budget, frozen: &frozen::Budget, r: ResourceId) {
        assert_eq!(
            real.resource(r).map(view_fields),
            frozen.resource(r).map(frozen_view_fields),
            "lockstep: step {step}: the view of {r:?} diverges (left real, right frozen)"
        );
    }

    /// A load finished decoding, on both.
    pub fn on_loaded(&mut self, r: ResourceId, gen: Generation, cost: Cost) {
        self.real.on_loaded(r, gen, cost);
        if let Some(f) = &mut self.frozen {
            f.on_loaded(r, gen, cost);
        }
    }

    /// A load failed, on both.
    pub fn on_failed(&mut self, r: ResourceId, gen: Generation, why: Failure) {
        self.real.on_failed(r, gen, why);
        if let Some(f) = &mut self.frozen {
            f.on_failed(r, gen, why);
        }
    }

    /// A local read missed, on both.
    pub fn on_store_miss(&mut self, r: ResourceId, gen: Generation) {
        self.real.on_store_miss(r, gen);
        if let Some(f) = &mut self.frozen {
            f.on_store_miss(r, gen);
        }
    }

    /// An upload acknowledged, on both.
    pub fn on_drawable(&mut self, r: ResourceId, gen: Generation, actual: Bytes) {
        self.real.on_drawable(r, gen, actual);
        if let Some(f) = &mut self.frozen {
            f.on_drawable(r, gen, actual);
        }
    }

    /// An upload dropped, on both.
    pub fn on_discarded(&mut self, r: ResourceId, gen: Generation) {
        self.real.on_discarded(r, gen);
        if let Some(f) = &mut self.frozen {
            f.on_discarded(r, gen);
        }
    }

    /// A content epoch, on both.
    pub fn new_epoch(&mut self, class: Class, layer: Option<LayerId>) {
        self.real.new_epoch(class, layer);
        if let Some(f) = &mut self.frozen {
            f.new_epoch(class, layer);
        }
    }

    /// The memory signal, on both.
    pub fn set_pressure(&mut self, p: Pressure) {
        self.real.set_pressure(p);
        if let Some(f) = &mut self.frozen {
            f.set_pressure(p);
        }
    }

    /// The thermal state, on both.
    pub fn set_thermal(&mut self, t: Thermal) {
        self.real.set_thermal(t);
        if let Some(f) = &mut self.frozen {
            f.set_thermal(t);
        }
    }

    /// A new total, on both.
    pub fn retarget(&mut self, total: Bytes) {
        self.real.retarget(total);
        if let Some(f) = &mut self.frozen {
            f.retarget(total);
        }
    }

    /// Arrivals applied ahead of the step, on both; the ledgers compared.
    pub fn apply_arrivals(&mut self, now: Micros) {
        self.real.apply_arrivals(now);
        if let Some(f) = &mut self.frozen {
            f.apply_arrivals(now);
            assert_eq!(
                self.real.ledger(),
                f.ledger(),
                "lockstep: after apply_arrivals before step {}: the ledgers diverge",
                self.steps
            );
            if let Some(rec) = &mut self.record {
                rec.chains.fold([
                    0,
                    digest::ledger(&f.ledger(), &f.audit(), f.tracked()),
                    0,
                    0,
                ]);
            }
        }
    }

    /// Batch release, on both, compared.
    pub fn release_beyond(&mut self, target: Bytes) -> Released {
        let a = self.real.release_beyond(target);
        if let Some(f) = &mut self.frozen {
            let b = f.release_beyond(target);
            assert_eq!(
                a, b,
                "lockstep: after step {}: release_beyond diverges (left real, right frozen)",
                self.steps
            );
            assert_eq!(self.real.ledger(), f.ledger());
            let (rs, fs) = (self.real.snapshot(), f.snapshot());
            assert!(
                rs == fs,
                "lockstep: after step {}: release_beyond: the internal state diverges; \
                 differing (left real, right frozen): {}",
                self.steps,
                rs.diff(&fs).join("; ")
            );
            if let Some(rec) = &mut self.record {
                rec.chains.fold([
                    digest::of(&(&b.gpu, &b.decoded)),
                    digest::ledger(&f.ledger(), &f.audit(), f.tracked()),
                    0,
                    digest::of(&fs),
                ]);
            }
        }
        a
    }

    /// Drops the step buffers, on both.
    pub fn release_scratch(&mut self) {
        self.real.release_scratch();
        if let Some(f) = &mut self.frozen {
            f.release_scratch();
        }
    }

    /// Both audits; they must agree.
    pub fn audit_both(&self) -> Result<(), AuditError> {
        let a = self.real.audit();
        if let Some(f) = &self.frozen {
            assert_eq!(a, f.audit(), "lockstep: the audits diverge");
        }
        a
    }
}

/// The frozen engine's parts of a step (`digest`): its decisions, its
/// ledger, the residency and views of every tile the need names and of
/// every resource the step names, its whole internal state.
fn frozen_parts(f: &frozen::Budget, need: &NeedSet, d: &Decisions) -> [u64; 4] {
    let mut views = digest::Fnv::default();
    let res = f.residency();
    for (_, t, _) in need.entries() {
        use digest::Feed;
        (res.resident(t), res.drawable(t), res.loaded(t)).feed(&mut views);
        let r = ResourceId::Mesh(t);
        digest::feed_view(&mut views, r, f.resource(r).map(frozen_view_fields));
    }
    for r in named(d) {
        digest::feed_view(&mut views, r, f.resource(r).map(frozen_view_fields));
    }
    [
        digest::decisions(d),
        digest::ledger(&f.ledger(), &f.audit(), f.tracked()),
        views.finish(),
        digest::of(&f.snapshot()),
    ]
}

/// Every resource a step names: drawn meshes and the drapes they wear,
/// starts, deliveries, evictions, releases.
fn named(d: &Decisions) -> impl Iterator<Item = ResourceId> + '_ {
    d.draw
        .items
        .iter()
        .flat_map(|i| std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource)))
        .chain(d.start.iter().map(|s| s.resource))
        .chain(d.deliver.iter().map(|(r, _)| *r))
        .chain(d.evict.iter().copied())
        .chain(d.release.iter().copied())
}

/// The view's fields as a tuple, comparable with the frozen twin's.
type ViewFields = (
    tuile_budget::ResourceState,
    tuile_budget::Tier,
    Option<Generation>,
    Bytes,
    Bytes,
    Option<tuile_budget::Protection>,
    Option<tuile_budget::Urgency>,
    Option<tuile_budget::PriorityKey>,
);

fn view_fields(v: tuile_budget::ResourceView) -> ViewFields {
    (
        v.state,
        v.tier,
        v.gen,
        v.gpu,
        v.decoded,
        v.protected,
        v.urgency,
        v.priority,
    )
}

fn frozen_view_fields(v: frozen::view::ResourceView) -> ViewFields {
    (
        v.state,
        v.tier,
        v.gen,
        v.gpu,
        v.decoded,
        v.protected,
        v.urgency,
        v.priority,
    )
}

/// The fields on which two steps' decisions differ; `stats.work` (the cost
/// counter) is not compared.
pub fn decisions_diff(a: &Decisions, b: &Decisions) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut eq = |name: &'static str, same: bool| {
        if !same {
            out.push(name);
        }
    };
    eq("start", a.start == b.start);
    eq("cancel", a.cancel == b.cancel);
    eq("deliver", a.deliver == b.deliver);
    eq("evict", a.evict == b.evict);
    eq("release", a.release == b.release);
    eq("upload_plan", a.upload_plan == b.upload_plan);
    eq("draw", a.draw == b.draw);
    eq("sse_scale_milli", a.sse_scale_milli == b.sse_scale_milli);
    eq("frame_ready", a.frame_ready == b.frame_ready);
    eq("fatal", a.fatal == b.fatal);
    let (mut sa, mut sb) = (a.stats.clone(), b.stats.clone());
    sa.work = 0;
    sb.work = 0;
    eq("stats", sa == sb);
    out
}
