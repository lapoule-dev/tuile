// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The guard of the O1 optimisation pass: the real [`Budget`], checked step
//! by step against what the engine as it stood before O1 produced on the
//! same inputs.
//!
//! O1 began with a frozen copy of that engine kept here and driven beside
//! the real one. Before it was retired, it recorded what it produced on
//! every `budget_sim` run and on a fixed corpus of property scripts
//! (`budget_properties.rs`): per step, four digests ([`digest`]) — the
//! decisions (digest and content, the statistics but `work`), the ledger
//! (with the audit's verdict and the tracked count), the views (the
//! residency and view of every tile the need names, the view of every
//! resource the step names), and the whole internal state
//! (`tuile_budget::snapshot`). A run folds them into one chain per part;
//! `recorded.txt` holds the chains every [`digest::CHECKPOINT`] steps and at
//! the run's end. The frozen engine is gone from the tree; the file was
//! recorded from it at the commit that removed it (named in that commit's
//! message), and can be recorded again from that commit's parent, which
//! still holds it: `TUILE_BLESS_REFERENCE=1` on `budget_sim` and on the
//! corpus test, then `assemble_the_recorded_reference`.
//!
//! [`Lockstep`] (named for what it was) runs the real budget, folds the
//! same digests from it, and compares them with the recorded ones at every
//! checkpoint and at the run's end; the first difference panics with the
//! run, the steps between the last matching checkpoint and this one, and
//! the parts that differ. A run whose key is not in the file (a test added
//! since) is not checked. Recording again from the real engine
//! (`TUILE_BLESS_REFERENCE=1` here) is for a deliberate, documented change
//! of behaviour only, reviewed as such.

// Not every binary that includes the harness uses all of it.
#![allow(dead_code)]

pub mod digest;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use digest::{Chains, Feed};
use tuile_budget::{
    cost::Cost, AuditError, Budget, BudgetConfig, Bytes, Class, ConfigError, Decisions, Failure,
    FrameInput, Generation, LayerId, Micros, NeedSet, Pressure, Released, ResourceId, Thermal,
};

/// The real budget, checked against the recorded reference. Reads go to it
/// (`Deref`); every mutation goes through the methods below, which fold
/// what the reference recorded of them.
#[derive(Debug)]
pub struct Lockstep {
    /// The budget under test.
    pub real: Budget,
    /// Steps taken so far.
    pub steps: u64,
    /// The run's check against the reference, or its recording; `None`
    /// for the bench ([`Lockstep::unchecked`]) and for random scripts.
    run: Option<Run>,
}

impl std::ops::Deref for Lockstep {
    type Target = Budget;
    fn deref(&self) -> &Budget {
        &self.real
    }
}

/// A run's key, its chains, and what it is checked against or records.
#[derive(Debug)]
struct Run {
    key: String,
    chains: Chains,
    /// The recorded checkpoints of the run, in order (checking).
    expected: Option<&'static [(String, Chains)]>,
    /// The last checkpoint matched, in steps.
    matched: u64,
    /// The lines written (recording).
    lines: Vec<String>,
}

impl Run {
    fn new(key: String) -> Option<Self> {
        assert!(
            !key.contains('\t') && !key.contains('\n'),
            "a run's key is one field"
        );
        let expected = recorded().get(&key).map(Vec::as_slice);
        (expected.is_some() || blessing()).then(|| Run {
            key,
            chains: Chains::default(),
            expected: if blessing() { None } else { expected },
            matched: 0,
            lines: Vec::new(),
        })
    }

