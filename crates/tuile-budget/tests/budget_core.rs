// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget's behaviour, by hand: one scenario per behaviour, through the
//! public API only, on a stepped clock.
//!
//! Each test names the invariant (I1–I15, `graphics-budget-to-be-and-spec.md`)
//! and the checklist item (C1–C30, `graphics-budget-independent-practices.md`
//! §8) it pins down.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use budget_common::*;
use tuile_budget::cost::Cost;
use tuile_budget::{
    Budget, BudgetConfig, Bytes, Class, ConfigError, Failure, FailurePolicy, LayerId, Micros,
    NeedSet, PerClass, Pressure, Protection, ResourceId, ResourceState, Role, SessionError, Source,
    Thermal, Tier, Urgency,
};

const RED: &str = "the budget core regressed";

fn mesh(id: tuile_budget::TileId) -> ResourceId {
    ResourceId::Mesh(id)
}

/// Every step of `ds` asked to start `r`?
fn started(ds: &[tuile_budget::Decisions], r: ResourceId) -> bool {
    ds.iter().any(|d| d.start.iter().any(|s| s.resource == r))
}

fn small(target_meshes: u64) -> BudgetConfig {
    let mut c = Rig::interactive_config();
    c.target[Class::TerrainMesh] = Bytes(MESH.0 * target_meshes);
    c.overflow[Class::TerrainMesh] = Bytes::ZERO;
    c.hard_limit[Class::TerrainMesh] = Bytes::mib(512);
    c.decoded_target = Bytes::ZERO;
    c
}

mod config {
    use super::*;

    /// I13, C1: films run unbounded, exact, and end on failure.
    #[test]
    fn a_batch_budget_is_unbounded_and_ends_the_session_on_failure() {
        let c = BudgetConfig::batch();
        for class in Class::ALL {
            assert_eq!(c.target[class], Bytes::MAX);
        }
        assert_eq!(c.upload_bytes_per_frame, Bytes::MAX);
        assert_eq!(c.failure, FailurePolicy::EndSession);
        assert_eq!(c.cancel_grace, Micros::NEVER);
        assert!(Budget::new(c).is_ok());
    }

    /// Engineering bar: incoherent numbers are refused with a typed error.
    #[test]
    fn incoherent_watermarks_are_refused_with_a_typed_error() {
        let mut c = BudgetConfig::interactive(Bytes::mib(256));
        c.watermarks.low_percent = 95;
        c.watermarks.high_percent = 90;
        assert_eq!(
            Budget::new(c).err(),
            Some(ConfigError::Watermarks { low: 95, high: 90 })
        );
    }

    /// I13: a batch budget that would retry is refused.
    #[test]
    fn a_batch_budget_that_retries_is_refused() {
        let mut c = BudgetConfig::batch();
        c.failure = FailurePolicy::RetryBackoff {
            first: Micros::from_millis(10),
            max: Micros::from_secs(1),
        };
        assert_eq!(
            Budget::new(c).err(),
            Some(ConfigError::BatchMustEndOnFailure)
        );
    }

    /// C1: the interactive split is per class and never exceeds the total.
    #[test]
    fn the_interactive_split_never_exceeds_the_total() {
        let total = Bytes::mib(300);
        let c = BudgetConfig::interactive(total);
        let sum = c.target.iter().fold(Bytes::ZERO, |a, (_, b)| a.plus(b));
        assert!(sum <= total);
        assert!(c.target.iter().all(|(_, b)| b > Bytes::ZERO));
    }

    /// D-A3 (decided 2026-10-20) and D3, spec §15.1: the ground is one pool
    /// of 82 % of the device — terrain between 25 % and 60 % of it, imagery
    /// between 40 % and 75 % —, the stand-ins' class gone, its 2 % in the
    /// pool.
    #[test]
    fn the_ground_is_one_pool_with_floors_and_ceilings() {
        let total = Bytes::gib(1);
        let c = BudgetConfig::interactive(total);
        let ground = total.scaled(82, 100);
        let p = c.pool;
        assert!(
            p.holds(Class::TerrainMesh) && p.holds(Class::Drape),
            "{RED}"
        );
        assert!(!p.holds(Class::Tiles3dMesh) && !p.holds(Class::Overlay));
        assert_eq!(p.hard_limit, ground);
        assert_eq!(c.hard_limit[Class::TerrainMesh], ground.scaled(60, 100));
        assert_eq!(c.hard_limit[Class::Drape], ground.scaled(75, 100));
        assert_eq!(
            (
                p.floor_percent[Class::TerrainMesh],
                p.floor_percent[Class::Drape]
            ),
            (25, 40)
        );
        let outside = c.hard_limit[Class::Tiles3dMesh].plus(c.hard_limit[Class::Overlay]);
        let all = outside.plus(p.hard_limit);
        assert!(
            all <= total && total.minus(all) < Bytes(4),
            "the pool and the other classes share the whole device"
        );
        assert_eq!(Class::ALL.len(), 4, "no class for stand-ins");
    }

    /// Engineering bar: a pool whose floors exceed the whole is refused.
    #[test]
    fn a_pool_whose_floors_exceed_the_whole_is_refused() {
        let mut c = BudgetConfig::interactive(Bytes::mib(256));
        c.pool.floor_percent[Class::Drape] = 90;
        assert_eq!(Budget::new(c).err(), Some(ConfigError::Pool));
    }
}

mod pool {
    use super::*;

    /// A ground of `pool` meshes' worth, terrain and imagery pooled, the
    /// classes' ceilings at the pool's, floors at 25 % and 40 %.
    fn pooled(pool: u64) -> BudgetConfig {
        let mut c = Rig::interactive_config();
        let whole = Bytes(MESH.0 * pool);
        c.pool.target = whole;
        c.pool.overflow = Bytes::ZERO;
        c.pool.hard_limit = Bytes::mib(512);
        for k in [Class::TerrainMesh, Class::Drape] {
            c.target[k] = whole;
            c.overflow[k] = Bytes::ZERO;
            c.hard_limit[k] = Bytes::mib(512);
        }
        c.decoded_target = Bytes::ZERO;
        c
    }

    /// D-A3: a member grows into what the other leaves — terrain alone may
    /// take the whole pool, up to its ceiling, with no drape to share it.
    #[test]
    fn a_member_grows_into_what_the_other_leaves() {
        let mut r = Rig::new(pooled(8));
        let tiles: Vec<_> = (0..8).map(|x| t(4, x, 0)).collect();
        let d = r.settle(&drawn(&tiles), 30);
        assert_eq!(
            d.stats.target[Class::TerrainMesh],
            Bytes(MESH.0 * 8),
            "{RED}"
        );
        assert!(d.evict.is_empty());
        assert_eq!(d.stats.exact, 8);
    }

    /// D-A3: one sweep over the pool, in the one eviction order: what is
    /// least needed goes, whatever its class, down to the pool's low
    /// watermark — and a member is not swept below its floor.
    #[test]
    fn one_sweep_trims_the_pool_and_spares_a_member_at_its_floor() {
        let mut c = pooled(8);
        c.pool.floor_percent[Class::Drape] = 40;
        let mut r = Rig::new(c);
        let draped: Vec<_> = (0..3).map(|x| t(3, x, 0)).collect();
        r.settle(&drawn_draped(&draped, 3), 30);
        let old: Vec<_> = (0..4).map(|x| t(4, x, 9)).collect();
        r.settle(&drawn(&old), 30);
        let new: Vec<_> = (0..4).map(|x| t(4, x, 12)).collect();
        let d = r.settle(&drawn(&new), 40);
        let used = d.stats.gpu[Class::TerrainMesh].plus(d.stats.gpu[Class::Drape]);
        assert!(used <= Bytes(MESH.0 * 8), "{RED}: the pool over its target");
        assert!(
            d.stats.gpu[Class::Drape] > Bytes::ZERO,
            "the drapes, at their floor, are spared"
        );
    }

    /// D-A3: the members together never cross the pool's hard limit, even
    /// when each alone is under its own.
    #[test]
    fn the_members_together_never_cross_the_pools_hard_limit() {
        let mut c = pooled(64);
        c.pool.hard_limit = Bytes(MESH.0 * 6 + drape().0 * 2);
        c.pool.target = c.pool.hard_limit;
        let mut r = Rig::new(c.clone());
        let tiles: Vec<_> = (0..8).map(|x| t(4, x, 0)).collect();
        for _ in 0..40 {
            let d = r.step(&drawn_draped(&tiles, 4));
            let used = d.stats.gpu[Class::TerrainMesh].plus(d.stats.gpu[Class::Drape]);
            assert!(used <= c.pool.hard_limit, "{RED}: {used:?}");
            r.complete(&d);
        }
    }
}

mod ledger {
    use super::*;
    use tuile_budget::cost::{drape_cost, mesh_cost, within_drift, Cost, TextureFormat};

    /// C2: a drape costs its full mip chain, level by level; its staging
    /// copy aligns every row to 256 bytes.
    #[test]
    fn a_drape_costs_its_mip_chain_and_its_aligned_staging() {
        assert_eq!(
            drape_cost(256, 256, TextureFormat::Rgba8),
            Cost {
                gpu: Bytes(349_524),
                staging: Bytes(360_192),
            }
        );
        assert_eq!(
            drape_cost(256, 256, TextureFormat::Block16),
            Cost {
                gpu: Bytes(87_408),
                staging: Bytes(90_368),
            }
        );
    }

    /// C2: a 100-texel row of RGBA stages as 512 bytes, not 400.
    #[test]
    fn an_unaligned_row_stages_at_the_next_256_bytes() {
        let c = drape_cost(100, 1, TextureFormat::Rgba8);
        // 100, 50, 25, 12, 6, 3, 1 texels wide, one row each.
        assert_eq!(c.gpu, Bytes((100 + 50 + 25 + 12 + 6 + 3 + 1) * 4));
        assert_eq!(c.staging, Bytes(512 + 256 * 6));
    }

    /// C2: buffers align to 4, the per-tile uniform to 256.
    #[test]
    fn buffers_and_the_uniform_are_aligned() {
        let c = mesh_cost(3, 6, 3, false);
        assert_eq!(c.gpu, Bytes(20 + 12 + 8_448));
        assert_eq!(c.staging, c.gpu);
    }

    /// C2: on unified memory the CPU copy kept for stitching is paid for,
    /// but never staged.
    #[test]
    fn a_mesh_costs_its_cpu_copy_on_unified_memory() {
        let without = mesh_cost(1_000, 16, 6_000, false);
        let with = mesh_cost(1_000, 16, 6_000, true);
        assert_eq!(with.gpu.0 - without.gpu.0, 16_000 + 24_000);
        assert_eq!(with.staging, without.staging);
    }

    /// D5, C2: the worst-case drape per profile, at its cap and side.
    #[test]
    fn the_worst_drape_per_profile_is_bounded() {
        for (cap, side, worst) in [
            (3, 2_048, 22_369_620),
            (2, 1_024, 5_592_404),
            (1, 512, 1_398_100),
        ] {
            let mut c = BudgetConfig::interactive(Bytes::mib(512));
            c.imagery_over_terrain_cap = cap;
            c.max_drape_side = side;
            let tiles = 1u32 << c.imagery_cap();
            let cost = drape_cost(256 * tiles, 256 * tiles, TextureFormat::Rgba8);
            assert_eq!(cost.gpu, Bytes(worst), "cap {cap}, side {side}");
        }
    }

    /// C2: an estimate more than 5 % off the actual bytes is detected.
    #[test]
    fn an_estimate_drift_above_five_percent_is_detected() {
        assert!(within_drift(Bytes(1_000), Bytes(1_050)));
        assert!(!within_drift(Bytes(1_000), Bytes(1_051)));
    }

    /// I2, C2: in transit the estimate is charged; once drawable, the actual.
    #[test]
    fn the_ledger_charges_the_estimate_in_transit_and_the_actual_once_drawable() {
        let mut r = Rig::interactive();
        let a = t(0, 0, 0);
        let need = drawn(&[a]);
        let d = r.step(&need);
        assert_eq!(d.start.len(), 1, "{RED}");
        r.load(&d);
        let d = r.step(&need);
        assert_eq!(d.deliver.len(), 1);
        assert_eq!(r.b.ledger().gpu[Class::TerrainMesh], MESH);
        assert_eq!(r.b.ledger().in_transit, MESH);
        let (_, g) = d.deliver[0];
        r.b.on_drawable(mesh(a), g, Bytes(MESH.0 + 1_000));
        r.step(&need);
        assert_eq!(r.b.ledger().gpu[Class::TerrainMesh], Bytes(MESH.0 + 1_000));
        assert_eq!(r.b.ledger().in_transit, Bytes::ZERO);
    }

    /// I2, C28: the ledger equals the sum of the states after every step.
    #[test]
    fn the_ledger_equals_the_sum_of_the_states_after_every_step() {
        let mut r = Rig::interactive();
        let tiles: Vec<_> = (0..4)
            .flat_map(|x| (0..4).map(move |y| t(2, x, y)))
            .collect();
        let need = drawn_draped(&tiles, 4);
        for _ in 0..20 {
            let d = r.step(&need);
            r.b.audit().unwrap();
            r.complete(&d);
        }
        assert!(r.b.tracked() >= 32, "{RED}");
        assert!(r.b.ledger().gpu[Class::Drape] > Bytes::ZERO);
    }

    /// I2: an eviction gives its bytes back.
    #[test]
    fn an_eviction_gives_its_bytes_back() {
        let mut r = Rig::new(small(4));
        let first = children(t(1, 0, 0));
        r.settle(&drawn(&first), 20);
        let second = children(t(1, 1, 1));
        let mut evicted = 0;
        for _ in 0..20 {
            let d = r.step(&drawn(&second));
            evicted += d.evict.len();
            r.complete(&d);
        }
        assert!(evicted > 0, "{RED}");
        let drawable = first
            .iter()
            .chain(&second)
            .filter(|&&id| r.b.residency().drawable(id))
            .count() as u64;
        assert_eq!(
            r.b.ledger().gpu[Class::TerrainMesh],
            Bytes(MESH.0 * drawable)
        );
    }
}

mod requests {
    use super::*;

    /// C7, C11: the pinned floor goes first.
    #[test]
    fn the_floor_is_requested_before_anything_else() {
        let mut c = Rig::interactive_config();
        c.pinned_level = Some(1);
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let deep = t(3, 5, 5);
        let mut need = drawn(&[deep]);
        need.ancestors = vec![
            (t(0, 0, 0), need_of(t(0, 0, 0))),
            (t(1, 1, 1), need_of(t(1, 1, 1))),
        ];
        let mut order = Vec::new();
        for _ in 0..10 {
            let d = r.step(&need);
            order.extend(d.start.iter().map(|s| s.resource));
            r.complete(&d);
        }
        assert_eq!(
            order.get(..2),
            Some(&[mesh(t(0, 0, 0)), mesh(t(1, 1, 1))][..]),
            "{RED}"
        );
        assert_eq!(
            r.b.resource(mesh(t(0, 0, 0))).and_then(|v| v.protected),
            Some(Protection::Floor)
        );
    }

