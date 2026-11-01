// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimNet`: seeded latency, jitter, bandwidth, failures, stalls (hosts that
//! never answer), per-class servers, and a local store with a presence index
//! that may lie.

use std::collections::{BTreeMap, BTreeSet};

use tuile_budget::{Generation, Micros, Presence, PresenceOracle, ResourceId, Source, Start};

use tuile_budget::cost::Cost;

use super::rng::Rng;
use super::world::cost;

/// One server's behaviour.
#[derive(Clone, Copy, Debug)]
pub struct Server {
    /// Fixed latency, ms.
    pub base_ms: f64,
    /// Mean of the exponential jitter, ms.
    pub jitter_ms: f64,
    /// Bytes per second per transfer.
    pub bandwidth: f64,
    /// Transient failures, per mille.
    pub fail_permille: u32,
    /// Requests that never answer, per mille.
    pub stall_permille: u32,
}

/// A network profile: one server for meshes, one for imagery, and the
/// local store's read latency.
#[derive(Clone, Copy, Debug)]
pub struct NetProfile {
    /// Terrain and 3D Tiles.
    pub meshes: Server,
    /// Imagery.
    pub imagery: Server,
    /// A local read and decode, ms.
    pub local_ms: f64,
}

impl NetProfile {
    /// A desktop on a good connection: p95 around 150 ms.
    pub fn desktop() -> Self {
        let s = Server {
            base_ms: 40.0,
            jitter_ms: 35.0,
            bandwidth: 40.0e6,
            fail_permille: 5,
            stall_permille: 1,
        };
        NetProfile {
            meshes: s,
            imagery: Server { base_ms: 60.0, ..s },
            local_ms: 3.0,
        }
    }

    /// A phone on cellular: p95 around 450 ms.
    pub fn handheld() -> Self {
        let s = Server {
            base_ms: 120.0,
            jitter_ms: 100.0,
            bandwidth: 8.0e6,
            fail_permille: 10,
            stall_permille: 2,
        };
        NetProfile {
            meshes: s,
            imagery: Server {
                base_ms: 160.0,
                ..s
            },
            local_ms: 8.0,
        }
    }

    /// A network where nothing fails and nothing stalls (for exactness).
    pub fn clean(mut self) -> Self {
        for s in [&mut self.meshes, &mut self.imagery] {
            s.fail_permille = 0;
            s.stall_permille = 0;
        }
        self
    }
}

/// What travels on the wire for a resource of `gpu` bytes: imagery comes
/// as JPEG or WebP tiles (about 1/16 of RGBA with mips), meshes as
/// quantized-mesh (about 1/4 of the vertex and index buffers).
pub fn wire_bytes(r: ResourceId, gpu: u64) -> u64 {
    match r {
        ResourceId::Mesh(_) => gpu / 4,
        ResourceId::Drape { .. } => gpu / 16,
    }
}

/// What came back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arrival {
    /// Decoded, at `cost`.
    Loaded(ResourceId, Generation, Cost),
    /// Failed, transient.
    Failed(ResourceId, Generation),
    /// The local store did not have it after all.
    StoreMiss(ResourceId, Generation),
}

#[derive(Clone, Copy, Debug)]
struct Flight {
    gen: Generation,
    started: Micros,
    done_at: Option<Micros>,
    outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    Fail,
    Miss,
}

/// The local store: what survived, and the index's false positives.
#[derive(Clone, Debug, Default)]
pub struct SimStore {
    /// Held for real.
    pub held: BTreeSet<ResourceId>,
    /// Claimed but not held.
    pub lies: BTreeSet<ResourceId>,
    /// Bumped by every change to `held` or `lies`: the index's generation.
    pub generation: u64,
}

impl PresenceOracle for SimStore {
    fn holds(&self, r: ResourceId) -> Presence {
        if self.held.contains(&r) || self.lies.contains(&r) {
            Presence::Stored
        } else {
            Presence::Absent
        }
    }

    fn generation(&self) -> Option<u64> {
        Some(self.generation)
    }
}