    /// The checkpoint `at` (`00000256`, `end:00000300`): recorded, or
    /// compared with the reference.
    fn checkpoint(&mut self, at: &str, steps: u64) {
        let c = self.chains.0;
        if blessing() {
            self.lines.push(format!(
                "{}\t{at}\t{:016x} {:016x} {:016x} {:016x}",
                self.key, c[0], c[1], c[2], c[3]
            ));
            return;
        }
        let Some(expected) = self.expected else {
            return;
        };
        let want = expected.iter().find(|(a, _)| a == at);
        match want {
            Some((_, w)) if *w == self.chains => self.matched = steps,
            Some((_, w)) => panic!(
                "reference: run {}: steps {}..={steps} diverge from the recorded reference; \
                 differing: {}",
                self.key,
                self.matched + 1,
                self.chains.differing(w).join(", ")
            ),
            None => {
                let end = expected.last().map_or("?", |(a, _)| a.as_str());
                panic!(
                    "reference: run {}: {at} is not recorded (the recorded run ends at {end}): \
                     the run took another number of steps",
                    self.key
                )
            }
        }
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

/// The recorded reference, by run: each run's checkpoints, in order.
fn recorded() -> &'static BTreeMap<String, Vec<(String, Chains)>> {
    static RECORDED: OnceLock<BTreeMap<String, Vec<(String, Chains)>>> = OnceLock::new();
    RECORDED.get_or_init(|| {
        let text = std::fs::read_to_string(recorded_path()).unwrap_or_default();
        let mut runs: BTreeMap<String, Vec<(String, Chains)>> = BTreeMap::new();
        for line in text.lines() {
            let mut f = line.split('\t');
            let (Some(key), Some(at), Some(c)) = (f.next(), f.next(), f.next()) else {
                panic!("reference: a malformed line: {line}");
            };
            let mut chains = Chains::default();
            let mut parts = c.split(' ');
            for x in &mut chains.0 {
                *x = parts
                    .next()
                    .and_then(|p| u64::from_str_radix(p, 16).ok())
                    .unwrap_or_else(|| panic!("reference: a malformed line: {line}"));
            }
            runs.entry(key.to_owned())
                .or_default()
                .push((at.to_owned(), chains));
        }
        runs
    })
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
        let Some(mut run) = self.run.take() else {
            return;
        };
        run.checkpoint(&format!("end:{:08}", self.steps), self.steps);
        if blessing() {
            let dir = parts_dir();
            std::fs::create_dir_all(&dir).expect("the parts directory");
            let name = format!("{:016x}.txt", digest::of(run.key.as_bytes()));
            let mut text = run.lines.join("\n");
            text.push('\n');
            std::fs::write(dir.join(name), text).expect("a run's part written");
        }
    }
}

impl Lockstep {
    /// The budget on `cfg`, its run keyed by its test's thread.
    pub fn new(cfg: BudgetConfig) -> Result<Self, ConfigError> {
        Ok(Lockstep {
            real: Budget::new(cfg)?,
            steps: 0,
            run: Run::new(thread_key("sim")),
        })
    }

    /// The budget on `cfg`, its run keyed `key` (not by its thread): for a
    /// fixed corpus run from one test.
    pub fn keyed(cfg: BudgetConfig, key: String) -> Result<Self, ConfigError> {
        Ok(Lockstep {
            real: Budget::new(cfg)?,
            steps: 0,
            run: Run::new(key),
        })
    }

    /// The budget alone, unchecked: for the bench
    /// (`benches/budget_decide.rs`) and for random scripts, which no
    /// recording covers.
    pub fn unchecked(cfg: BudgetConfig) -> Result<Self, ConfigError> {
        Ok(Lockstep {
            real: Budget::new(cfg)?,
            steps: 0,
            run: None,
        })
    }

    /// One step, checked: a step given the whole need.
    pub fn decide(&mut self, input: FrameInput<'_>) -> Decisions {
        let tuile_budget::NeedInput::Full(need) = input.need else {
            panic!("lockstep: a delta step goes through decide_delta, with its whole need");
        };
        self.decide_with(input, need, |b, i| (b.decide(i), ())).0
    }

    /// One step, checked: the budget is given `input` (whole or a delta);
    /// `need` is the whole need it stands for, whose tiles' views count.
    pub fn decide_delta(&mut self, input: FrameInput<'_>, need: &NeedSet) -> Decisions {
        self.decide_with(input, need, |b, i| (b.decide(i), ())).0
    }