    fn need_of(id: tuile_budget::TileId) -> tuile_budget::Need {
        need(id)
    }

    /// C11, C12: a hole outranks a blocker, which outranks the ring, which
    /// outranks a preloaded ancestor.
    #[test]
    fn a_hole_outranks_a_blocker_which_outranks_the_ring_then_ancestors() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let (h, b, g, a) = (t(3, 0, 0), t(3, 1, 0), t(3, 2, 0), t(3, 3, 0));
        let need = NeedSet {
            drawn: vec![(h, need(h))],
            blockers: vec![(b, need(b))],
            ring: vec![(g, need(g))],
            ancestors: vec![(a, need(a))],
        };
        let mut order = Vec::new();
        for _ in 0..8 {
            let d = r.step(&need);
            order.extend(d.start.iter().map(|s| s.resource));
            r.complete(&d);
        }
        assert_eq!(order, vec![mesh(h), mesh(b), mesh(g), mesh(a)], "{RED}");
    }

    /// C12: on a zoom-out, the coarser tile whose absence forces over-detail
    /// is urgent.
    #[test]
    fn a_zoom_out_ancestor_is_urgent() {
        let mut r = Rig::interactive();
        let p = t(2, 1, 1);
        r.settle(&drawn(&children(p)), 20);
        r.step(&drawn(&[p]));
        let v = r.b.resource(mesh(p)).expect(RED);
        assert_eq!(v.urgency, Some(Urgency::Hole));
    }

    /// C14: one request per resource, whatever lists it appears in.
    #[test]
    fn requests_are_deduplicated_by_resource() {
        let mut r = Rig::interactive();
        let a = t(3, 3, 3);
        let need = NeedSet {
            drawn: vec![(a, need(a)), (a, need(a).with_sse_excess(4.0))],
            blockers: vec![(a, need(a))],
            ancestors: vec![(a, need(a))],
            ring: vec![],
        };
        let d = r.step(&need);
        assert_eq!(d.start.len(), 1, "{RED}");
        assert_eq!(
            r.b.resource(mesh(a)).and_then(|v| v.urgency),
            Some(Urgency::Hole)
        );
    }

    /// C14, S2: a resource loading is not started again.
    #[test]
    fn a_resource_never_has_two_loads_in_flight() {
        let mut c = Rig::interactive_config();
        c.load_timeout = Micros::from_secs(60);
        let mut r = Rig::new(c);
        let a = t(3, 3, 3);
        let need = drawn(&[a]);
        let ds = r.idle_for(&need, Micros::from_secs(5));
        let starts: usize = ds.iter().map(|d| d.start.len()).sum();
        assert_eq!(starts, 1, "{RED}");
        assert!(ds.iter().all(|d| d.stats.duplicates_in_flight == 0));
    }

    /// C16: network loads never exceed the class quota.
    #[test]
    fn network_loads_never_exceed_the_class_quota() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 3;
        let mut r = Rig::new(c);
        let tiles: Vec<_> = (0..8)
            .flat_map(|x| (0..8).map(move |y| t(3, x, y)))
            .collect();
        let ds = r.idle_for(&drawn(&tiles), Micros::from_secs(1));
        assert_eq!(ds[0].start.len(), 3, "{RED}");
        assert!(ds
            .iter()
            .all(|d| d.stats.in_flight[Class::TerrainMesh] <= 3));
    }

    /// C16: a 3D Tiles fly-over cannot starve the ground.
    #[test]
    fn three_d_tiles_cannot_starve_the_ground() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 2;
        c.max_in_flight[Class::Tiles3dMesh] = 2;
        let mut r = Rig::new(c);
        let mut need = NeedSet::new();
        for x in 0..10 {
            let id = t(4, x, 0);
            let mut n = need_of(id).with_sse_excess(10.0);
            n.class = Class::Tiles3dMesh;
            need.drawn.push((id, n));
            let g = t(4, x, 1);
            need.drawn.push((g, need_of(g).with_sse_excess(0.0)));
        }
        let d = r.step(&need);
        let terrain = d
            .start
            .iter()
            .filter(|s| s.class == Class::TerrainMesh)
            .count();
        let tiles = d
            .start
            .iter()
            .filter(|s| s.class == Class::Tiles3dMesh)
            .count();
        assert_eq!((terrain, tiles), (2, 2), "{RED}");
    }

    /// I7, C15: the wish list actually scheduled is bounded.
    #[test]
    fn the_wish_list_is_bounded_by_the_request_heap() {
        let mut c = Rig::interactive_config();
        c.request_heap = 64;
        let mut r = Rig::new(c);
        let tiles: Vec<_> = (0..64)
            .flat_map(|x| (0..64).map(move |y| t(6, x, y)))
            .collect();
        let d = r.step(&drawn(&tiles));
        assert!(d.stats.heap_len > 0, "{RED}");
        assert!(d.stats.heap_len <= 64);
    }

    /// I5, C13: a queued request that left the need set is dropped in the
    /// same step: it never starts.
    #[test]
    fn a_queued_request_that_left_the_need_set_never_starts() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let (a, b, z) = (t(3, 0, 0), t(3, 1, 0), t(3, 7, 7));
        let mut first = drawn(&[a, b]);
        first.drawn[0].1.sse_excess = 5.0;
        let d = r.step(&first);
        assert_eq!(d.start.first().map(|s| s.resource), Some(mesh(a)), "{RED}");
        let mut later = Vec::new();
        for _ in 0..60 {
            let d = r.step(&drawn(&[z]));
            r.complete(&d);
            later.push(d);
        }
        assert!(!started(&later, mesh(b)));
        assert!(started(&later, mesh(z)));
    }

    /// C15: an urgent request takes a slot from in-flight preload rather
    /// than wait for it.
    #[test]
    fn an_urgent_request_preempts_in_flight_preload_rather_than_wait() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 2;
        let mut r = Rig::new(c);
        let (g1, g2, h) = (t(3, 0, 0), t(3, 1, 0), t(3, 5, 5));
        let ring = NeedSet {
            ring: vec![(g1, need(g1)), (g2, need(g2))],
            ..NeedSet::new()
        };
        let d = r.step(&ring);
        assert_eq!(d.start.len(), 2, "{RED}");
        let mut urgent = ring.clone();
        urgent.drawn.push((h, need(h)));
        let d = r.step(&urgent);
        assert!(d.start.iter().any(|s| s.resource == mesh(h)));
        assert_eq!(d.cancel.len(), 1);
        assert!(d.stats.in_flight[Class::TerrainMesh] <= 2);
    }

    /// C27 (liveness): nothing waits while a slot is free.
    #[test]
    fn nothing_waits_while_a_slot_is_free() {
        let mut r = Rig::interactive();
        let tiles = children(t(1, 0, 0));
        let mut need = drawn(&tiles);
        need.ring.push((t(2, 3, 3), need_of(t(2, 3, 3))));
        let d = r.step(&need);
        assert_eq!(d.start.len(), 5, "{RED}");
    }

    /// I14: the drape a drawn tile needs is requested at its own imagery
    /// need, not at its terrain level.
    #[test]
    fn a_drawn_tiles_drape_is_requested_at_its_imagery_need() {
        let mut r = Rig::interactive();
        let a = t(2, 1, 1);
        let d = r.step(&drawn_draped(&[a], 5));
        assert!(
            d.start.iter().any(|s| s.resource == dr(a, 5)),
            "{RED}: {:?}",
            d.start
        );
        assert_eq!(
            r.b.resource(dr(a, 5)).and_then(|v| v.urgency),
            Some(Urgency::Hole),
            "a missing tile's drape is as urgent as its mesh"
        );
    }

    /// C21: the cap holds in the requests: no drape more than three levels
    /// above its terrain.
    #[test]
    fn a_drape_is_never_requested_beyond_the_cap_over_terrain() {
        let mut r = Rig::interactive();
        let a = t(2, 1, 1);
        let d = r.step(&drawn_draped(&[a], 9));
        assert!(d.start.iter().any(|s| s.resource == dr(a, 5)), "{RED}");
        assert!(!d
            .start
            .iter()
            .any(|s| matches!(s.resource, ResourceId::Drape { level, .. } if level > 5)));
    }

    /// C11, §4: a drawn, drawable tile wearing a coarser drape asks for its
    /// own at the Drape tier.
    #[test]
    fn a_drawable_tile_wearing_a_coarser_drape_asks_at_the_drape_tier() {
        let mut r = Rig::interactive();
        let a = t(2, 1, 1);
        r.settle(&drawn_draped(&[a], 3), 20);
        r.step(&drawn_draped(&[a], 5));
        assert_eq!(
            r.b.resource(dr(a, 5)).and_then(|v| v.urgency),
            Some(Urgency::Drape),
            "{RED}"
        );
    }
    /// D-A4, I16 by layer: two layers of one tile are two requests, one
    /// per layer, and one draw item wearing both.
    #[test]
    fn two_layers_of_one_tile_are_two_requests_and_one_draw_item() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let need = drawn_layered(&[a], 4, &[0, 1]);
        let d = r.step(&need);
        let mut drapes: Vec<_> = d
            .start
            .iter()
            .filter(|s| s.class == Class::Drape)
            .map(|s| s.resource)
            .collect();
        drapes.sort();
        assert_eq!(drapes, vec![dl(a, 0, 4), dl(a, 1, 4)], "{RED}");
        r.complete(&d);
        let d = r.settle(&need, 20);
        assert_eq!(d.draw.items.len(), 1);
        let worn: Vec<_> = d.draw.items[0].drapes.iter().map(|x| x.resource).collect();
        assert_eq!(worn, vec![dl(a, 0, 4), dl(a, 1, 4)], "in stacking order");
    }
    /// P2.3 (a): a drape change never reloads its mesh.
    #[test]
    fn a_drape_change_never_reloads_its_mesh() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.settle(&drawn_draped(&[a], 3), 20);
        let mut drapes = 0;
        for level in [5, 2, 6] {
            for _ in 0..10 {
                let d = r.step(&drawn_draped(&[a], level));
                assert!(
                    d.start.iter().all(|s| s.resource != mesh(a)),
                    "{RED}: the mesh reloaded for a drape"
                );
                drapes += d.start.len();
                r.complete(&d);
            }
        }
        assert!(drapes > 0, "the drapes were asked for");
    }

    /// P4.1 (c): a ring tile entering the view is asked for as a hole, and
    /// takes a slot from the ring's in-flight work.
    #[test]
    fn a_ring_tile_entering_the_view_is_requested_as_a_hole() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let (g1, g2) = (t(3, 1, 1), t(3, 6, 6));
        let ring = NeedSet {
            ring: vec![(g1, need(g1)), (g2, need(g2))],
            ..NeedSet::new()
        };
        let d = r.step(&ring);
        assert_eq!(
            d.start.iter().map(|s| s.resource).collect::<Vec<_>>(),
            vec![mesh(g1)],
            "{RED}"
        );
        let entered = NeedSet {
            drawn: vec![(g2, need(g2))],
            ring: vec![(g1, need(g1))],
            ..NeedSet::new()
        };
        let d = r.step(&entered);
        assert!(d.start.iter().any(|s| s.resource == mesh(g2)));
        assert!(
            d.cancel.iter().any(|(x, _)| *x == mesh(g1)),
            "the ring gave its slot"
        );
        assert_eq!(
            r.b.resource(mesh(g2)).and_then(|v| v.urgency),
            Some(Urgency::Hole)
        );
    }
}

mod cancellation {
    use super::*;

    /// I5, S2 (M2.1): a still camera cancels nothing, however long the loads.
    #[test]
    fn a_still_camera_cancels_nothing() {
        let mut c = Rig::interactive_config();
        c.load_timeout = Micros::from_secs(60);
        let mut r = Rig::new(c);
        let tiles = children(t(1, 1, 0));
        let need = drawn_draped(&tiles, 3);
        let ds = r.idle_for(&need, Micros::from_secs(10));
        assert!(started(&ds, mesh(tiles[0])), "{RED}");
        assert!(ds.iter().all(|d| d.cancel.is_empty()));
    }

    fn cancel_time(dt: Micros) -> Option<Micros> {
        let mut r = Rig::interactive();
        r.dt = dt;
        let a = t(3, 2, 2);
        r.step(&drawn(&[a]));
        let left = r.now;
        while r.now.since(left) < Micros::from_secs(2) {
            let at = r.now;
            let d = r.step(&NeedSet::new());
            if d.cancel.iter().any(|(x, _)| *x == mesh(a)) {
                return Some(at.since(left));
            }
        }
        None
    }

    /// I5, C13: a load that left the need set is cancelled after the grace
    /// time, not before.
    #[test]
    fn a_load_that_left_the_need_set_is_cancelled_after_the_grace_not_before() {
        let grace = Rig::interactive_config().cancel_grace;
        let at = cancel_time(Micros(16_667)).expect(RED);
        assert!(at >= grace && at < grace.plus(Micros(16_667)), "{at:?}");
    }

    /// C26: the grace is time: 30 and 120 frames per second cancel at the
    /// same simulated time, to one frame.
    #[test]
    fn the_cancel_grace_is_time_not_frames() {
        let slow = cancel_time(Micros(33_333)).expect(RED);
        let fast = cancel_time(Micros(8_333)).expect(RED);
        assert!(slow.0.abs_diff(fast.0) <= 33_333, "{slow:?} vs {fast:?}");
    }

    /// I5, C13: a load whose tile comes back within the grace keeps going.
    #[test]
    fn a_load_whose_tile_returns_within_the_grace_is_kept() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.start.len(), 1, "{RED}");
        let mut ds = r.idle_for(&NeedSet::new(), Micros::from_millis(300));
        ds.extend(r.idle_for(&drawn(&[a]), Micros::from_secs(2)));
        assert!(ds.iter().all(|d| d.cancel.is_empty()));
        assert!(!started(&ds, mesh(a)));
    }

    /// I5: a cancelled load frees its slot in the same step.
    #[test]
    fn a_cancelled_load_frees_its_slot_in_the_same_step() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let (a, b) = (t(3, 0, 0), t(3, 4, 4));
        r.step(&drawn(&[a]));
        let ds = r.idle_for(&drawn(&[b]), Micros::from_secs(1));
        let d = ds.iter().find(|d| !d.cancel.is_empty()).expect(RED);
        assert!(d.start.iter().any(|s| s.resource == mesh(b)));
    }

    /// I13: Batch never cancels.
    #[test]
    fn batch_never_cancels() {
        let mut r = Rig::batch();
        let a = t(3, 2, 2);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.start.len(), 1, "{RED}");
        let ds = r.idle_for(&NeedSet::new(), Micros::from_secs(10));
        assert!(ds.iter().all(|d| d.cancel.is_empty()));
    }

    /// C13, M2.7: a load that never answers is timed out and retried, so a
    /// stalled host holds no slot for ever.
    #[test]
    fn a_load_that_never_answers_is_timed_out_and_retried() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let need = drawn(&[a]);
        let first = r.step(&need).start.first().map(|s| s.gen).expect(RED);
        let ds = r.idle_for(&need, Micros::from_secs(15));
        assert!(ds.iter().any(|d| d.cancel.contains(&(mesh(a), first))));
        let retry = ds
            .iter()
            .flat_map(|d| &d.start)
            .find(|s| s.resource == mesh(a))
            .expect("retried");
        assert_ne!(retry.gen, first);
    }
}