/// The simulated network and store.
pub struct SimNet {
    /// The profile; may change mid-run.
    pub profile: NetProfile,
    rng: Rng,
    flights: BTreeMap<ResourceId, Flight>,
    /// The local store.
    pub store: SimStore,
    /// Latencies of completed network loads, ms, with their completion time.
    pub latencies: Vec<(Micros, f64)>,
    /// Requests started over the network, total.
    pub network_starts: u64,
    /// Network bytes transferred, total.
    pub network_bytes: u64,
    /// Whether what arrives fills the local store (a disk cache); off for
    /// a device that keeps nothing between evictions.
    pub store_fills: bool,
    /// A network outage: every network load started in `[from, to)` fails
    /// at once (a refused connection, a resolver with no answer: back within
    /// a millisecond, not after a round trip).
    pub outage: Option<(Micros, Micros)>,
}

impl SimNet {
    /// A network on `profile`, seeded.
    pub fn new(profile: NetProfile, seed: u64) -> Self {
        SimNet {
            profile,
            rng: Rng::new(seed),
            flights: BTreeMap::new(),
            store: SimStore::default(),
            latencies: Vec::new(),
            network_starts: 0,
            network_bytes: 0,
            store_fills: true,
            outage: None,
        }
    }

    /// Whether the network is out at `now`.
    pub fn out(&self, now: Micros) -> bool {
        self.outage
            .is_some_and(|(from, to)| from <= now && now < to)
    }

    /// Whether `r` is in flight.
    pub fn in_flight(&self, r: ResourceId) -> bool {
        self.flights.contains_key(&r)
    }

    /// Starts a load. Panics on a duplicate: two loads of one resource in
    /// flight is a budget bug.
    pub fn start(&mut self, s: &Start, now: Micros) {
        assert!(
            !self.flights.contains_key(&s.resource),
            "{:?} started twice in flight",
            s.resource
        );
        let bytes = cost(s.resource);
        let (done_at, outcome) = match s.source {
            Source::Local => {
                let lie = !self.store.held.contains(&s.resource);
                if lie && self.store.lies.remove(&s.resource) {
                    self.store.generation += 1;
                }
                let at = now.plus(Micros((self.profile.local_ms * 1_000.0) as u64));
                (Some(at), if lie { Outcome::Miss } else { Outcome::Ok })
            }
            Source::Network => {
                self.network_starts += 1;
                let server = match s.resource {
                    ResourceId::Mesh(_) => self.profile.meshes,
                    ResourceId::Drape { .. } => self.profile.imagery,
                };
                if self.out(now) {
                    (Some(now.plus(Micros::from_millis(1))), Outcome::Fail)
                } else if self.rng.permille(server.stall_permille) {
                    (None, Outcome::Ok)
                } else {
                    let ms = server.base_ms
                        + self.rng.exp(server.jitter_ms)
                        + wire_bytes(s.resource, bytes.gpu.0) as f64 / server.bandwidth * 1_000.0;
                    let out = if self.rng.permille(server.fail_permille) {
                        Outcome::Fail
                    } else {
                        Outcome::Ok
                    };
                    (Some(now.plus(Micros((ms * 1_000.0) as u64))), out)
                }
            }
        };
        self.flights.insert(
            s.resource,
            Flight {
                gen: s.gen,
                started: now,
                done_at,
                outcome,
            },
        );
    }

    /// Cancels a load; its result never comes.
    pub fn cancel(&mut self, r: ResourceId, gen: Generation) {
        if self.flights.get(&r).map(|f| f.gen) == Some(gen) {
            self.flights.remove(&r);
        }
    }

    /// What arrived by `now`, in completion order.
    pub fn poll(&mut self, now: Micros) -> Vec<Arrival> {
        let mut due: Vec<(Micros, ResourceId)> = self
            .flights
            .iter()
            .filter_map(|(r, f)| f.done_at.filter(|t| *t <= now).map(|t| (t, *r)))
            .collect();
        due.sort();
        let mut out = Vec::new();
        for (at, r) in due {
            let Some(f) = self.flights.remove(&r) else {
                continue;
            };
            match f.outcome {
                Outcome::Ok => {
                    let local = self.store.held.contains(&r);
                    if !local {
                        self.network_bytes += wire_bytes(r, cost(r).gpu.0);
                        self.latencies
                            .push((at, at.since(f.started).0 as f64 / 1_000.0));
                    }
                    if self.store_fills && self.store.held.insert(r) {
                        self.store.generation += 1;
                    }
                    out.push(Arrival::Loaded(r, f.gen, cost(r)));
                }
                Outcome::Fail => out.push(Arrival::Failed(r, f.gen)),
                Outcome::Miss => out.push(Arrival::StoreMiss(r, f.gen)),
            }
        }
        out
    }
}