    /// One step, checked; `run` makes the budget's call, so a caller can
    /// measure it (time, heap) without counting the check's.
    pub fn decide_with<R>(
        &mut self,
        input: FrameInput<'_>,
        need: &NeedSet,
        run: impl FnOnce(&mut Budget, FrameInput<'_>) -> (Decisions, R),
    ) -> (Decisions, R) {
        let (d, r) = run(&mut self.real, input);
        self.folded(need, &d);
        (d, r)
    }

    /// One step, checked, written into `out` as the host does
    /// (`Budget::decide_into`: `out` emptied, its buffers kept); `run` makes
    /// the budget's call, so a caller can measure it without counting the
    /// check's.
    pub fn decide_into_with<R>(
        &mut self,
        input: FrameInput<'_>,
        need: &NeedSet,
        out: &mut Decisions,
        run: impl FnOnce(&mut Budget, FrameInput<'_>, &mut Decisions) -> R,
    ) -> R {
        let r = run(&mut self.real, input, out);
        self.folded(need, out);
        r
    }

    /// A step was taken, deciding `d`: its digests folded, the checkpoint
    /// compared when due.
    fn folded(&mut self, need: &NeedSet, d: &Decisions) {
        self.steps += 1;
        if let Some(rn) = &mut self.run {
            rn.chains.fold(parts(&self.real, need, d));
            if self.steps.is_multiple_of(digest::CHECKPOINT) {
                rn.checkpoint(&format!("{:08}", self.steps), self.steps);
            }
        }
    }

    /// A load finished decoding.
    pub fn on_loaded(&mut self, r: ResourceId, gen: Generation, cost: Cost) {
        self.real.on_loaded(r, gen, cost);
    }

    /// A load failed.
    pub fn on_failed(&mut self, r: ResourceId, gen: Generation, why: Failure) {
        self.real.on_failed(r, gen, why);
    }

    /// A local read missed.
    pub fn on_store_miss(&mut self, r: ResourceId, gen: Generation) {
        self.real.on_store_miss(r, gen);
    }

    /// An upload acknowledged.
    pub fn on_drawable(&mut self, r: ResourceId, gen: Generation, actual: Bytes) {
        self.real.on_drawable(r, gen, actual);
    }

    /// An upload dropped.
    pub fn on_discarded(&mut self, r: ResourceId, gen: Generation) {
        self.real.on_discarded(r, gen);
    }

    /// A content epoch.
    pub fn new_epoch(&mut self, class: Class, layer: Option<LayerId>) {
        self.real.new_epoch(class, layer);
    }

    /// The memory signal.
    pub fn set_pressure(&mut self, p: Pressure) {
        self.real.set_pressure(p);
    }

    /// The thermal state.
    pub fn set_thermal(&mut self, t: Thermal) {
        self.real.set_thermal(t);
    }

    /// A new total.
    pub fn retarget(&mut self, total: Bytes) {
        self.real.retarget(total);
    }

    /// Arrivals applied ahead of the step; the ledger folded.
    pub fn apply_arrivals(&mut self, now: Micros) {
        self.real.apply_arrivals(now);
        if let Some(rn) = &mut self.run {
            let b = &self.real;
            rn.chains.fold([
                0,
                digest::ledger(&b.ledger(), &b.audit(), b.tracked()),
                0,
                0,
            ]);
        }
    }

    /// Batch release; what it let go of, the ledger and the state folded.
    pub fn release_beyond(&mut self, target: Bytes) -> Released {
        let a = self.real.release_beyond(target);
        if let Some(rn) = &mut self.run {
            let b = &self.real;
            rn.chains.fold([
                digest::of(&(&a.gpu, &a.decoded)),
                digest::ledger(&b.ledger(), &b.audit(), b.tracked()),
                0,
                digest::of(&b.snapshot()),
            ]);
        }
        a
    }

    /// Drops the step buffers.
    pub fn release_scratch(&mut self) {
        self.real.release_scratch();
    }

    /// The audit.
    pub fn audit_both(&self) -> Result<(), AuditError> {
        self.real.audit()
    }
}

/// A step's four parts ([`digest`]): its decisions, its ledger, the
/// residency and views of every tile the need names and of every resource
/// the step names, its whole internal state.
fn parts(b: &Budget, need: &NeedSet, d: &Decisions) -> [u64; 4] {
    let mut views = digest::Fnv::default();
    let res = b.residency();
    for (_, t, _) in need.entries() {
        (res.resident(t), res.drawable(t), res.loaded(t)).feed(&mut views);
        let r = ResourceId::Mesh(t);
        digest::feed_view(&mut views, r, b.resource(r).map(view_fields));
    }
    for r in named(d) {
        digest::feed_view(&mut views, r, b.resource(r).map(view_fields));
    }
    [
        digest::decisions(d),
        digest::ledger(&b.ledger(), &b.audit(), b.tracked()),
        views.finish(),
        digest::of(&b.snapshot()),
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

fn view_fields(v: tuile_budget::ResourceView) -> digest::ViewFields {
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