mod generations {
    use super::*;

    /// I5, C25: a stale generation is never delivered.
    #[test]
    fn a_stale_generation_is_never_delivered() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let g1 = r
            .step(&drawn(&[a]))
            .start
            .first()
            .map(|s| s.gen)
            .expect(RED);
        r.idle_for(&NeedSet::new(), Micros::from_secs(1));
        let g2 = r
            .step(&drawn(&[a]))
            .start
            .first()
            .map(|s| s.gen)
            .expect("restarted");
        assert_ne!(g1, g2);
        r.b.on_loaded(mesh(a), g1, est(mesh(a)));
        let d = r.step(&drawn(&[a]));
        assert!(d.deliver.is_empty());
        assert_eq!(d.stats.stale, 1);
        r.b.on_loaded(mesh(a), g2, est(mesh(a)));
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.deliver, vec![(mesh(a), g2)]);
    }

    /// I5, D5: a late result after eviction does not resurrect the tile.
    #[test]
    fn a_late_result_after_eviction_does_not_resurrect_the_tile() {
        let mut r = Rig::new(small(1));
        let (a, b) = (t(3, 0, 0), t(3, 5, 5));
        let g = r
            .step(&drawn(&[a]))
            .start
            .first()
            .map(|s| s.gen)
            .expect(RED);
        r.b.on_loaded(mesh(a), g, est(mesh(a)));
        r.settle(&drawn(&[a]), 10);
        r.settle(&drawn(&[b]), 10);
        assert!(!r.b.residency().drawable(a), "a was evicted");
        r.b.on_loaded(mesh(a), g, est(mesh(a)));
        let d = r.step(&drawn(&[b]));
        assert!(d.deliver.iter().all(|(x, _)| *x != mesh(a)));
        assert!(!matches!(
            r.b.resource(mesh(a)).map(|v| v.state),
            Some(ResourceState::Loaded | ResourceState::Drawable | ResourceState::InTransit)
        ));
    }

    /// C25, §4.5.5: arrivals between two steps apply in a canonical order,
    /// whatever order they came in.
    #[test]
    fn arrivals_apply_in_a_canonical_order() {
        let tiles: Vec<_> = (0..4).map(|x| t(2, x, 0)).collect();
        let need = drawn_draped(&tiles, 4);
        let run = |reverse: bool| {
            let mut r = Rig::interactive();
            let mut digests = Vec::new();
            for _ in 0..30 {
                let d = r.step(&need);
                digests.push(d.digest());
                let mut starts = d.start.clone();
                if reverse {
                    starts.reverse();
                }
                for s in starts {
                    r.b.on_loaded(s.resource, s.gen, est(s.resource));
                }
                let mut ups = d.upload_plan.items.clone();
                if reverse {
                    ups.reverse();
                }
                for u in ups {
                    let g = r.delivered[&u];
                    r.b.on_drawable(u, g, est(u).gpu);
                }
            }
            digests
        };
        let (a, b) = (run(false), run(true));
        assert!(a.iter().any(|d| *d != a[0]), "{RED}: nothing happened");
        assert_eq!(a, b);
    }

    /// C25, §4.5.5: two contradictory arrivals for one load (a result and a
    /// failure) resolve the same whatever order they came in: the result.
    #[test]
    fn contradictory_arrivals_resolve_the_same_whatever_their_order() {
        let run = |failure_first: bool| {
            let mut r = Rig::interactive();
            let a = t(3, 2, 2);
            let s = *r.step(&drawn(&[a])).start.first().expect(RED);
            if failure_first {
                r.b.on_failed(mesh(a), s.gen, Failure::Transient);
                r.b.on_loaded(mesh(a), s.gen, est(mesh(a)));
            } else {
                r.b.on_loaded(mesh(a), s.gen, est(mesh(a)));
                r.b.on_failed(mesh(a), s.gen, Failure::Transient);
            }
            let d = r.step(&drawn(&[a]));
            (d.digest(), r.b.resource(mesh(a)).map(|v| v.state))
        };
        let (a, b) = (run(true), run(false));
        assert_eq!(a, b);
        assert_eq!(a.1, Some(ResourceState::InTransit));
    }

    /// §3: an arrival changes nothing until the next step.
    #[test]
    fn an_arrival_changes_nothing_until_the_next_step() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let d = r.step(&drawn(&[a]));
        r.load(&d);
        let d = r.step(&drawn(&[a]));
        let (_, g) = *d.deliver.first().expect(RED);
        r.b.on_drawable(mesh(a), g, MESH);
        assert!(!r.b.residency().drawable(a));
        r.step(&drawn(&[a]));
        assert!(r.b.residency().drawable(a));
    }
}

mod tiers {
    use super::*;

    /// I14: a tile already on the GPU is never requested again.
    #[test]
    fn a_tile_already_on_the_gpu_is_never_requested_again() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.settle(&drawn(&[a]), 10);
        assert_eq!(
            r.b.resource(mesh(a)).map(|v| v.tier),
            Some(Tier::Gpu),
            "{RED}"
        );
        let ds = r.idle_for(&drawn(&[a]), Micros::from_secs(60));
        assert!(!started(&ds, mesh(a)));
        assert!(ds.iter().all(|d| d.deliver.is_empty()));
    }

    fn evicted_but_decoded() -> (Rig, tuile_budget::TileId) {
        let mut c = small(1);
        c.decoded_target = Bytes::mib(64);
        let mut r = Rig::new(c);
        let (a, b) = (t(3, 0, 0), t(3, 5, 5));
        r.settle(&drawn(&[a]), 10);
        r.settle(&drawn(&[b]), 10);
        (r, a)
    }

    /// I14, C10: evicting from the GPU keeps the decoded copy: a re-view
    /// costs an upload, not a fetch.
    #[test]
    fn a_decoded_tile_is_uploaded_again_not_refetched() {
        let (mut r, a) = evicted_but_decoded();
        assert_eq!(
            r.b.resource(mesh(a)).map(|v| v.tier),
            Some(Tier::Decoded),
            "{RED}"
        );
        let d = r.step(&drawn(&[a]));
        assert!(d.start.iter().all(|s| s.resource != mesh(a)));
        assert!(d.deliver.iter().any(|(x, _)| *x == mesh(a)));
        assert_eq!(d.stats.network_bytes_avoided, MESH);
    }

    /// C10: the decoded tier has its own budget.
    #[test]
    fn the_decoded_tier_stays_within_its_own_budget() {
        let mut c = small(1);
        c.decoded_target = Bytes(MESH.0 * 2);
        let mut r = Rig::new(c);
        for x in 0..6 {
            r.settle(&drawn(&[t(3, x, 0)]), 10);
            assert!(r.b.ledger().decoded <= Bytes(MESH.0 * 2));
        }
        let decoded = (0..6)
            .filter(|&x| {
                r.b.resource(mesh(t(3, x, 0)))
                    .is_some_and(|v| v.decoded > Bytes::ZERO)
            })
            .count();
        assert!(decoded <= 2, "{decoded} decoded copies");
        let first = r.b.resource(mesh(t(3, 0, 0))).map(|v| v.tier);
        assert!(
            matches!(first, None | Some(Tier::Remote)),
            "{RED}: {first:?}"
        );
    }

    /// I15: a stored tile is read locally, without a network slot.
    #[test]
    fn a_stored_tile_is_read_locally_without_a_network_slot() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.store.0.insert(mesh(a));
        let d = r.step(&drawn(&[a]));
        let s = d.start.first().expect(RED);
        assert_eq!(s.source, Source::Local);
        assert_eq!(d.stats.started_network, 0);
        assert_eq!(d.stats.in_flight[Class::TerrainMesh], 0);
        assert_eq!(d.stats.local_reads, 1);
    }

    /// I15: local reads never wait behind network work.
    #[test]
    fn local_reads_never_wait_behind_network_work() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let remote = t(3, 0, 0);
        let stored = [t(3, 1, 0), t(3, 2, 0), t(3, 3, 0)];
        for s in stored {
            r.store.0.insert(mesh(s));
        }
        let mut need = drawn(&[remote]);
        need.drawn[0].1.sse_excess = 8.0;
        for s in stored {
            need.ring.push((s, need_ring(s)));
        }
        let d = r.step(&need);
        let local = d.start.iter().filter(|s| s.source == Source::Local).count();
        assert_eq!(local, 3, "{RED}");
    }

    fn need_ring(id: tuile_budget::TileId) -> tuile_budget::Need {
        need(id)
    }

    /// §4b: a wrong `Stored` hint degrades to the network exactly once.
    #[test]
    fn a_wrong_store_hint_degrades_to_the_network_exactly_once() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.store.0.insert(mesh(a));
        let s = *r.step(&drawn(&[a])).start.first().expect(RED);
        assert_eq!(s.source, Source::Local);
        r.b.on_store_miss(mesh(a), s.gen);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.stats.store_misses, 1);
        let again = d.start.first().expect("refetched");
        assert_eq!(again.source, Source::Network);
        let ds = r.idle_for(&drawn(&[a]), Micros::from_secs(3));
        assert!(
            !started(&ds, mesh(a)),
            "no second local read, no second fetch"
        );
    }

    /// §4b: at equal urgency and score, the cheaper tier goes first.
    #[test]
    fn at_equal_urgency_and_score_the_cheaper_tier_goes_first() {
        let mut r = Rig::interactive();
        let (a, b) = (t(3, 0, 0), t(3, 1, 0));
        r.store.0.insert(mesh(b));
        r.step(&drawn(&[a, b]));
        let pa = r.b.resource(mesh(a)).and_then(|v| v.priority).expect(RED);
        let pb = r.b.resource(mesh(b)).and_then(|v| v.priority).expect(RED);
        assert!(mesh(a) < mesh(b));
        assert!(pb < pa);
    }
}

mod protection {
    use super::*;

    /// I11, C5: a drawn tile is never evicted, however far over budget.
    #[test]
    fn a_drawn_tile_is_never_evicted_even_far_over_budget() {
        let mut r = Rig::new(small(1));
        let tiles: Vec<_> = (0..8).map(|x| t(3, x, 0)).collect();
        let need = drawn(&tiles);
        r.settle(&need, 30);
        assert!(
            tiles.iter().all(|&id| r.b.residency().drawable(id)),
            "{RED}"
        );
        for d in r.idle_for(&need, Micros::from_secs(5)) {
            assert!(d.evict.is_empty());
        }
    }

    /// I11, C6: the ancestor a missing tile stands on is never evicted.
    #[test]
    fn a_drawn_tiles_fallback_chain_is_never_evicted() {
        let mut r = Rig::new(small(1));
        let root = t(0, 0, 0);
        let stray = t(3, 7, 7);
        r.settle(&drawn(&[root]), 10);
        r.settle(&drawn(&[root, stray]), 10);
        let kids = children(root);
        let ds = r.idle_for(&drawn(&kids), Micros::from_secs(3));
        assert!(ds.iter().any(|d| d.evict.contains(&mesh(stray))), "{RED}");
        assert!(ds.iter().all(|d| !d.evict.contains(&mesh(root))));
        let last = ds.last().expect("steps");
        assert_eq!(
            last.draw.get(mesh(root)).map(|i| i.role),
            Some(Role::Fallback)
        );
    }

    /// I11, C6: the ancestor's drape a tile wears through a UV transform is
    /// never evicted.
    #[test]
    fn the_ancestor_drape_a_tile_wears_is_never_evicted() {
        let mut c = Rig::interactive_config();
        c.target[Class::Drape] = Bytes::ZERO;
        c.overflow[Class::Drape] = Bytes::ZERO;
        let mut r = Rig::new(c);
        let root = t(0, 0, 0);
        r.settle(&drawn_draped(&[root], 2), 10);
        let kid = children(root)[0];
        // The child's mesh arrives, its own drape never does.
        let mut ds = Vec::new();
        for _ in 0..30 {
            let d = r.step(&drawn_draped(&[kid], 3));
            for s in &d.start {
                if matches!(s.resource, ResourceId::Mesh(_)) {
                    r.b.on_loaded(s.resource, s.gen, est(s.resource));
                }
            }
            r.upload(&d);
            ds.push(d);
        }
        let last = ds.last().expect("steps");
        let worn = last.draw.get(mesh(kid)).and_then(base_drape).expect(RED);
        assert_eq!(worn.resource, dr(root, 2));
        assert!(ds.iter().all(|d| !d.evict.contains(&dr(root, 2))));
    }

    /// I11, C7: the pinned floor is never evicted, needed or not.
    #[test]
    fn the_pinned_floor_is_never_evicted() {
        let mut c = small(0);
        c.pinned_level = Some(0);
        let mut r = Rig::new(c);
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        assert!(r.b.residency().drawable(root), "{RED}");
        for x in 0..8 {
            let d = r.settle(&drawn(&[t(3, x, 0)]), 10);
            assert!(!d.evict.contains(&mesh(root)));
        }
        assert!(r.b.residency().drawable(root));
    }

    /// I11, C5: a resource in transit to the GPU is never evicted.
    #[test]
    fn a_resource_in_transit_is_never_evicted() {
        let mut r = Rig::new(small(0));
        let a = t(3, 2, 2);
        let d = r.step(&drawn(&[a]));
        r.load(&d);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.deliver.len(), 1, "{RED}");
        let ds = r.idle_for(&NeedSet::new(), Micros::from_millis(200));
        assert!(ds.iter().all(|d| !d.evict.contains(&mesh(a))));
        assert_eq!(
            r.b.resource(mesh(a)).and_then(|v| v.protected),
            Some(Protection::InTransit)
        );
    }

    /// C8: eviction takes the longest-unneeded first, then the deepest.
    #[test]
    fn eviction_takes_the_longest_unneeded_first_then_the_deepest() {
        let mut r = Rig::new(small(100));
        let (x, shallow, deep) = (t(3, 0, 0), t(2, 3, 3), t(4, 15, 15));
        r.settle(&drawn(&[x]), 10);
        r.settle(&drawn(&[shallow, deep]), 10);
        r.idle_for(&drawn(&[t(3, 4, 0)]), Micros::from_secs(1));
        // Shrink the target to nothing: everything unprotected goes, in order.
        r.b.retarget(Bytes::ZERO);
        let d = r.step(&drawn(&[t(3, 4, 0)]));
        let order: Vec<_> = d
            .evict
            .iter()
            .copied()
            .filter(|e| *e != mesh(t(3, 4, 0)))
            .collect();
        assert_eq!(order, vec![mesh(x), mesh(deep), mesh(shallow)], "{RED}");
    }

    /// C8, S3 (M3.3): ancestors outlive their descendants.
    #[test]
    fn ancestors_outlive_their_descendants() {
        let mut c = small(3);
        c.watermarks.low_percent = 100;
        let mut r = Rig::new(c);
        let p = t(2, 1, 1);
        let c = children(p)[0];
        r.settle(&drawn(&[p, c]), 10);
        let other = [t(3, 7, 7), t(3, 7, 6)];
        r.settle(&drawn(&other), 10);
        assert!(!r.b.residency().drawable(c), "{RED}");
        assert!(r.b.residency().drawable(p));
    }

    /// C3: a sweep starts above the high watermark and trims to the low one.
    #[test]
    fn eviction_trims_to_the_low_watermark_and_waits_for_the_high_one() {
        let mut c = small(10);
        c.watermarks.high_percent = 100;
        c.watermarks.low_percent = 80;
        let mut r = Rig::new(c);
        let row = |y: u64, n: u64| (0..n).map(|x| t(4, x, y)).collect::<Vec<_>>();
        r.settle(&drawn(&row(0, 9)), 30);
        r.settle(&drawn(&row(1, 2)), 30);
        let held = Class::TerrainMesh;
        assert_eq!(r.b.ledger().gpu[held], Bytes(MESH.0 * 8), "{RED}");
        r.settle(&drawn(&row(2, 2)), 30);
        assert_eq!(
            r.b.ledger().gpu[held],
            Bytes(MESH.0 * 10),
            "no sweep at the high mark"
        );
    }

    /// C9, I2: eviction outpaces admission: after every step, what is not
    /// protected fits in the target.
    #[test]
    fn eviction_outpaces_admission() {
        let mut r = Rig::new(small(4));
        let mut moved = 0;
        for x in 0..32 {
            for _ in 0..5 {
                let d = r.step(&drawn(&[t(5, x, 3), t(5, x, 4)]));
                let unprotected =
                    d.stats.gpu[Class::TerrainMesh].minus(d.stats.protected[Class::TerrainMesh]);
                assert!(unprotected <= d.stats.target[Class::TerrainMesh]);
                moved += d.start.len();
                r.complete(&d);
            }
        }
        assert!(moved > 0, "{RED}");
    }

    /// C9: needed-but-undrawn resources go only after unneeded ones.
    #[test]
    fn a_needed_blocker_is_evicted_only_after_unneeded_resources() {
        let mut c = small(3);
        c.watermarks.low_percent = 100;
        let mut r = Rig::new(c);
        let (p, u) = (t(2, 0, 0), t(3, 7, 7));
        let b = children(p)[0];
        r.settle(&drawn(&[u]), 10);
        r.settle(&drawn(&[p, b]), 10);
        let need = NeedSet {
            drawn: vec![(p, need(p)), (t(3, 6, 6), need(t(3, 6, 6)))],
            blockers: vec![(b, need(b))],
            ..NeedSet::new()
        };
        let d = r.settle(&need, 10);
        let _ = d;
        assert!(!r.b.residency().drawable(u), "{RED}");
        assert!(r.b.residency().drawable(b));
    }

    /// C17: a failed load releases every pin and slot.
    #[test]
    fn a_failed_load_releases_every_pin() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let s = *r.step(&drawn(&[a])).start.first().expect(RED);
        r.b.on_failed(mesh(a), s.gen, Failure::Transient);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.stats.in_flight[Class::TerrainMesh], 0);
        let v = r.b.resource(mesh(a)).expect("tracked while needed");
        assert!(matches!(v.state, ResourceState::Failed { retries: 1, .. }));
        assert_eq!(v.protected, None);
    }
}

mod pressure {
    use super::*;

    fn over_budget(dt: Micros) -> u32 {
        let mut c = small(1);
        c.relax.up_per_second = 1_000;
        let mut r = Rig::new(c);
        r.dt = dt;
        let tiles: Vec<_> = (0..4).map(|x| t(3, x, 0)).collect();
        let before = r.settle(&drawn(&tiles), 60).sse_scale_milli;
        let ds = r.idle_for(&drawn(&tiles), Micros::from_secs(1));
        ds.last()
            .map(|d| d.sse_scale_milli)
            .unwrap_or(0)
            .saturating_sub(before)
    }

    /// I2: the protected set may overflow; nothing else may.
    #[test]
    fn the_protected_set_may_overflow_but_nothing_else_may() {
        let mut r = Rig::new(small(2));
        let tiles: Vec<_> = (0..8).map(|x| t(3, x, 0)).collect();
        let mut ds = Vec::new();
        for _ in 0..40 {
            let d = r.step(&drawn(&tiles));
            r.complete(&d);
            ds.push(d);
        }
        let last = ds.last().expect("steps");
        assert_eq!(
            last.stats.gpu[Class::TerrainMesh],
            Bytes(MESH.0 * 8),
            "{RED}"
        );
        for d in &ds {
            let s = &d.stats;
            assert!(
                s.gpu[Class::TerrainMesh].minus(s.protected[Class::TerrainMesh])
                    <= s.target[Class::TerrainMesh]
            );
        }
    }

    /// C26, I8: relaxation rises with time, not frames: 30 and 120 frames
    /// per second reach the same scale after one second.
    #[test]
    fn relaxation_rises_with_time_not_frames() {
        let slow = over_budget(Micros(33_333));
        let fast = over_budget(Micros(8_333));
        assert!(slow > 900, "{RED}: {slow}");
        assert!(slow.abs_diff(fast) <= 40, "{slow} vs {fast}");
    }

    /// C3, B4: relaxation decays back to 1 under the low watermark, holds in
    /// the band, and never ratchets.
    #[test]
    fn relaxation_decays_back_to_one_under_target_and_holds_in_the_band() {
        let mut r = Rig::new(small(1));
        let tiles: Vec<_> = (0..4).map(|x| t(3, x, 0)).collect();
        r.settle(&drawn(&tiles), 60);
        let up = r.idle_for(&drawn(&tiles), Micros::from_secs(2));
        let peak = up.last().map(|d| d.sse_scale_milli).unwrap_or(0);
        assert!(peak > 1_000, "{RED}");
        r.b.retarget(Bytes::mib(512));
        let down = r.idle_for(&NeedSet::new(), Micros::from_secs(30));
        assert_eq!(down.last().map(|d| d.sse_scale_milli), Some(1_000));
        assert!(down
            .windows(2)
            .all(|w| w[1].sse_scale_milli <= w[0].sse_scale_milli));
    }

    /// I8, I1: a memory warning brings bytes under the new target within a
    /// step, without a hole.
    #[test]
    fn a_memory_warning_trims_to_the_new_target_without_a_hole() {
        let mut r = Rig::new(small(16));
        let old: Vec<_> = (0..8).map(|x| t(4, x, 0)).collect();
        let new: Vec<_> = (0..8).map(|x| t(4, x, 9)).collect();
        r.settle(&drawn(&old), 30);
        r.settle(&drawn(&new), 30);
        r.b.set_pressure(Pressure::Warning);
        let d = r.step(&drawn(&new));
        assert_eq!(
            d.stats.target[Class::TerrainMesh],
            Bytes(MESH.0 * 8),
            "{RED}"
        );
        assert!(d.stats.gpu[Class::TerrainMesh] <= Bytes(MESH.0 * 8));
        assert_eq!(d.stats.lost, 0);
        assert_eq!(d.stats.exact, 8);
    }

    /// I8: in the background only the floor and the drawn set stay.
    #[test]
    fn background_keeps_only_the_floor_and_the_drawn_set() {
        let mut c = small(64);
        c.pinned_level = Some(0);
        let mut r = Rig::new(c);
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let old: Vec<_> = (0..8).map(|x| t(4, x, 0)).collect();
        r.settle(&drawn(&old), 30);
        let now = [t(4, 3, 3)];
        r.settle(&drawn(&now), 10);
        r.b.set_pressure(Pressure::Background);
        r.step(&drawn(&now));
        assert!(r.b.residency().drawable(root), "{RED}");
        assert!(r.b.residency().drawable(now[0]));
        assert!(old.iter().all(|&id| !r.b.residency().drawable(id)));
    }

    /// I8: thermal Serious halves uploads and quotas and coarsens the LOD.
    #[test]
    fn thermal_serious_slows_uploads_and_coarsens_the_lod() {
        let mut c = Rig::interactive_config();
        c.upload_bytes_per_frame = Bytes(MESH.0 * 4);
        let quota = c.max_in_flight[Class::TerrainMesh];
        let mut r = Rig::new(c);
        r.b.set_thermal(Thermal::Serious);
        let tiles: Vec<_> = (0..8)
            .flat_map(|x| (0..8).map(move |y| t(3, x, y)))
            .collect();
        let need = drawn(&tiles);
        let d = r.step(&need);
        assert_eq!(d.stats.in_flight[Class::TerrainMesh], quota / 2, "{RED}");
        assert!(d.sse_scale_milli >= 1_500);
        r.load(&d);
        let d = r.step(&need);
        assert!(d.upload_plan.bytes <= Bytes(MESH.0 * 2));
        assert_eq!(d.upload_plan.items.len(), 2);
    }

    /// C4, I8: the host re-derives the budget at runtime, platform-neutral,
    /// without a hole.
    #[test]
    fn a_runtime_retarget_shrinks_targets_without_a_hole() {
        let mut r = Rig::interactive();
        let tiles: Vec<_> = (0..4).map(|x| t(3, x, 0)).collect();
        r.settle(&drawn(&tiles), 30);
        r.b.retarget(Bytes::mib(64));
        let d = r.step(&drawn(&tiles));
        let sum = d
            .stats
            .target
            .iter()
            .fold(Bytes::ZERO, |a, (_, b)| a.plus(b));
        assert!(sum <= Bytes::mib(64) && sum > Bytes::ZERO, "{RED}");
        assert_eq!(d.stats.lost, 0);
        assert_eq!(d.stats.exact, 4);
    }

    /// I2: Batch never relaxes, whatever the pressure.
    #[test]
    fn batch_never_relaxes() {
        let mut r = Rig::batch();
        r.b.set_pressure(Pressure::Critical);
        r.b.set_thermal(Thermal::Critical);
        let tiles: Vec<_> = (0..8).map(|x| t(3, x, 0)).collect();
        let ds: Vec<_> = (0..30)
            .map(|_| {
                let d = r.step(&drawn(&tiles));
                r.complete(&d);
                d
            })
            .collect();
        assert!(ds.iter().any(|d| d.frame_ready), "{RED}");
        assert!(ds
            .iter()
            .all(|d| d.sse_scale_milli == 1_000 && d.evict.is_empty()));
    }
}

mod upload {
    use super::*;

    fn loaded(c: BudgetConfig, need: &NeedSet) -> (Rig, tuile_budget::Decisions) {
        let mut r = Rig::new(c);
        let d = r.step(need);
        r.load(&d);
        let d = r.step(need);
        (r, d)
    }

    /// C23: the plan never exceeds its bytes, except its first item.
    #[test]
    fn the_upload_plan_never_exceeds_its_bytes_except_its_first_item() {
        let mut c = Rig::interactive_config();
        c.upload_bytes_per_frame = Bytes(MESH.0 * 5 / 2);
        let tiles: Vec<_> = (0..10).map(|x| t(4, x, 0)).collect();
        let (_, d) = loaded(c.clone(), &drawn(&tiles));
        assert_eq!(d.upload_plan.items.len(), 2, "{RED}");
        assert!(d.upload_plan.bytes <= c.upload_bytes_per_frame);
        c.upload_bytes_per_frame = Bytes(1);
        let (_, d) = loaded(c, &drawn(&tiles));
        assert_eq!(d.upload_plan.items.len(), 1, "the first item always goes");
    }

    /// C23, S2 (M2.6): the plan follows priority, not arrival.
    #[test]
    fn the_upload_plan_follows_priority_not_arrival() {
        let mut c = Rig::interactive_config();
        c.upload_bytes_per_frame = Bytes(1);
        let (a, h) = (t(3, 0, 0), t(3, 5, 5));
        let mut r = Rig::new(c);
        let pre = NeedSet {
            ancestors: vec![(a, need(a))],
            ..NeedSet::new()
        };
        let d = r.step(&pre);
        r.load(&d);
        let mut both = pre.clone();
        both.drawn.push((h, need(h)));
        let d = r.step(&both);
        r.load(&d);
        let d = r.step(&both);
        assert_eq!(d.upload_plan.items.first(), Some(&mesh(h)), "{RED}");
    }

    /// I5, S2: content no longer needed is dropped before upload, never
    /// uploaded ahead of needed work.
    #[test]
    fn content_no_longer_needed_is_dropped_before_upload() {
        let mut c = Rig::interactive_config();
        c.upload_bytes_per_frame = Bytes(1);
        let mut r = Rig::new(c);
        let (x, h) = (t(3, 0, 0), t(3, 5, 5));
        let d = r.step(&drawn(&[x]));
        r.load(&d);
        let d = r.step(&drawn(&[x]));
        assert_eq!(d.deliver.len(), 1, "{RED}");
        let ds = r.idle_for(&drawn(&[h]), Micros::from_secs(1));
        let cancelled_at = ds
            .iter()
            .position(|d| d.cancel.iter().any(|(r, _)| *r == mesh(x)))
            .expect("dropped from transit");
        assert!(ds[cancelled_at..]
            .iter()
            .all(|d| !d.upload_plan.items.contains(&mesh(x))));
        assert_eq!(r.b.resource(mesh(x)).map(|v| v.tier), Some(Tier::Decoded));
    }

    /// I9, C19: the children of a refining parent form one swap group.
    #[test]
    fn siblings_of_a_refining_parent_form_one_swap_group() {
        let p = t(2, 1, 1);
        let kids = children(p);
        let need = NeedSet {
            drawn: vec![(p, need(p))],
            blockers: kids.iter().map(|&k| (k, need(k))).collect(),
            ..NeedSet::new()
        };
        let (_, d) = loaded(Rig::interactive_config(), &need);
        let mut want: Vec<_> = kids.iter().map(|&k| mesh(k)).collect();
        want.sort();
        assert!(
            d.upload_plan.groups.iter().any(|g| g.members == want),
            "{RED}"
        );
    }

    /// I9, C19: a mesh and its drape form one swap group.
    #[test]
    fn a_mesh_and_its_drape_form_one_swap_group() {
        let a = t(3, 2, 2);
        let (_, d) = loaded(Rig::interactive_config(), &drawn_draped(&[a], 4));
        let want = vec![mesh(a), dr(a, 4)];
        assert!(
            d.upload_plan.groups.iter().any(|g| g.members == want),
            "{RED}"
        );
    }

    /// I7: delivery stays within the credit, except its first item.
    #[test]
    fn delivery_stays_within_the_credit() {
        let mut c = Rig::interactive_config();
        c.delivery_credit = Bytes(MESH.0 * 3);
        c.upload_bytes_per_frame = Bytes(1);
        let tiles: Vec<_> = (0..10).map(|x| t(4, x, 0)).collect();
        let mut r = Rig::new(c);
        let need = drawn(&tiles);
        let d = r.step(&need);
        r.load(&d);
        let d = r.step(&need);
        assert_eq!(d.deliver.len(), 3, "{RED}");
        assert!(r.b.ledger().in_transit <= Bytes(MESH.0 * 3));
    }
    /// I1 (D-A1): the floor's mesh and its drape form one swap group: the
    /// mesh, loaded first, waits for its drape, and both are delivered and
    /// uploaded together.
    #[test]
    fn a_floor_mesh_and_its_drape_upload_as_one_group() {
        let mut c = Rig::interactive_config();
        c.pinned_level = Some(0);
        let mut r = Rig::new(c);
        let root = t(0, 0, 0);
        let need = drawn_draped(&[root], 0);
        r.step(&need);
        r.load_where(|x| x == mesh(root));
        let d = r.step(&need);
        assert!(
            d.deliver.is_empty(),
            "{RED}: the floor mesh delivered without its drape"
        );
        r.load_where(|x| x == dr(root, 0));
        let d = r.step(&need);
        let mut both: Vec<_> = d.deliver.iter().map(|x| x.0).collect();
        both.sort();
        assert_eq!(both, vec![mesh(root), dr(root, 0)]);
        assert!(d
            .upload_plan
            .groups
            .iter()
            .any(|g| g.members == vec![mesh(root), dr(root, 0)]));
    }
}

mod draw {
    use super::*;

    fn need_of_tile(id: tuile_budget::TileId) -> tuile_budget::Need {
        need(id)
    }

    /// I1: a drawable wanted tile is drawn as itself.
    #[test]
    fn a_drawable_wanted_tile_is_drawn_exact() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let d = r.settle(&drawn(&[a]), 10);
        let item = d.draw.get(mesh(a)).expect(RED);
        assert_eq!(item.role, Role::Exact);
        assert_eq!(item.serves, vec![a]);
        assert_eq!(d.stats.exact, 1);
    }

    /// I1, C6: a wanted tile that is not drawable stands on its nearest
    /// drawable ancestor.
    #[test]
    fn a_missing_tile_stands_on_its_nearest_drawable_ancestor() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let child = children(root)[0];
        let grandchild = children(child)[0];
        // The traversal lists the chain it walked down, as preload.
        let mut need = drawn(&[grandchild]);
        need.ancestors.push((child, need_of_tile(child)));
        let d = r.step(&need);
        let item = d.draw.get(mesh(root)).expect(RED);
        assert_eq!(item.role, Role::Fallback);
        assert_eq!(item.serves, vec![grandchild]);
        assert_eq!((d.stats.fallback, d.stats.lost), (1, 0));
    }

    /// I9 (coarsening), C19: on a zoom-out the children stay drawn until the
    /// parent is drawable, then go in the same step.
    #[test]
    fn on_zoom_out_the_children_stay_until_the_parent_is_drawable() {
        let mut r = Rig::interactive();
        let p = t(2, 1, 1);
        let kids = children(p);
        r.settle(&drawn(&kids), 20);
        let d = r.step(&drawn(&[p]));
        for k in kids {
            assert_eq!(
                d.draw.get(mesh(k)).map(|i| i.role),
                Some(Role::Held),
                "{RED}"
            );
        }
        assert_eq!(d.stats.held, 4);
        let d = r.settle(&drawn(&[p]), 10);
        assert_eq!(d.draw.get(mesh(p)).map(|i| i.role), Some(Role::Exact));
        assert!(kids.iter().all(|&k| !d.draw.draws(k)));
    }

    /// I9 (refining), C19: the parent stays drawn while any wanted child is
    /// missing.
    #[test]
    fn the_parent_stays_drawn_while_any_wanted_child_is_missing() {
        let mut r = Rig::interactive();
        let p = t(2, 1, 1);
        let kids = children(p);
        r.settle(&drawn(&[p]), 10);
        r.settle(&drawn(&[p, kids[0], kids[1], kids[2]]), 10);
        let d = r.step(&drawn(&kids));
        let item = d.draw.get(mesh(p)).expect(RED);
        assert_eq!(item.role, Role::Fallback);
        assert_eq!(item.serves, vec![kids[3]]);
        let d = r.settle(&drawn(&kids), 10);
        assert!(!d.draw.draws(p));
    }

    /// I9, C20: the old drape is worn until the new one is drawable.
    #[test]
    fn the_old_drape_is_worn_until_the_new_one_is_drawable() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.settle(&drawn_draped(&[a], 3), 10);
        let d = r.step(&drawn_draped(&[a], 5));
        let worn = d.draw.get(mesh(a)).and_then(base_drape).expect(RED);
        assert_eq!(worn.resource, dr(a, 3));
        let d = r.settle(&drawn_draped(&[a], 5), 10);
        let worn = d.draw.get(mesh(a)).and_then(base_drape).expect("worn");
        assert_eq!(worn.resource, dr(a, 5));
    }

    /// I9, C20: on a zoom-out a finer drape already worn is kept until the
    /// needed one is drawable: never blurrier than necessary.
    #[test]
    fn a_finer_drape_is_kept_until_the_coarser_one_is_drawable() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        let a = children(root)[1];
        r.settle(&drawn_draped(&[root], 1), 10);
        r.settle(&drawn_draped(&[a], 4), 10);
        let d = r.step(&drawn_draped(&[a], 2));
        let worn = d.draw.get(mesh(a)).and_then(base_drape).expect(RED);
        assert_eq!(worn.resource, dr(a, 4));
    }

    /// I1: a wanted tile with nothing to draw is counted lost.
    #[test]
    fn a_tile_with_no_drawable_cover_is_counted_lost() {
        let mut r = Rig::interactive();
        let d = r.step(&drawn(&[t(3, 2, 2)]));
        assert_eq!(d.stats.lost, 1, "{RED}");
        assert!(d.draw.items.is_empty());
    }

    /// C24: each mesh is drawn once, in one role; a fallback never serves a
    /// tile that is drawn as itself.
    #[test]
    fn each_mesh_is_drawn_once_in_one_role() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let kids = children(root);
        r.settle(&drawn(&[root, kids[0]]), 10);
        let d = r.step(&drawn(&kids));
        assert!(!d.draw.items.is_empty(), "{RED}");
        let meshes: Vec<_> = d.draw.items.iter().map(|i| i.mesh).collect();
        let mut sorted = meshes.clone();
        sorted.dedup();
        assert_eq!(meshes, sorted);
        for i in d.draw.items.iter().filter(|i| i.role == Role::Fallback) {
            assert!(i.serves.iter().all(|s| !d.draw.draws(*s)));
        }
    }

    /// I10, S3 (M3.1): a drawn tile's drape follows its need, both ways.
    #[test]
    fn a_drawn_tiles_drape_follows_its_need_both_ways() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        for level in [3, 6, 2, 5] {
            let d = r.settle(&drawn_draped(&[a], level), 20);
            let worn = d.draw.get(mesh(a)).and_then(base_drape).expect(RED);
            assert!(
                worn.level.abs_diff(level) <= 1,
                "need {level}, worn {}",
                worn.level
            );
            assert_eq!(d.stats.underdraped, 0);
        }
    }

    /// P5.1: a need widened by a resize or a rotation covers its new tiles
    /// in the step it widens, by a fallback on the nearest drawable
    /// ancestor, while what was drawn stays drawn.
    #[test]
    fn a_widened_need_covers_its_new_tiles_by_fallback_in_the_same_step() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        let kids = children(root);
        let mut narrow = drawn(&[kids[0]]);
        narrow.ancestors.push((root, need_of_tile(root)));
        r.settle(&narrow, 20);
        let mut wide = drawn(&kids);
        wide.ancestors.push((root, need_of_tile(root)));
        let d = r.step(&wide);
        assert_eq!(d.draw.get(mesh(kids[0])).map(|i| i.role), Some(Role::Exact));
        let cover = d.draw.get(mesh(root)).expect(RED);
        assert_eq!(cover.role, Role::Fallback);
        let mut new_tiles = kids[1..].to_vec();
        new_tiles.sort();
        assert_eq!(cover.serves, new_tiles);
        assert_eq!((d.stats.lost, d.stats.fallback), (0, 3));
    }
    /// D-A4, M36: each layer falls back on its own: a tile whose base
    /// drape is drawable wears it, and wears its overlay through the
    /// nearest ancestor that has the overlay — never another layer's drape.
    #[test]
    fn each_layer_falls_back_on_its_own() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        let a = children(root)[3];
        // The root wears both layers; `a` then gets its base drape only.
        let mut both = drawn_layered(&[root], 0, &[0, 1]);
        r.settle(&both, 20);
        both = drawn_layered(&[a], 1, &[0]);
        both.ancestors.push((root, layered(root, 0, &[0, 1])));
        r.settle(&both, 20);
        // Now `a` wants both layers at level 1: the overlay's own is not
        // loaded yet.
        let mut need = drawn_layered(&[a], 1, &[0, 1]);
        need.ancestors.push((root, layered(root, 0, &[0, 1])));
        let d = r.step(&need);
        let item = d.draw.get(mesh(a)).expect(RED);
        assert_eq!(item.role, Role::Exact);
        let base = item.drape(LayerId(0)).expect("the base layer worn");
        assert_eq!(base.resource, dl(a, 0, 1));
        let overlay = item.drape(LayerId(1)).expect("the overlay worn");
        assert_eq!(
            overlay.resource,
            dl(root, 1, 0),
            "the overlay's own ancestor"
        );
        assert_ne!(overlay.uv, tuile_budget::UvTransform::IDENTITY);
        assert_eq!(item.drapes.len(), 2, "one drape per layer");
    }

    /// I1 (D-A1), D-A4: on a layer switch the old layer stays worn until
    /// the new one is drawable, then the new one replaces it in one step.
    #[test]
    fn a_layer_switch_keeps_the_old_layer_worn_until_the_new_one_is_drawable() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        r.settle(&drawn_layered(&[a], 3, &[0]), 20);
        let switched = drawn_layered(&[a], 3, &[1]);
        let d = r.step(&switched);
        let item = d.draw.get(mesh(a)).expect(RED);
        assert_eq!(
            item.drapes.iter().map(|x| x.resource).collect::<Vec<_>>(),
            vec![dl(a, 0, 3)],
            "the old layer stays worn"
        );
        assert!(d.start.iter().any(|s| s.resource == dl(a, 1, 3)));
        let d = r.settle(&switched, 20);
        let item = d.draw.get(mesh(a)).expect("drawn");
        assert_eq!(
            item.drapes.iter().map(|x| x.resource).collect::<Vec<_>>(),
            vec![dl(a, 1, 3)],
            "the new layer replaced it"
        );
    }
    /// I1 (D-A1), P1.1, M25: the floor's mesh is drawn only with its drape:
    /// a floor mesh whose drape is not drawable is not drawn bare, and the
    /// floor is in only once both are.
    #[test]
    fn the_floor_is_drawn_only_with_its_drape() {
        let mut c = Rig::interactive_config();
        c.pinned_level = Some(0);
        let mut r = Rig::new(c);
        let root = t(0, 0, 0);
        let need = drawn_draped(&[root], 0);
        r.step(&need);
        r.load_where(|x| x == mesh(root));
        for _ in 0..4 {
            let d = r.step(&need);
            r.upload(&d);
        }
        let d = r.step(&need);
        assert!(!d.draw.draws(root), "{RED}: the floor mesh drawn bare");
        assert_eq!(d.stats.bare, 0);
        assert!(!d.stats.floor_in);
        r.load_where(|x| x == dr(root, 0));
        let d = r.settle(&need, 10);
        let item = d.draw.get(mesh(root)).expect("drawn once dressed");
        assert_eq!(base_drape(item).map(|x| x.resource), Some(dr(root, 0)));
        assert!(d.stats.floor_in);
        assert_eq!(d.stats.bare, 0);
    }

    /// P1.1 (b): no area refines while wanted ground anywhere is lost or
    /// bare: a deep tile whose mesh and drape arrived first is not drawn
    /// while the floor elsewhere is still missing.
    #[test]
    fn nothing_refines_while_wanted_ground_is_lost_or_bare() {
        let mut c = Rig::interactive_config();
        c.pinned_level = Some(1);
        let mut r = Rig::new(c);
        let root = t(0, 0, 0);
        let kids = children(root);
        let mid = children(kids[0])[0];
        let deep = children(mid)[0];
        let need = NeedSet {
            drawn: std::iter::once((deep, draped(deep, 3)))
                .chain(kids[1..].iter().map(|&k| (k, draped(k, 1))))
                .collect(),
            ancestors: vec![
                (root, draped(root, 0)),
                (kids[0], draped(kids[0], 1)),
                (mid, draped(mid, 2)),
            ],
            ..NeedSet::new()
        };
        r.step(&need);
        r.load_where(|x| x.tile() == deep);
        for _ in 0..4 {
            let d = r.step(&need);
            r.upload(&d);
        }
        let d = r.step(&need);
        assert!(d.stats.lost > 0, "{RED}: the floor came in by itself");
        assert!(
            !d.draw.draws(deep),
            "a level-3 tile drawn while {} wanted tiles are lost",
            d.stats.lost
        );
        assert_eq!(d.stats.bare, 0);
    }

    /// P2.1, I1 (D-A1): a refining parent's children appear together, in
    /// one step, each dressed (here in the parent's drape, theirs not yet
    /// loaded), and the parent goes in that step.
    #[test]
    fn refining_children_appear_together_each_with_its_drapes() {
        let mut r = Rig::interactive();
        let p = t(2, 1, 1);
        let kids = children(p);
        r.settle(&drawn_draped(&[p], 2), 20);
        let mut hold = drawn_draped(&[p], 2);
        hold.blockers = kids.iter().map(|&k| (k, draped(k, 3))).collect();
        r.step(&hold);
        r.load_where(|x| matches!(x, ResourceId::Mesh(_)));
        for _ in 0..4 {
            let d = r.step(&hold);
            r.upload(&d);
        }
        let mut refined = drawn_draped(&kids, 3);
        refined.ancestors.push((p, draped(p, 2)));
        let d = r.step(&refined);
        for k in kids {
            let item = d.draw.get(mesh(k)).expect(RED);
            assert_eq!(item.role, Role::Exact);
            let worn = base_drape(item).expect("dressed");
            assert_eq!(worn.resource, dr(p, 2), "in the parent's drape");
        }
        assert!(!d.draw.draws(p), "the parent goes in the same step");
        assert_eq!(d.stats.bare, 0);
    }

    /// P3.1, I1 (D-A1), M35: on a zoom-out the held children go only once
    /// the parent is drawable and dressed: a parent whose mesh arrived
    /// before any drape it could wear does not replace them.
    #[test]
    fn held_children_go_only_once_the_parent_is_drawable_and_dressed() {
        let mut r = Rig::interactive();
        let p = t(2, 1, 1);
        let kids = children(p);
        r.settle(&drawn_draped(&kids, 3), 20);
        let need = drawn_draped(&[p], 2);
        r.step(&need);
        r.load_where(|x| x == mesh(p));
        for _ in 0..4 {
            let d = r.step(&need);
            r.upload(&d);
        }
        let d = r.step(&need);
        assert!(
            r.b.residency().drawable(p),
            "{RED}: the parent's mesh is drawable"
        );
        for k in kids {
            assert_eq!(
                d.draw.get(mesh(k)).map(|i| i.role),
                Some(Role::Held),
                "a held child went before its parent was dressed"
            );
        }
        assert!(!d.draw.draws(p), "the parent drawn bare");
        assert_eq!(d.stats.bare, 0);
        r.load_where(|x| x == dr(p, 2));
        let d = r.settle(&need, 10);
        assert_eq!(d.draw.get(mesh(p)).map(|i| i.role), Some(Role::Exact));
        assert!(kids.iter().all(|&k| !d.draw.draws(k)));
    }
    /// P1.2: a still need never draws coarser than it drew: whatever
    /// arrives, a tile drawn exactly stays drawn with a drape no coarser,
    /// until the need moves.
    #[test]
    fn a_still_need_never_draws_coarser_than_it_drew() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        let kids = children(root);
        let mut need = drawn_draped(&kids, 3);
        need.ancestors.push((root, draped(root, 1)));
        r.settle(&drawn_draped(&[root], 1), 20);
        let mut was: std::collections::BTreeMap<_, u32> = Default::default();
        for k in 0..40 {
            let d = r.step(&need);
            // Arrivals trickle in: one load in two, every other upload.
            if k % 2 == 0 {
                r.load(&d);
            }
            if k % 3 == 0 {
                r.upload(&d);
            }
            for (tile, img) in &was {
                let item = d.draw.get(mesh(*tile)).expect("still drawn");
                assert_eq!(item.role, Role::Exact, "{tile:?} gave way");
                let now = base_drape(item).expect("dressed").level;
                assert!(now >= *img, "{tile:?}: drape {img} → {now}");
            }
            for i in d.draw.items.iter().filter(|i| i.role == Role::Exact) {
                if let Some(x) = base_drape(i) {
                    was.insert(i.mesh.tile(), x.level);
                }
            }
        }
        assert!(!was.is_empty(), "{RED}");
    }
}

mod batch {
    use super::*;

    /// I13: a frame is ready only when every mesh and drape it needs is
    /// drawable, exactly.
    #[test]
    fn a_batch_frame_is_ready_only_when_complete_and_exact() {
        let mut r = Rig::batch();
        let tiles = children(t(1, 0, 0));
        let need = drawn_draped(&tiles, 5);
        let mut ready = None;
        for _ in 0..30 {
            let d = r.step(&need);
            if d.frame_ready {
                ready = Some(d);
                break;
            }
            r.complete(&d);
        }
        let d = ready.expect(RED);
        assert_eq!(d.draw.items.len(), 4);
        for i in &d.draw.items {
            assert_eq!(i.role, Role::Exact);
            assert_eq!(
                base_drape(i).map(|x| x.resource),
                Some(dr(i.mesh.tile(), 5))
            );
        }
    }

    /// I13: a batch frame's drapes are those of what it draws. A tile the
    /// need names while the selection is still settling — its mesh not yet
    /// in, the tile replaced by its children before it is — never has a
    /// drape loaded: batch never cancels a load it started, so a drape
    /// started for it would be read whatever the frame draws, and what a
    /// bake reads would depend on the order its meshes came in.
    #[test]
    fn a_batch_drape_waits_for_the_meshes_the_frame_draws() {
        let mut r = Rig::batch();
        let parent = t(3, 2, 2);
        let first = r.step(&drawn_draped(&[parent], 6));
        assert!(
            first
                .start
                .iter()
                .all(|s| matches!(s.resource, ResourceId::Mesh(_))),
            "a drape started before the frame's meshes were in: {:?}",
            first.start
        );
        // The parent's mesh is not in yet: the need moves to its children.
        let kids = children(parent);
        let need = drawn_draped(&kids, 7);
        let mut started: Vec<ResourceId> = first.start.iter().map(|s| s.resource).collect();
        let mut ready = false;
        for _ in 0..30 {
            let d = r.step(&need);
            started.extend(d.start.iter().map(|s| s.resource));
            if d.frame_ready {
                ready = true;
                break;
            }
            r.complete(&d);
        }
        assert!(ready, "the frame was made");
        assert!(
            !started
                .iter()
                .any(|s| matches!(s, ResourceId::Drape { tile, .. } if *tile == parent)),
            "a drape was loaded for a tile the frame never draws: {started:?}"
        );
        for k in kids {
            assert!(started.contains(&dr(k, 7)), "{k:?} wears its drape");
        }
    }

    /// I13 with the frames ahead: the need's ring (what the views of the
    /// frames to come select, loaded beside the frame) loads meshes only.
    /// A frame ahead's selection is still settling, and batch never cancels
    /// a load it started, so a drape asked for it would be read whatever
    /// that frame draws; its drapes are asked when it is the frame. And the
    /// ring's meshes, never drawn by this frame, do not hold the frame's own
    /// drapes back.
    #[test]
    fn a_batch_ring_loads_meshes_only_and_holds_no_drape_back() {
        let mut r = Rig::batch();
        let here = t(3, 2, 2);
        let ahead = t(3, 4, 4);
        let mut need = drawn_draped(&[here], 6);
        need.ring.push((ahead, draped(ahead, 6)));
        let first = r.step(&need);
        assert!(
            first
                .start
                .iter()
                .all(|s| matches!(s.resource, ResourceId::Mesh(_))),
            "a drape started before the frame's meshes were in: {:?}",
            first.start
        );
        // The frame's mesh lands; the ring's stays in flight.
        r.load_where(|res| res == mesh(here));
        let mut started: Vec<ResourceId> = first.start.iter().map(|s| s.resource).collect();
        for _ in 0..10 {
            let d = r.step(&need);
            started.extend(d.start.iter().map(|s| s.resource));
            r.load_where(|res| matches!(res, ResourceId::Drape { .. }) || res == mesh(here));
            r.upload(&d);
        }
        assert!(
            started.contains(&mesh(ahead)),
            "the ring's mesh was loaded ahead"
        );
        assert!(
            started.contains(&dr(here, 6)),
            "the ring's mesh in flight held the frame's drape back: {started:?}"
        );
        // The ring's mesh lands too: still no drape for it.
        r.load_where(|res| res == mesh(ahead));
        for _ in 0..10 {
            let d = r.step(&need);
            started.extend(d.start.iter().map(|s| s.resource));
            r.complete(&d);
        }
        assert!(
            !started
                .iter()
                .any(|s| matches!(s, ResourceId::Drape { tile, .. } if *tile == ahead)),
            "a drape was asked for a frame ahead: {started:?}"
        );
    }

    /// I13: what a batch frame reads does not depend on the order its loads
    /// come back in. A stand-in for the traversal refines a tile once its
    /// mesh is in — holding it, its four children waited on, until they all
    /// are — as terrain is revealed pass by pass. Loads come back one at a
    /// time (a bake at one fetch in flight), in six orders. Every order reads
    /// the same drapes, and those are exactly the drapes the frame wears:
    /// batch never cancels a load it started, so a drape started for a pass
    /// the selection settles past is read anyway, and whether one is would
    /// depend on the order the meshes land in.
    #[test]
    fn a_batch_frame_reads_the_same_drapes_whatever_order_its_loads_land_in() {
        use std::cmp::Reverse;
        use std::collections::{BTreeMap, BTreeSet};
        use tuile_budget::{Generation, TileId};

        type Pick = fn(&BTreeMap<ResourceId, Generation>) -> ResourceId;
        let first: Pick = |p| *p.keys().next().expect("pending");
        let last: Pick = |p| *p.keys().next_back().expect("pending");
        let meshes_first: Pick = |p| {
            *p.keys()
                .find(|r| matches!(r, ResourceId::Mesh(_)))
                .or_else(|| p.keys().next())
                .expect("pending")
        };
        let drapes_first: Pick = |p| {
            *p.keys()
                .find(|r| matches!(r, ResourceId::Drape { .. }))
                .or_else(|| p.keys().next())
                .expect("pending")
        };
        let finest_first: Pick = |p| {
            *p.keys()
                .max_by_key(|r| (r.tile().terrain_coord().0, Reverse(**r)))
                .expect("pending")
        };
        let coarsest_first: Pick = |p| {
            *p.keys()
                .min_by_key(|r| (r.tile().terrain_coord().0, **r))
                .expect("pending")
        };
        let roots = [t(3, 2, 2), t(3, 4, 4)];
        let level = |tile: TileId| tile.terrain_coord().0 + 3;
        // The pass: a root whose mesh is not in is drawn; one whose mesh is
        // in is held, its children waited on, until every child's is in;
        // then the children are drawn.
        let pass = |loaded: &BTreeSet<ResourceId>| {
            let mut need = NeedSet::new();
            for root in roots {
                let kids = children(root);
                let wear = |k: &TileId| (*k, draped(*k, level(*k)));
                if !loaded.contains(&mesh(root)) {
                    need.drawn.push(wear(&root));
                } else if kids.iter().all(|k| loaded.contains(&mesh(*k))) {
                    need.drawn.extend(kids.iter().map(wear));
                } else {
                    need.drawn.push(wear(&root));
                    need.blockers.extend(kids.iter().map(wear));
                }
            }
            need
        };
        let orders: [(&str, Pick); 6] = [
            ("in order", first),
            ("in reverse", last),
            ("meshes first", meshes_first),
            ("drapes first", drapes_first),
            ("finest first", finest_first),
            ("coarsest first", coarsest_first),
        ];
        // Without a floor, and with the roots in it: a floor tile's own
        // drape is read whatever the order, the finer one it asks for while
        // a pass draws it is not.
        for pinned in [None, Some(3)] {
            let mut read: Vec<(&str, BTreeSet<ResourceId>)> = Vec::new();
            for (name, pick) in orders {
                // Slots saturated, as behind one fetch in flight: which
                // drapes find one while a pass still draws a tile then turns
                // on the order. Two drape slots, one per root, so the
                // floor's own drapes never wait on each other.
                let mut c = BudgetConfig::batch();
                c.pinned_level = pinned;
                c.max_in_flight[Class::TerrainMesh] = 1;
                c.max_in_flight[Class::Drape] = 2;
                let mut r = Rig::new(c);
                let mut loaded = BTreeSet::new();
                let mut started = BTreeSet::new();
                let mut worn = None;
                for _ in 0..200 {
                    let d = r.step(&pass(&loaded));
                    started.extend(
                        d.start
                            .iter()
                            .map(|s| s.resource)
                            .filter(|r| matches!(r, ResourceId::Drape { .. })),
                    );
                    if d.frame_ready {
                        worn = Some(
                            d.draw
                                .items
                                .iter()
                                .flat_map(|i| i.drapes.iter().map(|x| x.resource))
                                .collect::<BTreeSet<_>>(),
                        );
                        break;
                    }
                    r.upload(&d);
                    // One load comes back.
                    if !r.pending.is_empty() {
                        let done = pick(&r.pending);
                        let gen = r.pending.remove(&done).expect("pending");
                        r.b.on_loaded(done, gen, est(done));
                        loaded.insert(done);
                    }
                }
                let worn = worn.unwrap_or_else(|| {
                    panic!("{name}, floor {pinned:?}: the frame was never made")
                });
                let kids: BTreeSet<ResourceId> = roots
                    .iter()
                    .flat_map(|root| children(*root))
                    .map(|k| dr(k, level(k)))
                    .collect();
                assert_eq!(
                    worn, kids,
                    "{name}, floor {pinned:?}: the frame wears its children's drapes"
                );
                let floor: BTreeSet<ResourceId> = match pinned {
                    Some(p) => roots.iter().map(|root| dr(*root, p)).collect(),
                    None => BTreeSet::new(),
                };
                let expected: BTreeSet<ResourceId> = worn.union(&floor).copied().collect();
                assert_eq!(
                    started, expected,
                    "{name}, floor {pinned:?}: a drape was read that neither the frame \
                     nor the floor wears"
                );
                read.push((name, started));
            }
            for (name, drapes) in &read[1..] {
                assert_eq!(
                    drapes, &read[0].1,
                    "{name}, floor {pinned:?}: other drapes read than in order"
                );
            }
        }
    }

    /// I13: under Batch, the traversal sees loaded content as resident, so
    /// terrain selection does not wait on uploads.
    #[test]
    fn batch_residency_is_loaded_and_interactive_residency_is_drawable() {
        for (mut r, after_load) in [(Rig::batch(), true), (Rig::interactive(), false)] {
            let a = t(3, 2, 2);
            let d = r.step(&drawn(&[a]));
            r.load(&d);
            r.step(&drawn(&[a]));
            assert_eq!(r.b.residency().resident(a), after_load, "{RED}");
        }
    }

    /// I13: `release_beyond` keeps what the last frame needed and is
    /// deterministic.
    #[test]
    fn release_beyond_keeps_the_last_frame_and_is_deterministic() {
        let run = || {
            let mut r = Rig::batch();
            let one: Vec<_> = (0..4).map(|x| t(3, x, 0)).collect();
            let two: Vec<_> = (2..6).map(|x| t(3, x, 0)).collect();
            r.settle(&drawn(&one), 20);
            r.settle(&drawn(&two), 20);
            (r.b.release_beyond(Bytes::ZERO), two)
        };
        let (a, two) = run();
        let (b, _) = run();
        assert_eq!(a.gpu, vec![mesh(t(3, 0, 0)), mesh(t(3, 1, 0))], "{RED}");
        assert_eq!(a, b);
        assert!(two.iter().all(|t| !a.gpu.contains(&mesh(*t))));
    }

    /// I13: what a release between frames lets go of, it lets go of whole —
    /// the decoded copy with the GPU's. A batch's decoded target is
    /// unbounded, so a copy left in the decoded tier would never go, and an
    /// in-process consumer's copy is the very allocation the decoded copy
    /// shares: releasing one and keeping the other bounds nothing.
    #[test]
    fn release_beyond_lets_go_of_the_decoded_copy_too() {
        let mut r = Rig::batch();
        let one: Vec<_> = (0..4).map(|x| t(3, x, 0)).collect();
        let two: Vec<_> = (2..6).map(|x| t(3, x, 0)).collect();
        r.settle(&drawn(&one), 20);
        r.settle(&drawn(&two), 20);
        let before = r.b.ledger().decoded;
        let released = r.b.release_beyond(Bytes::ZERO);
        assert!(!released.gpu.is_empty(), "{RED}");
        assert_eq!(released.decoded, released.gpu, "{RED}");
        for gone in &released.gpu {
            let held = r.b.resource(*gone).map_or(Bytes::ZERO, |v| v.decoded);
            assert_eq!(held, Bytes::ZERO, "{gone:?} kept its decoded copy");
        }
        assert!(r.b.ledger().decoded < before, "{RED}");
        // What the last frame needed keeps both.
        for kept in &two {
            assert!(r
                .b
                .resource(mesh(*kept))
                .is_some_and(|v| v.decoded > Bytes::ZERO));
        }
    }

    /// I13, D1: a Batch failure ends the session.
    #[test]
    fn a_batch_failure_ends_the_session() {
        let mut r = Rig::batch();
        let a = t(3, 2, 2);
        let s = *r.step(&drawn(&[a])).start.first().expect(RED);
        r.b.on_failed(mesh(a), s.gen, Failure::Transient);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.fatal, Some(SessionError::LoadFailed(mesh(a))));
        assert!(!d.frame_ready);
    }

    /// The end of a session names the resource that ended it, so the
    /// runtime can report it to the client.
    #[test]
    fn an_end_of_session_names_its_resource() {
        let mut r = Rig::batch();
        let a = t(3, 2, 2);
        let s = *r.step(&drawn(&[a])).start.first().expect(RED);
        r.b.on_failed(mesh(a), s.gen, Failure::Transient);
        let d = r.step(&drawn(&[a]));
        let fatal = d.fatal.expect(RED);
        assert_eq!(fatal.resource(), Some(mesh(a)), "{RED}");
    }
}

mod failures {
    use super::*;

    /// D1, C17: an interactive failure is retried after a doubling backoff.
    #[test]
    fn an_interactive_failure_is_retried_with_doubling_backoff() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let need = drawn(&[a]);
        let mut starts = Vec::new();
        for _ in 0..400 {
            let at = r.now;
            let d = r.step(&need);
            for s in &d.start {
                starts.push(at);
                r.b.on_failed(s.resource, s.gen, Failure::Transient);
            }
        }
        assert!(starts.len() >= 3, "{RED}");
        let gap1 = starts[1].since(starts[0]);
        let gap2 = starts[2].since(starts[1]);
        assert!(
            gap1 >= Micros::from_millis(500) && gap1 < Micros::from_millis(550),
            "{gap1:?}"
        );
        assert!(
            gap2 >= Micros::from_millis(1_000) && gap2 < Micros::from_millis(1_050),
            "{gap2:?}"
        );
    }

    /// D1, I1: a failed tile keeps its fallback drawn.
    #[test]
    fn a_failed_tile_keeps_its_fallback_drawn() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let kid = children(root)[2];
        for _ in 0..120 {
            let d = r.step(&drawn(&[kid]));
            for s in &d.start {
                r.b.on_failed(s.resource, s.gen, Failure::Permanent);
            }
            assert_eq!(d.stats.lost, 0, "{RED}");
            assert_eq!(d.draw.get(mesh(root)).map(|i| i.role), Some(Role::Fallback));
        }
    }

    /// D1: a permanent failure waits the full backoff.
    #[test]
    fn a_permanent_failure_waits_the_full_backoff() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let s = *r.step(&drawn(&[a])).start.first().expect(RED);
        r.b.on_failed(mesh(a), s.gen, Failure::Permanent);
        let ds = r.idle_for(&drawn(&[a]), Micros::from_secs(29));
        assert!(!started(&ds, mesh(a)));
        let ds = r.idle_for(&drawn(&[a]), Micros::from_secs(2));
        assert!(started(&ds, mesh(a)));
    }

    /// C17: a failure holds no slot.
    #[test]
    fn a_failure_holds_no_slot() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let (a, b) = (t(3, 0, 0), t(3, 1, 0));
        let mut need = drawn(&[a, b]);
        need.drawn[0].1.sse_excess = 5.0;
        let s = *r.step(&need).start.first().expect(RED);
        assert_eq!(s.resource, mesh(a));
        r.b.on_failed(mesh(a), s.gen, Failure::Transient);
        let d = r.step(&need);
        assert_eq!(d.start.first().map(|s| s.resource), Some(mesh(b)));
    }
}

mod determinism {
    use super::*;

    fn script(shuffle: bool) -> Vec<u64> {
        let mut r = Rig::interactive();
        let mut out = Vec::new();
        for step in 0u64..200 {
            let x = step / 20;
            let mut tiles: Vec<_> = (0..4).map(|k| t(4, x + k, (step / 50) % 4)).collect();
            if shuffle {
                tiles.reverse();
            }
            let mut need = drawn_draped(&tiles, 6);
            if shuffle {
                need.drawn.rotate_left(1);
            }
            let d = r.step(&need);
            out.push(d.digest());
            if step % 3 != 0 {
                r.complete(&d);
            }
        }
        out
    }

    /// I12, C25: the same inputs give the same decision stream, bit for bit.
    #[test]
    fn the_same_inputs_give_the_same_digest_stream() {
        let a = script(false);
        assert!(a.windows(2).any(|w| w[0] != w[1]), "{RED}: nothing decided");
        assert_eq!(a, script(false));
    }

    /// I12, C11: the decisions do not depend on the order of the need.
    #[test]
    fn the_decisions_do_not_depend_on_the_order_of_the_need() {
        let a = script(false);
        assert!(a.windows(2).any(|w| w[0] != w[1]), "{RED}: nothing decided");
        assert_eq!(a, script(true));
    }

    /// Engineering bar: a non-finite float counts as zero and is reported.
    #[test]
    fn a_non_finite_need_counts_as_zero_and_is_reported() {
        let a = t(3, 2, 2);
        let mut bad = drawn(&[a]);
        bad.drawn[0].1.sse_excess = f32::NAN;
        let mut zero = drawn(&[a]);
        zero.drawn[0].1.sse_excess = 0.0;
        let (mut r1, mut r2) = (Rig::interactive(), Rig::interactive());
        let (d1, d2) = (r1.step(&bad), r2.step(&zero));
        assert_eq!(d1.stats.rejected_inputs, 1, "{RED}");
        assert_eq!(d1.digest(), d2.digest());
    }
}

mod bounded {
    use super::*;

    /// I7, S2 (M2.5): tracked state stays bounded by what is needed and
    /// resident, however long the session.
    #[test]
    fn tracked_state_stays_bounded_over_a_long_pan() {
        let mut r = Rig::new(small(8));
        let mut peak = 0;
        for x in 0..512u64 {
            let tiles = [t(9, x, 100), t(9, x, 101)];
            let d = r.settle(&drawn(&tiles), 6);
            let resident = Class::ALL
                .iter()
                .map(|&c| d.stats.gpu[c].0 / MESH.0)
                .sum::<u64>() as usize;
            assert!(
                r.b.tracked() <= 4 * (resident + 2) + 16,
                "{}",
                r.b.tracked()
            );
            peak = peak.max(r.b.tracked());
        }
        assert!(peak > 0, "{RED}");
    }
}

mod stats {
    use super::*;

    /// C28: the stats report bytes per class against the target.
    #[test]
    fn the_stats_report_bytes_per_class_against_the_target() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let d = r.settle(&drawn_draped(&[a], 4), 10);
        assert_eq!(d.stats.gpu[Class::TerrainMesh], MESH, "{RED}");
        assert_eq!(d.stats.gpu[Class::Drape], drape());
        assert_eq!(d.stats.target, Rig::interactive_config().target);
        assert_eq!(d.stats.sse_scale_milli, 1_000);
    }

    /// C28, audit: completed loads land in the latency histogram.
    #[test]
    fn completed_loads_land_in_the_latency_histogram() {
        let mut r = Rig::interactive();
        r.dt = Micros::from_millis(100);
        let a = t(3, 2, 2);
        let d = r.step(&drawn(&[a]));
        r.load(&d);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.stats.completed, 1, "{RED}");
        let mut want = [0; tuile_budget::stats::LATENCY_BUCKETS];
        want[7] = 1; // 100 ms ∈ [64, 128)
        assert_eq!(d.stats.latency_ms, want);
    }

    /// C28, B-anti-pattern: evict-then-request is counted as thrash.
    #[test]
    fn evict_then_request_is_counted_as_thrash() {
        let mut r = Rig::new(small(1));
        let (a, b) = (t(3, 0, 0), t(3, 5, 5));
        r.settle(&drawn(&[a]), 10);
        r.settle(&drawn(&[b]), 10);
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.stats.thrash, 1, "{RED}");
    }

    /// I14, review of Checkpoint B: an evicted resource read back from a
    /// local store is a demotion paying off, not thrash; only the network
    /// counts as thrash.
    #[test]
    fn a_local_reread_after_eviction_is_not_thrash() {
        let mut r = Rig::new(small(1));
        let (a, b) = (t(3, 0, 0), t(3, 5, 5));
        r.settle(&drawn(&[a]), 10);
        r.settle(&drawn(&[b]), 10);
        assert!(!r.b.residency().drawable(a), "a was evicted");
        r.store.0.insert(mesh(a));
        let d = r.step(&drawn(&[a]));
        assert_eq!(d.start.first().map(|s| s.source), Some(Source::Local));
        assert_eq!((d.stats.thrash, d.stats.rereads), (0, 1));
    }

    /// I3, C28: convergence is reported once everything is exact and idle.
    #[test]
    fn convergence_is_reported_once_everything_is_exact_and_idle() {
        let mut r = Rig::interactive();
        let tiles = children(t(1, 1, 1));
        let need = drawn_draped(&tiles, 4);
        let first = r.step(&need);
        assert!(!first.stats.converged);
        let d = r.settle(&need, 20);
        assert!(d.stats.converged, "{RED}");
        assert_eq!(d.stats.underdraped, 0);
        assert_eq!(d.stats.imagery_over_terrain[4 - 2 + 8], 4);
    }

    /// C28, audit: cancel and complete are counted, for the still-camera
    /// ratio.
    #[test]
    fn cancels_and_completions_are_counted() {
        let mut r = Rig::interactive();
        let (a, b) = (t(3, 0, 0), t(3, 1, 0));
        let d = r.step(&drawn(&[a, b]));
        for s in d.start.iter().filter(|s| s.resource == mesh(a)) {
            r.b.on_loaded(s.resource, s.gen, est(s.resource));
        }
        let ds = r.idle_for(&drawn(&[a]), Micros::from_secs(1));
        let completed: u32 = ds.iter().map(|d| d.stats.completed).sum();
        let cancelled: u32 = ds.iter().map(|d| d.stats.cancelled).sum();
        assert_eq!((completed, cancelled), (1, 1), "{RED}");
    }

    /// PerClass is what the observer serialises.
    #[test]
    fn per_class_indexes_by_class() {
        let mut p = PerClass::splat(0u32);
        p[Class::Drape] = 7;
        assert_eq!(p.0[Class::Drape.index()], 7);
        assert_eq!(p.iter().filter(|(_, v)| *v == 7).count(), 1);
    }
}

mod hard_limit {
    use super::*;

    fn ceiling(hard_meshes: u64) -> BudgetConfig {
        let mut c = small(1);
        c.hard_limit[Class::TerrainMesh] = Bytes(MESH.0 * hard_meshes);
        c.hard_reserve_percent = 25;
        c
    }

    /// I2, review of Checkpoint A: no delivery ever takes a class beyond its
    /// hard limit, not even a hole's.
    #[test]
    fn no_delivery_ever_crosses_the_hard_limit() {
        let mut r = Rig::new(ceiling(4));
        let tiles: Vec<_> = (0..8).map(|x| t(3, x, 0)).collect();
        let mut exact = 0;
        for _ in 0..60 {
            let d = r.step(&drawn(&tiles));
            assert!(r.b.ledger().gpu[Class::TerrainMesh] <= Bytes(MESH.0 * 4));
            exact = d.stats.exact;
            r.complete(&d);
        }
        // Holes stop at the hard limit less its reserve: three of four.
        assert_eq!(exact, 3, "{RED}");
    }

    /// I8: at the hard limit the relaxation climbs faster than at mere
    /// overflow.
    #[test]
    fn at_the_hard_limit_the_relaxation_climbs_faster() {
        let rise = |hard: u64| {
            let mut r = Rig::new(ceiling(hard));
            let tiles: Vec<_> = (0..8).map(|x| t(3, x, 0)).collect();
            let before = r.settle(&drawn(&tiles), 30).sse_scale_milli;
            let ds = r.idle_for(&drawn(&tiles), Micros::from_secs(1));
            ds.last().map(|d| d.sse_scale_milli).unwrap_or(0) - before
        };
        let overflowing = rise(64);
        let at_ceiling = rise(4);
        assert!(overflowing > 0, "{RED}");
        assert!(
            at_ceiling >= 2 * overflowing,
            "{at_ceiling} vs {overflowing}"
        );
    }

    /// I1, I9: at the ceiling, a coarser parent replacing its drawn children
    /// uses the reserve, so a zoom-out always completes.
    #[test]
    fn a_coarsening_parent_uses_the_reserve_at_the_ceiling() {
        let mut r = Rig::new(ceiling(4));
        let p = t(2, 1, 1);
        let kids = children(p);
        // Three children fill the ceiling less its reserve.
        r.settle(&drawn(&kids[..3]), 30);
        assert_eq!(
            r.b.ledger().gpu[Class::TerrainMesh],
            Bytes(MESH.0 * 3),
            "{RED}"
        );
        let d = r.settle(&drawn(&[p]), 30);
        assert_eq!(d.draw.get(mesh(p)).map(|i| i.role), Some(Role::Exact));
        assert!(r.b.ledger().gpu[Class::TerrainMesh] <= Bytes(MESH.0 * 4));
    }
}

mod acknowledged {
    use super::*;

    /// I1, review of point 2: only acknowledged resources are drawn; a
    /// discarded upload is never drawn and its fallback stays.
    #[test]
    fn a_discarded_upload_is_never_drawn_and_its_fallback_stays() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let kid = children(root)[1];
        let d = r.step(&drawn(&[kid]));
        r.load(&d);
        let d = r.step(&drawn(&[kid]));
        let (_, g) = *d.deliver.first().expect(RED);
        r.b.on_discarded(mesh(kid), g);
        for _ in 0..10 {
            let d = r.step(&drawn(&[kid]));
            assert!(!d.draw.draws(kid));
            assert_eq!(d.draw.get(mesh(root)).map(|i| i.role), Some(Role::Fallback));
            assert_eq!(d.stats.lost, 0);
        }
    }

    /// I1: a delivered, uploaded but unacknowledged resource is not drawn.
    #[test]
    fn an_unacknowledged_upload_is_not_drawn() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let kid = children(root)[2];
        let d = r.step(&drawn(&[kid]));
        r.load(&d);
        let d = r.step(&drawn(&[kid]));
        assert!(!d.upload_plan.items.is_empty(), "{RED}");
        let d = r.step(&drawn(&[kid]));
        assert!(!d.draw.draws(kid));
        assert_eq!(d.draw.get(mesh(root)).map(|i| i.role), Some(Role::Fallback));
    }
}

mod refresh {
    use super::*;

    /// Review of point 7: an epoch change re-requests what is resident, at
    /// the Refresh urgency, and swaps new before old without a hole.
    #[test]
    fn an_epoch_change_refreshes_resident_drapes_without_a_hole() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let need = drawn_draped(&[a], 4);
        r.settle(&need, 10);
        let old = r.b.resource(dr(a, 4)).and_then(|v| v.gen);
        r.b.new_epoch(Class::Drape, None);
        let d = r.step(&need);
        assert!(d.start.iter().any(|s| s.resource == dr(a, 4)), "{RED}");
        assert_eq!(
            r.b.resource(dr(a, 4)).and_then(|v| v.urgency),
            Some(Urgency::Refresh)
        );
        r.complete(&d);
        for _ in 0..10 {
            let d = r.step(&need);
            let worn = d.draw.get(mesh(a)).and_then(base_drape).map(|x| x.resource);
            assert_eq!(worn, Some(dr(a, 4)), "never a hole");
            r.complete(&d);
        }
        let new = r.b.resource(dr(a, 4)).and_then(|v| v.gen);
        assert_ne!(new, old, "the new content replaced the old");
        assert_eq!(r.b.resource(dr(a, 4)).map(|v| v.urgency), Some(None));
    }

    /// Review of point 7: a refresh uses only free slots: floor, holes and
    /// blockers go first.
    #[test]
    fn a_refresh_never_starves_floor_hole_or_blocker() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::Drape] = 1;
        let mut r = Rig::new(c);
        let a = t(3, 2, 2);
        r.settle(&drawn_draped(&[a], 4), 10);
        r.b.new_epoch(Class::Drape, None);
        let h = t(3, 6, 6);
        let d = r.step(&drawn_draped(&[a, h], 4));
        let drapes: Vec<_> = d
            .start
            .iter()
            .filter(|s| s.class == Class::Drape)
            .map(|s| s.resource)
            .collect();
        assert_eq!(drapes, vec![dr(h, 4)], "{RED}");
    }

    /// Review of point 7: a refresh never preempts anything.
    #[test]
    fn a_refresh_never_preempts() {
        let mut c = Rig::interactive_config();
        c.max_in_flight[Class::TerrainMesh] = 1;
        let mut r = Rig::new(c);
        let a = t(3, 2, 2);
        r.settle(&drawn(&[a]), 10);
        let g = t(3, 5, 5);
        let mut need = drawn(&[a]);
        need.ring.push((g, need_ring(g)));
        let d = r.step(&need);
        assert!(d.start.iter().any(|s| s.resource == mesh(g)), "{RED}");
        r.b.new_epoch(Class::TerrainMesh, None);
        let ds = r.idle_for(&need, Micros::from_secs(1));
        assert!(ds.iter().all(|d| d.cancel.is_empty()));
        assert!(!started(&ds, mesh(a)), "the ring holds the only slot");
    }

    fn need_ring(id: tuile_budget::TileId) -> tuile_budget::Need {
        need(id)
    }
    /// D-A4: a layer's epoch refreshes that layer's drapes only.
    #[test]
    fn an_epoch_refreshes_only_its_layer() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let need = drawn_layered(&[a], 4, &[0, 1]);
        r.settle(&need, 20);
        r.b.new_epoch(Class::Drape, Some(LayerId(1)));
        let d = r.step(&need);
        let drapes: Vec<_> = d
            .start
            .iter()
            .filter(|s| s.class == Class::Drape)
            .map(|s| s.resource)
            .collect();
        assert_eq!(drapes, vec![dl(a, 1, 4)], "{RED}");
        assert_eq!(
            r.b.resource(dl(a, 1, 4)).and_then(|v| v.urgency),
            Some(Urgency::Refresh)
        );
    }
}

mod drape_caps {
    use super::*;

    /// D5, review of point 14: the drape side caps how far a drape climbs:
    /// at 1024 texels, two levels above terrain.
    #[test]
    fn the_drape_side_caps_how_far_a_drape_climbs() {
        let mut c = Rig::interactive_config();
        c.max_drape_side = 1_024;
        let mut r = Rig::new(c);
        let a = t(2, 1, 1);
        let d = r.step(&drawn_draped(&[a], 9));
        assert!(d.start.iter().any(|s| s.resource == dr(a, 4)), "{RED}");
    }

    /// D5: the cap over terrain is a profile parameter.
    #[test]
    fn the_cap_over_terrain_is_a_profile_parameter() {
        let mut c = Rig::interactive_config();
        c.imagery_over_terrain_cap = 1;
        let mut r = Rig::new(c);
        let a = t(2, 1, 1);
        let d = r.step(&drawn_draped(&[a], 9));
        assert!(d.start.iter().any(|s| s.resource == dr(a, 3)), "{RED}");
    }

    /// The configuration reports the effective cap.
    #[test]
    fn the_effective_cap_is_the_smaller_of_the_two() {
        let mut c = BudgetConfig::interactive(Bytes::mib(64));
        assert_eq!(c.imagery_cap(), 3);
        c.max_drape_side = 1_024;
        assert_eq!(c.imagery_cap(), 2);
        c.max_drape_side = 256;
        assert_eq!(c.imagery_cap(), 0);
        c.imagery_over_terrain_cap = 0;
        c.max_drape_side = 4_096;
        assert_eq!(c.imagery_cap(), 0);
    }
}

mod staging {
    use super::*;

    /// C2, review of point 17: staging bytes count against the delivery
    /// credit.
    #[test]
    fn staging_bytes_count_against_the_delivery_credit() {
        let mut c = Rig::interactive_config();
        c.delivery_credit = Bytes(MESH.0 * 4);
        c.upload_bytes_per_frame = Bytes(1);
        let mut r = Rig::new(c);
        let tiles: Vec<_> = (0..8).map(|x| t(4, x, 0)).collect();
        let need = drawn(&tiles);
        let d = r.step(&need);
        for s in &d.start {
            r.b.on_loaded(
                s.resource,
                s.gen,
                Cost {
                    gpu: MESH,
                    staging: Bytes(MESH.0 * 2),
                },
            );
        }
        let d = r.step(&need);
        assert_eq!(d.deliver.len(), 2, "{RED}");
        assert_eq!(r.b.ledger().in_transit, Bytes(MESH.0 * 4));
    }
}

mod need_lifetime {
    use super::*;

    fn static_and_send<T: 'static + Send + Sync>() {}

    /// I7, review: the budget holds no reference to a step's need; it is
    /// `'static`, `Send` and `Sync`, so it cannot.
    #[test]
    fn the_budget_borrows_nothing_from_a_step() {
        static_and_send::<Budget>();
        let mut r = Rig::interactive();
        {
            let need = drawn(&[t(3, 1, 1)]);
            r.step(&need);
        }
        r.step(&NeedSet::new());
    }

    /// I7: a huge need leaves nothing behind once it is gone.
    #[test]
    fn a_huge_need_leaves_nothing_behind() {
        let mut c = Rig::interactive_config();
        c.request_heap = 64;
        let mut r = Rig::new(c);
        let tiles: Vec<_> = (0..128)
            .flat_map(|x| (0..128).map(move |y| t(7, x, y)))
            .collect();
        let d = r.step(&drawn_draped(&tiles, 9));
        assert!(!d.start.is_empty(), "{RED}");
        assert!(r.b.tracked() <= 64 + 16, "{}", r.b.tracked());
        r.idle_for(&NeedSet::new(), Micros::from_secs(2));
        assert_eq!(r.b.tracked(), 0);
    }
}

mod stats_digest {
    use super::*;

    /// Audit: the per-step statistics have a digest, so device runs compare
    /// to the simulation.
    #[test]
    fn the_stats_digest_follows_every_field() {
        let mut r = Rig::interactive();
        let a = t(3, 2, 2);
        let d1 = r.step(&drawn(&[a]));
        let mut r2 = Rig::interactive();
        let d2 = r2.step(&drawn(&[a]));
        assert_eq!(d1.stats.digest(), d2.stats.digest());
        let mut other = d1.stats.clone();
        other.exact += 1;
        assert_ne!(other.digest(), d1.stats.digest(), "{RED}");
        other = d1.stats.clone();
        other.latency_ms[17] += 1;
        assert_ne!(other.digest(), d1.stats.digest());
    }
}

mod deadline {
    use super::*;
    use tuile_budget::{FrameDeadline, SwapGroup};

    /// I18 (D-A8): a frame deadline is its share of the display period, at
    /// the executor's throughput: a quarter of 13.9, 11.1 and 8.3 ms at 72,
    /// 90 and 120 Hz.
    #[test]
    fn a_frame_deadline_is_its_share_of_the_display_period() {
        for (hz, period_us) in [(72u32, 13_888u64), (90, 11_111), (120, 8_333)] {
            let d = FrameDeadline::from_refresh(hz, 250, 1_000_000);
            assert_eq!(d.time, Micros(period_us * 250 / 1_000), "{hz} Hz {RED}");
            assert_eq!(d.bytes, Bytes(d.time.0 * 1_000), "{hz} Hz: 1 MB per ms");
        }
        let at = |hz| FrameDeadline::from_refresh(hz, 250, 1_000_000).bytes;
        assert!(
            at(120) < at(90) && at(90) < at(72),
            "a faster display, less per frame"
        );
        let whole = FrameDeadline::from_refresh(60, 1_000, 1_000);
        assert_eq!(whole.time, Micros(16_666));
        assert_eq!(whole.bytes, Bytes(16_666));
    }

    /// I18, I9: the first item of a frame always goes, a swap group goes
    /// whole when it fits and waits whole when it does not.
    #[test]
    fn the_first_item_always_goes_and_a_swap_group_goes_whole() {
        let cap = |b| FrameDeadline {
            bytes: Bytes(b),
            time: Micros(1_000),
        };
        let (a, b, c, e) = (
            mesh(t(3, 0, 0)),
            mesh(t(3, 1, 0)),
            mesh(t(3, 2, 0)),
            mesh(t(3, 3, 0)),
        );
        let cost = |_| Bytes(100);
        let queue = [a, b, c, e];
        let groups = [SwapGroup {
            members: vec![b, c],
        }];
        let none: &[ResourceId] = &[];
        // a fits; the group b + c would make 300 > 250: it waits, whole.
        assert_eq!(
            cap(250).take(&queue, &groups, Bytes::ZERO, cost),
            &[a],
            "{RED}"
        );
        // Without the group, b goes alone.
        assert_eq!(cap(250).take(&queue, &[], Bytes::ZERO, cost), &[a, b]);
        // A group that fits goes whole.
        assert_eq!(
            cap(300).take(&queue, &groups, Bytes::ZERO, cost),
            &[a, b, c]
        );
        // First in a frame and larger than the deadline: its first item goes.
        assert_eq!(cap(50).take(&[b, c, e], &groups, Bytes::ZERO, cost), &[b]);
        assert_eq!(cap(50).take(&[e], &[], Bytes::ZERO, cost), &[e]);
        // Not first in the frame: nothing over the deadline goes.
        assert_eq!(cap(50).take(&[e], &[], Bytes(10), cost), none);
        assert_eq!(cap(250).take(&queue, &[], Bytes(100), cost), &[a]);
    }
}

/// O1.4: a step reads again only what may have changed (`downstream`). Each
/// test pins one propagation rule with a tile that nothing else would read
/// again: left out, the tile keeps last step's answer (red in a release
/// build; a debug build's full recomputation fails first).
mod dirty_sets {
    use super::*;

    /// The parent's rule: a tile standing on an ancestor is read again when
    /// a finer tile below it starts being drawn — the held set changes at
    /// the step's end — though nothing of its own changed; it is then held
    /// by that tile, no longer falling back.
    #[test]
    fn a_tile_is_held_by_a_finer_tile_drawn_since() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let p = children(root)[0];
        let k = children(p)[0];
        let ask = drawn(&[p, k]);
        let d = r.step(&ask);
        assert_eq!(d.stats.fallback, 2, "both stand on the root first");
        r.load_where(|x| x == mesh(k));
        for _ in 0..4 {
            let d = r.step(&ask);
            r.upload(&d);
        }
        let d = r.step(&ask);
        assert!(r.b.residency().drawable(k));
        assert!(!r.b.residency().drawable(p));
        assert_eq!(
            d.draw.get(mesh(k)).map(|i| i.serves.clone()),
            Some(vec![p, k]),
            "{RED}: the finer tile drawn since holds its ancestor"
        );
        assert_eq!(d.stats.fallback, 0, "{RED}");
    }

    /// The descendants' rule: a tile falling back is read again when a
    /// nearer ancestor becomes drawable, and stands on it.
    #[test]
    fn a_tile_falling_back_moves_to_a_nearer_ancestor_drawn_since() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let p = children(root)[0];
        let k = children(p)[0];
        let mut ask = drawn(&[k]);
        ask.ancestors.push((p, need(p)));
        let d = r.step(&ask);
        assert_eq!(
            d.draw.get(mesh(root)).map(|i| i.serves.clone()),
            Some(vec![k])
        );
        r.load_where(|x| x == mesh(p));
        for _ in 0..4 {
            let d = r.step(&ask);
            r.upload(&d);
        }
        let d = r.step(&ask);
        assert!(r.b.residency().drawable(p));
        assert!(!r.b.residency().drawable(k));
        assert_eq!(
            d.draw.get(mesh(p)).map(|i| (i.role, i.serves.clone())),
            Some((Role::Fallback, vec![k])),
            "{RED}: the tile stands on its nearest drawable ancestor"
        );
        assert!(!d.draw.draws(root), "{RED}");
    }

    /// The drapes' rule: a drawn mesh wearing an ancestor's drape is read
    /// again when a nearer ancestor's drape becomes drawable, and wears it.
    /// The nearer ancestor is a blocker: its drape is wanted, it is not
    /// drawn, so nothing else reads the mesh again.
    #[test]
    fn a_mesh_wears_a_nearer_ancestors_drape_drawn_since() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn_draped(&[root], 0), 10);
        let p = children(root)[0];
        let k = children(p)[0];
        let mut ask = drawn_draped(&[k], 2);
        ask.blockers.push((p, draped(p, 1)));
        r.step(&ask);
        r.load_where(|x| x == mesh(k));
        for _ in 0..4 {
            let d = r.step(&ask);
            r.upload(&d);
        }
        let worn = |d: &tuile_budget::Decisions| {
            d.draw.get(mesh(k)).and_then(base_drape).map(|c| c.resource)
        };
        let d = r.step(&ask);
        assert_eq!(worn(&d), Some(dr(root, 0)), "the root's drape first");
        r.load_where(|x| x == dr(p, 1));
        for _ in 0..4 {
            let d = r.step(&ask);
            r.upload(&d);
        }
        let d = r.step(&ask);
        assert!(r.b.residency().drawable(k));
        assert_eq!(
            worn(&d),
            Some(dr(p, 1)),
            "{RED}: the mesh wears the nearer drape"
        );
    }

    /// The place's rule: a mesh wearing an ancestor's drape is read again
    /// when that ancestor's place in the tree changes (here its footprint),
    /// and maps the drape into the new footprint. The ancestor is a
    /// blocker, not drawn: nothing else reads the mesh again.
    #[test]
    fn a_mesh_maps_an_ancestors_drape_into_its_new_footprint() {
        let mut r = Rig::interactive();
        let root = t(0, 0, 0);
        r.settle(&drawn(&[root]), 10);
        let p = children(root)[0];
        let k = children(p)[0];
        let mut ask = drawn_draped(&[k], 2);
        ask.blockers.push((p, draped(p, 1)));
        r.step(&ask);
        r.load_where(|x| x == mesh(k) || x == dr(p, 1));
        for _ in 0..4 {
            let d = r.step(&ask);
            r.upload(&d);
        }
        let uv = |d: &tuile_budget::Decisions| {
            d.draw
                .get(mesh(k))
                .and_then(base_drape)
                .map(|c| (c.resource, c.uv))
        };
        let d = r.step(&ask);
        let between = tuile_budget::UvTransform::between;
        assert_eq!(uv(&d), Some((dr(p, 1), between(rect(k), rect(p)).unwrap())));
        // The ancestor's footprint, told differently: its root's.
        ask.blockers[0].1 = draped(p, 1).with_rect(rect(root));
        r.step(&ask);
        let d = r.step(&ask);
        assert_eq!(
            uv(&d),
            Some((dr(p, 1), between(rect(k), rect(root)).unwrap())),
            "{RED}: the drape mapped into the footprint the need tells"
        );
    }
}
