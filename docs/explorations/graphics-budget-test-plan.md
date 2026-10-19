<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Graphics budget — test plan (Phase 2)

The tests that prove the spec (`graphics-budget-to-be-and-spec.md`): the motion patterns
P1–P6 of §12, the invariants I1–I18 of §1, the lens of §13, the platforms of §14 and the
observability contract of §16. Every cell of the matrix below is a named test: the
invariant or obligation it proves, the profiles it runs under, whether it exists (file and
test) or is new, the phase that writes it, and the signals it asserts through.

Phase 2 of the budget plan, written on 2026-10-19 against `feat/budget-core` at `b9eda88`
(rebased on `main` 65b04cb). It is a review gate: nothing here is implemented yet.

## 0. Summary

- **Six levels.** L1 unit behaviour, L2 property and model-based, L3 deterministic
  simulation, L4 the real viewer headless on a real GPU, L5 the apps on devices, and O,
  observability: every signal is seen moving with its behaviour and still without it.
- **Tests are written from the spec, red first.** A new test lands `#[ignore = "red until
  …"]`, is seen failing against the code as it is (or a stub), then the code turns it green.
  Each phase ends at a gate where mutation testing checks the plan itself: a fault the spec
  forbids that no test catches is a hole in this plan, and is closed here before the next
  phase (§9).
- **Tests and production share one vocabulary.** A test asserts through the signals of the
  spec's §16 wherever one exists (`lost_tile_steps_total == 0`, `converged_seconds ≤ T_conv`),
  so a regression reads the same on a dashboard as in a red test.
- **Counts** (§12): 218 cells — 74 existing tests, 143 new, and one later, for a headset
  shell that does not exist yet. By level, existing / new: L1 34 / 20; L2 14 / 16; L3 24 /
  38; L4 2 / 17; L5 0 / 23, plus the one later; O 0 / 29.

## 1. Levels

| Level | What | Where | Runs |
|---|---|---|---|
| **L1** | One behaviour of the budget, against a hand-built `NeedSet` and scripted arrivals | `crates/tuile-core/tests/budget_core.rs`, `budget_imagery.rs`; traversal units in `crates/tuile-core/src/traversal.rs` | `cargo test -p tuile-core --test budget_core --test budget_imagery` (debug) |
| **L2** | A property over random inputs; a reference model checked step by step | `crates/tuile-core/tests/budget_properties.rs`, and a new `budget_model.rs` | `PROPTEST_CASES` 256 locally, 10 000 on a PR, 1 000 000 at a gate |
| **L3** | Scripted cameras over a synthetic world, a seeded network and a simulated GPU, every invariant checked every step | `crates/tuile-core/tests/budget_sim/` | `cargo test --release -p tuile-core --test budget_sim` (the scenarios are ignored in debug) |
| **L4** | The real viewer, headless, on a real GPU, with the same pattern scripts | `crates/tuile-viewer` headless mode and its control API (spec §4c), `crates/tuile-wgpu/tests/` | Metal on the Mac, lavapipe on Linux CI, `TUILE_REQUIRE_GPU=1` |
| **L5** | The apps on devices, driven by their own UI test tools | macOS app, iOS simulator (normal and low memory), iPhone, Android emulator; a headset later | at the phase gates (§7) |
| **O** | A signal moves when its behaviour happens and stays still when it does not | a new `crates/tuile-core/tests/budget_signals.rs`; the sinks' own crates | with L1–L3 on a PR |

Profiles (spec §14): **D** desktop, **H** handheld, **Hl** handheld at its low clamp,
**W** web (the budget a message away from its executor), **X72**, **X90**, **X120** a
stereo headset at that rate, **D2** a desktop with two eyes (hyperstereo), **B** batch,
**R** a remote session for a consumer that declares its budget (decisions and acks a round
trip late on a LAN, the budget on the declared memory; D-A5).

## 2. The matrix

Existing tests are cited as `file` › `module::test`. In the tables, `core` is
`crates/tuile-core/tests/budget_core.rs`, `img` is `budget_imagery.rs`, `prop` is
`budget_properties.rs`, `sim` is `budget_sim/` (`scenarios.rs` unless named), `trav` is
`crates/tuile-core/src/traversal.rs`, `wgpu` is `crates/tuile-wgpu/tests/`. Phase 3 is the
budget, the simulation and the core; 4.1 the headless harness on today's viewer (its tests
start red where today's code fails); 4.2 the cut-over; 5 the headless pattern runs; 6 the
devices.

### 2.1 P1 — Progressive loading

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `requests::the_floor_is_requested_before_anything_else` | P1.1 (a) | D | exists: core | — | `starts_total` |
| L1 | `requests::nothing_waits_while_a_slot_is_free` | P1.4 (b) | D | exists: core | — | `in_flight` |
| L1 | `upload::the_upload_plan_follows_priority_not_arrival` | P1.4 (a), P1.1 (a) | D | exists: core | — | `upload_plan_bytes` |
| L1 | `tiers::local_reads_never_wait_behind_network_work` | P1.4 (c), I15 | D | exists: core | — | `local_waits_total` |
| L1 | `stats::convergence_is_reported_once_everything_is_exact_and_idle` | P1.3 | D | exists: core | — | `converged` |
| L1 | `draw::the_floor_is_drawn_only_with_its_drape` | I1 (D-A1), P1.1 | D | new | 3 | `bare_items`, `floor_in` |
| L1 | `upload::a_floor_mesh_and_its_drape_upload_as_one_group` | I1 (D-A1) | D | new | 3 | `swaps_total{kind="mesh_drape"}` |
| L1 | `draw::nothing_refines_while_wanted_ground_is_lost_or_bare` | P1.1 (b) | D | new | 3 | `lost_tiles`, `bare_items` |
| L1 | `draw::a_still_need_never_draws_coarser_than_it_drew` | P1.2 | D | new | 3 | `drawn_tiles` |
| L2 | `the_ground_is_always_covered_once_the_floor_is_in` | I1 (meshes); extended to dressed items in Phase 3 | D, B | exists: prop | 3 (extended) | `lost_tile_steps_total` |
| L2 | `a_completing_script_brings_the_floor_in` | P1.1 | D | exists: prop | — | `floor_in` |
| L2 | `every_drawn_item_wears_its_drapes_once_the_floor_is_in` | I1 (D-A1) | D, B | new | 3 | `bare_item_steps_total` |
| L2 | `a_still_camera_only_ever_refines` | P1.2, over generated paths that end still (§3) | D, H | new | 3 | `drawn_tiles` |
| L2 | `starts_follow_the_priority_order_within_each_quota` | P1.4 | D, H | new | 3 | `starts_total`, `in_flight` |
| L3 | `a_still_camera_converges_within_two_seconds_on_desktop` | P1.3 | D | exists: sim | — | `converged_seconds` |
| L3 | `a_still_camera_converges_within_four_seconds_on_a_phone` | P1.3; red and ignored until its bound is set from the L5 iPhone sessions (D-P1) | H | exists: sim | 6 (D-P1) | `converged_seconds` |
| L3 | `a_still_camera_converges_at_the_handheld_low_clamp` | P1.3, P1-low | Hl | exists: sim | — | `converged_seconds` |
| L3 | `a_relaunch_fills_the_view_from_the_store_without_network` | P1-warm, I14, I15 | H | exists: sim | — | `starts_total{source}` |
| L3 | `ten_still_minutes_cancel_nothing_flap_nothing_and_hold_the_digest` | P1.2 (I4), I5, I3 | D | exists: sim | — | `flaps_total`, `cancels_total` |
| L3 | `a_cold_start_draws_no_bare_ground` | I1 (D-A1), test first | D, H, Hl, X90 | new | 3 | `bare_item_steps_total` |
| L3 | `a_cold_start_covers_the_view_before_it_refines_any_of_it` | P1.1 (b) | D, H, Hl | new | 3 | `lost_tile_steps_total`, `drawn_tiles` |
| L3 | `the_floor_is_in_within_its_bound` | P1.1, T_F | D, H, Hl, X90 | new | 3 | `floor_seconds` |
| L3 | `a_still_camera_never_coarsens_what_it_drew` | P1.2 | D, H, Hl | new | 3 | `drawn_tiles`, the digest |
| L3 | `a_warm_start_converges_no_later_than_a_cold_one` | P1.3 (warm) | D, H | new | 3 | `converged_seconds`, `starts_total{source}` |
| L3 | `a_web_cold_start_converges_one_message_late` | P1.3 | W | new | 3 | `converged_seconds` |
| L4 | `no_black_ground.rs` › `ground_under_the_camera_is_never_black` | I1 on a real GPU, today's pipeline | D | exists: wgpu | 4.2 (ported) | the black scan |
| L4 | `headless_cold_start_is_never_black_and_converges` | I1, P1.1, P1.3 | D, H, Hl, X90 | new | 4.1, green at 5 | `lost_tile_steps_total`, `converged_seconds` |
| L4 | `relaunch_fills_from_the_store_without_network` | P1-warm, I14, I15 (spec §8) | D, H | new | 4.1, green at 5 | `starts_total{source}` |
| L4 | `still_camera_cancels_nothing` | I5, P1.2 (spec §8) | D, H | new | 4.1, green at 5 | `cancels_total` |
| L5 | `macos_cold_launch_is_never_black` | I1, P1.3 | D | new | 6 | black scan of the trace, `converged_seconds` |
| L5 | `ios_simulator_cold_launch_converges_at_low_memory` | P1-low | Hl | new | 6 | `converged_seconds`, `bytes` |
| L5 | `iphone_relaunch_fills_from_the_store` | P1-warm | H | new | 6 | `starts_total{source}` |
| L5 | `android_emulator_cold_launch_is_never_black` | I1, P1.3 | H | new | 6 | black scan, `converged_seconds` |

### 2.2 P2 — Zoom in

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `draw::the_parent_stays_drawn_while_any_wanted_child_is_missing` | P2.1, I9 | D | exists: core | — | `swaps_total{kind="refine"}` |
| L1 | `upload::siblings_of_a_refining_parent_form_one_swap_group` | P2.1 | D | exists: core | — | `swaps_total` |
| L1 | `requests::a_queued_request_that_left_the_need_set_never_starts` | P2.2 (a) | D | exists: core | — | `starts_total` |
| L1 | `cancellation::a_load_that_left_the_need_set_is_cancelled_after_the_grace_not_before` | P2.2 (b), I5 | D | exists: core | — | `cancels_total{reason="grace"}` |
| L1 | `requests::a_drawn_tiles_drape_is_requested_at_its_imagery_need` | P2.3 | D | exists: core | — | `underdraped_tiles` |
| L1 | `tiers::a_tile_already_on_the_gpu_is_never_requested_again` | P2.3 (a), I14 | D | exists: core | — | `starts_total` |
| L1 | `the_need_does_not_depend_on_the_terrain_level` | P2.3, I10 | D | exists: img | — | — |
| L1 | `a_plain_at_a_shallow_terrain_level_wears_imagery_deeper_than_its_terrain` | P2.3 | D | exists: img | — | `imagery_over_terrain` |
| L1 | `draw::refining_children_appear_together_each_with_its_drapes` | P2.1, I1 (D-A1) | D | new | 3 | `swap_gaps_total`, `bare_items` |
| L1 | `requests::a_drape_change_never_reloads_its_mesh` | P2.3 (a) | D | new | 3 | `starts_total{class="terrain_mesh"}` |
| L2 | `each_mesh_is_drawn_once_in_one_role` | the draw set's shape under any path | D, B | exists: prop | — | `drawn_tiles` |
| L2 | `a_zoom_in_never_draws_part_of_a_refining_parents_children` | P2.1, over generated zooms | D, H | new | 3 | `swap_gaps_total` |
| L2 | `nothing_starts_outside_the_need` | P2.2 (a), any path | D, H, B | new | 3 | `starts_total` |
| L3 | `over_the_plain_imagery_is_deeper_than_terrain` | P2.3 (b) | D | exists: sim | — | `imagery_over_terrain` |
| L3 | `a_zoom_out_to_200_km_and_back_never_shows_a_hole` | P2.1 on its way back, I1 | D | exists: sim | — | `lost_tile_steps_total` |
| L3 | `a_cold_fast_zoom_in_to_the_deepest_level_never_shows_a_hole` | P2.1, I1 | D, H, Hl, X90 | new | 3 | `lost_tile_steps_total`, `fallback_depth_max` |
| L3 | `a_slow_zoom_in_converges_at_the_deepest_level` | P2.4 | D, H, Hl | new | 3 | `converged_seconds` |
| L3 | `a_fast_zoom_in_starts_nothing_the_view_has_left` | P2.2; wasted starts goldened | D, H | new | 3 | `wasted_starts_total`, `cancels_total` |
| L3 | `over_the_plain_imagery_keeps_sharpening_after_terrain_stops` | P2.3 | D, H | new | 3 | `underdraped_tiles`, `imagery_over_terrain` |
| L3 | `a_film_dive_is_exact_every_frame` | I13 under P2 | B | new | 3 | `batch_steps_per_frame` |
| L4 | `fast_zoom_never_empties.rs` › `a_fast_zoom_never_leaves_a_tile_empty` | I1 on a real GPU | D | exists: wgpu | 4.2 (ported) | the black scan |
| L4 | `headless_fast_zoom_in_is_never_black` | P2.1, I1 | D, H, X90 | new | 4.1, green at 5 | `lost_tile_steps_total` |
| L4 | `coast_imagery_sharper_than_terrain` | P2.3, I10 (spec §8) | D | new | 4.1, green at 5 | `imagery_over_terrain` |
| L5 | `macos_scroll_zoom_in_is_never_black` | I1, P2.4 | D | new | 6 | black scan |
| L5 | `ios_pinch_zoom_in_is_never_black` | I1, P2.4 | H | new | 6 | black scan |
| L5 | `android_pinch_zoom_in_is_never_black` | I1, P2.4 | H | new | 6 | black scan |

### 2.3 P3 — Zoom out

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `draw::on_zoom_out_the_children_stay_until_the_parent_is_drawable` | P3.1 | D | exists: core | — | `drawn_tiles{role="held"}` |
| L1 | `hard_limit::a_coarsening_parent_uses_the_reserve_at_the_ceiling` | P3.3 | Hl | exists: core | — | `bytes`, `hard_limit_bytes` |
| L1 | `hard_limit::no_delivery_ever_crosses_the_hard_limit` | P3.3 | Hl | exists: core | — | `invariant_violations_total{invariant="I2"}` |
| L1 | `requests::a_zoom_out_ancestor_is_urgent` | P3.1 | D | exists: core | — | `starts_total` |
| L1 | `protection::ancestors_outlive_their_descendants` | P3.1, the chain | D | exists: core | — | `evictions_total` |
| L1 | `protection::a_drawn_tiles_fallback_chain_is_never_evicted` | P3.1, I11 | D | exists: core | — | violations I11 |
| L1 | `draw::a_finer_drape_is_kept_until_the_coarser_one_is_drawable` | P3.1 for imagery | D | exists: core | — | `swaps_total{kind="coarsen"}` |
| L1 | `protection::eviction_trims_to_the_low_watermark_and_waits_for_the_high_one` | P3.2 | D | exists: core | — | `bytes`, `target_bytes` |
| L1 | `draw::held_children_go_only_once_the_parent_is_drawable_and_dressed` | P3.1, I1 (D-A1) | D | new | 3 | `swap_gaps_total` |
| L2 | `no_class_ever_crosses_its_hard_limit` | P3.3 | D | exists: prop | — | violations I2 |
| L2 | `only_the_protected_set_overflows` | P3.2 (I2) | D | exists: prop | — | `protected_bytes` |
| L2 | `a_zoom_out_never_drops_a_held_tile_before_its_parent` | P3.1, over generated zooms | D, Hl | new | 3 | `swap_gaps_total` |
| L2 | `bytes_return_under_target_once_a_zoom_out_settles` | P3.2 | D, H, Hl | new | 3 | `bytes`, `over_target_seconds_total` |
| L3 | `a_zoom_out_at_the_hard_ceiling_never_deadlocks` | P3.3 | Hl | exists: sim | — | `bytes`, `sse_scale` |
| L3 | `after_a_zoom_out_and_back_the_drapes_are_sharp_again` | P3.4 | D | exists: sim | — | `underdraped_tiles` |
| L3 | `a_zoom_out_to_200_km_and_back_never_shows_a_hole` | P3.1 | D | exists: sim | — | `lost_tile_steps_total` |
| L3 | `a_fast_zoom_out_returns_under_target_within_the_bound` | P3.2 | D, H | new | 3 | `bytes`, `target_bytes` |
| L3 | `a_cold_climb_never_shows_a_hole` | P3.1 | D, H | new | 3 | `lost_tile_steps_total` |
| L3 | `a_zoom_out_on_a_headset_never_shows_a_hole_in_either_eye` | P3.1, I17 | X90 | new | 3 | `eye_lost_tiles` |
| L4 | `zoom_back_redrapes_sharp` | P3.4 (spec §8) | D, H | new | 4.1, green at 5 | `underdraped_tiles` |
| L4 | `headless_zoom_out_at_the_ceiling_is_never_black` | P3.3, I1 | Hl | new | 5 | black scan, `bytes` |
| L5 | `macos_zoom_out_to_orbit_and_back_is_never_black` | I1, P3.4 | D | new | 6 | black scan |
| L5 | `ios_simulator_low_memory_pinch_out_is_never_black` | I1, P3.3 | Hl | new | 6 | black scan, `bytes` |
| L5 | `android_emulator_pinch_out_under_trim_is_never_black` | I1, P3.3, I8 | H | new | 6 | black scan, `pressure_level` |

### 2.4 P4 — Rotation

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `protection::a_drawn_tile_is_never_evicted_even_far_over_budget` | P4.2 | D | exists: core | — | violations I11 |
| L1 | `protection::the_ancestor_drape_a_tile_wears_is_never_evicted` | P4.2, drapes | D | exists: core | — | violations I11 |
| L1 | `protection::eviction_takes_the_longest_unneeded_first_then_the_deepest` | P4.3 | D | exists: core | — | `evictions_total` |
| L1 | `requests::a_hole_outranks_a_blocker_which_outranks_the_ring_then_ancestors` | P4.1 (c) | D | exists: core | — | — |
| L1 | `cancellation::a_load_whose_tile_returns_within_the_grace_is_kept` | P4.3 (b) | D | exists: core | — | `cancels_total` |
| L1 | `stats::evict_then_request_is_counted_as_thrash` | P4.3 (c) | D | exists: core | — | `thrash_total` |
| L1 | `requests::a_ring_tile_entering_the_view_is_requested_as_a_hole` | P4.1 (c) | D | new | 3 | `starts_total` |
| L2 | `a_rotation_never_evicts_a_visible_tile` | P4.2, over generated yaws, orbits and tilts | D, H, Hl | new | 3 | `evictions_total` |
| L2 | `rotating_back_within_ten_seconds_fetches_nothing_again` | P4.3 (c) | D | new | 3 | `thrash_total` |
| L3 | `a_half_turn_in_one_second_converges` | P4.5 | D | exists: sim | — | `converged_seconds` |
| L3 | `back_and_forth_reuses_what_is_held` | P4.3 by the same mechanism (a pan) | D | exists: sim | — | `thrash_total`, `starts_total{source}` |
| L3 | `newly_visible_ground_is_covered_the_step_it_appears` | P4.1 (a), (b) | D, H, X90 | new | 3 | `lost_tile_steps_total`, `fallback_depth_max` |
| L3 | `a_head_rate_yaw_never_shows_a_hole` | P4.1, P4.5 at 300°/s | D, X72, X90, X120 | new | 3 | `lost_tile_steps_total`, `converged_seconds` |
| L3 | `an_orbit_keeps_its_target_exact` | P4.4 | D, H | new | 3 | `drawn_tiles{role="exact"}`, `underdraped_tiles` |
| L3 | `a_tilt_to_the_horizon_converges_without_a_hole` | P4.5, the frustum's pitch (§13.2) | D, H | new | 3 | `lost_tile_steps_total`, `converged_seconds` |
| L3 | `a_turn_and_back_flaps_nothing_and_refetches_nothing` | P4.3 | D, H | new | 3 | `flaps_total`, `thrash_total` |
| L3 | `a_rotation_never_evicts_what_is_still_in_view` | P4.2 | D, H, Hl | new | 3 | `evictions_total` |
| L4 | `rotation_converges` | P4.5 (spec §8) | D, H | new | 4.1, green at 5 | `converged_seconds` |
| L4 | `headless_head_rate_yaw_is_never_black_in_either_eye` | P4.1, I17 | X90 | new | 5 | per-eye black scan |
| L5 | `macos_drag_orbit_is_never_black` | I1, P4.5 | D | new | 6 | black scan |
| L5 | `ios_twist_rotation_is_never_black` | I1, P4.5 (the touch script's `twist`) | H | new | 6 | black scan |
| L5 | `android_drag_orbit_is_never_black` | I1, P4.5 | H | new | 6 | black scan |

### 2.5 P5 — Orientation change

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `the_footprint_follows_the_lens_not_a_fixed_focal` | §13.2 | D, H | new (img) | 3 | — |
| L1 | `a_scale_change_at_constant_logical_size_changes_no_footprint` | P5.5, D-L1 | D | new (img) | 3 | — |
| L1 | `an_asymmetric_view_culls_by_its_four_angles` | §13.2, D-A7 | X90 | new (trav) | 3 | — |
| L1 | `the_focal_is_the_sharper_axis_in_logical_pixels` | §13.2 | D, H, X90 | new (trav) | 3 | — |
| L1 | `draw::a_widened_need_covers_its_new_tiles_by_fallback_in_the_same_step` | P5.1 | D | new | 3 | `drawn_tiles{role="fallback"}` |
| L2 | `a_resize_never_leaves_a_hole` | P5.1, over generated lens changes | D, H | new | 3 | `lost_tile_steps_total` |
| L2 | `the_need_is_invariant_under_a_scale_change` | P5.5 | D, W | new | 3 | — |
| L3 | `a_portrait_landscape_rotation_never_shows_a_hole` | P5.1, P5.6 | H, Hl | new | 3 | `lost_tile_steps_total`, `converged_seconds` |
| L3 | `ten_rotations_fetch_nothing_after_the_first` | P5.4 | H, Hl | new | 3 | `starts_total{source="network"}`, `thrash_total` |
| L3 | `a_resize_decides_on_the_new_viewport_the_same_step` | P5.2 | D, W | new | 3 | `/views` and the need's digest |
| L3 | `a_resize_starts_nothing_outside_the_new_need` | P5.3 | D, H | new | 3 | `starts_total`, `wasted_starts_total` |
| L3 | `a_split_screen_shrink_comes_back_under_target_without_a_hole` | P5.6, P3.2 | H | new | 3 | `bytes`, `lost_tile_steps_total` |
| L3 | `a_scale_change_alone_decides_nothing_new` | P5.5 | D, W | new | 3 | the digest |
| L4 | `headless_resize_is_never_black` | P5.1 (desktop sizes; portrait and landscape) | D, H | new | 4.1, green at 5 | black scan |
| L4 | `headless_scale_change_keeps_the_digest` | P5.5 | D | new | 5 | the digest |
| L5 | `macos_window_resize_is_never_black` | P5.1 | D | new | 6 | black scan |
| L5 | `ios_device_rotation_is_never_black` | P5.1, P5.4 | H, Hl | new | 6 | black scan, `starts_total{source}` |
| L5 | `ipad_split_screen_is_never_black` | P5.1, P5.6 (the split itself by hand) | H | new | 6 | black scan |
| L5 | `android_emulator_rotation_is_never_black` | P5.1, P5.4 | H | new | 6 | black scan |

### 2.6 P6 — Stereo

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `tests::second_view_unions_the_selection` | I16, the union | D2 | exists: trav | — | — |
| L1 | `tests::refinement_uses_the_most_demanding_view` | I16, the largest need | D2 | exists: trav | — | — |
| L1 | `two_eyes_list_a_shared_tile_once_with_its_largest_need` | §13.3 (imagery footprint included) | X90 | new (trav) | 3 | — |
| L1 | `deadline::a_frame_deadline_is_its_share_of_the_display_period` | I18 | X72, X90, X120 | new | 3 | `frame_deadline_bytes` |
| L1 | `deadline::the_first_item_always_goes_and_a_swap_group_goes_whole` | I18, I9 | X90 | new | 3 | `deadline_deferrals_total` |
| L2 | `two_eyes_cost_what_their_union_costs` | I16, P6.1, over generated eye pairs (63 mm to 630 m) | X90, D2 | new | 3 | `bytes`, `uploads_total` |
| L2 | `every_eye_is_covered_by_the_one_draw_set` | I17, P6.2 | X90, D2 | new | 3 | `eye_lost_tiles` |
| L2 | `any_deferral_pattern_keeps_the_ground_covered` | I18, P6.3 | X90 | new | 3 | `deadline_deferrals_total`, `lost_tile_steps_total` |
| L3 | `a_stereo_head_never_shows_a_hole_in_either_eye` | I17, P6.2 | X72, X90, X120 | new | 3 | `eye_lost_tiles`, `eye_bare_items` |
| L3 | `a_stereo_pair_costs_its_union` | I16, P6.1 (the same digests as the union need) | X90 | new | 3 | the digest, `bytes` |
| L3 | `hyperstereo_costs_its_union_and_covers_both_eyes` | I16, I17 | D2 | new | 3 | `bytes`, `eye_lost_tiles` |
| L3 | `uploads_fit_the_refresh_deadline_at_72_90_and_120_hz` | I18, P6.3 | X72, X90, X120 | new | 3 | `frame_upload_bytes`, `deadline_deferrals_total` |
| L3 | `decide_work_grows_with_changes_not_with_the_view` | P6.4, I6 | X90 | new | 3 | `decide_work` |
| L3 | `a_stereo_head_converges_after_it_stops` | P6.5 | X90 | new | 3 | `converged_seconds` |
| L3 | `a_stereo_film_is_exact_in_both_eyes` | I13, I16 | B | new | 3 | `batch_steps_per_frame` |
| L4 | `headless_stereo_is_never_black_in_either_eye` | I17 | X90 | new | 4.1, green at 5 | per-eye black scan |
| L4 | `headless_stereo_uploads_each_resource_once` | I16 | X90 | new | 5 | `uploads_total` |
| L4 | `headless_frame_uploads_fit_the_90_hz_deadline` | I18 | X90 | new | 5 | `frame_upload_bytes`, `frame_upload_seconds` |
| L5 | `android_emulator_stereo_profile_is_never_black` | I17, P6.3 (two eyes side by side on a phone surface) | X90 | new | 6 | per-eye black scan |
| L5 | `quest_stereo_session_is_never_black` | I17, P6.3 on a headset | X72, X90, X120 | later (needs the OpenXR shell, outside this plan) | — | per-eye black scan |

### 2.7 Every pattern: cross-cutting cells

| Level | Test | Proves | Profiles | Status | Phase | Signals |
|---|---|---|---|---|---|---|
| L1 | `draw::each_layer_falls_back_on_its_own` | D-A4 | D | new | 3 | `bare_items` |
| L1 | `draw::a_layer_switch_keeps_the_old_layer_worn_until_the_new_one_is_drawable` | I1 (D-A1), D-A4 | D | new | 3 | `swaps_total{kind="layer"}` |
| L1 | `refresh::an_epoch_refreshes_only_its_layer` | D-A4 | D | new | 3 | `starts_total{class="drape"}` |
| L1 | `requests::two_layers_of_one_tile_are_two_requests_and_one_draw_item` | D-A4, I16 by layer | D | new | 3 | `starts_total` |
| L1 | `pressure::a_memory_warning_trims_to_the_new_target_without_a_hole` | I8 | H | exists: core | — | `target_bytes`, `lost_tiles` |
| L1 | `pressure::background_keeps_only_the_floor_and_the_drawn_set` | I8 | H | exists: core | — | `bytes` |
| L1 | `pressure::a_runtime_retarget_shrinks_targets_without_a_hole` | I8, D-A6 (`Retarget`) | H, W | exists: core | — | `target_bytes` |
| L1 | `generations::a_stale_generation_is_never_delivered` | I5 | D | exists: core | — | `stale_results_total` |
| L1 | `determinism::the_same_inputs_give_the_same_digest_stream` | I12 | D | exists: core | — | the digest |
| L2 | `protected_resources_are_never_evicted` | I11 | D | exists: prop | — | violations I11 |
| L2 | `decisions_do_not_depend_on_need_order` | I12 | D | exists: prop | — | the digest |
| L2 | `the_same_script_gives_the_same_digests` | I12 | D | exists: prop | — | the digest |
| L2 | `the_ledger_equals_the_sum_of_the_states` | the ledger | D | exists: prop | — | `bytes` |
| L2 | `only_fresh_results_are_delivered` | I5 | D | exists: prop | — | `stale_results_total` |
| L2 | `nothing_held_is_fetched_again` | I14 | D | exists: prop | — | `starts_total{source}` |
| L2 | `quotas_hold_per_class` | the quotas | D | exists: prop | — | `in_flight` |
| L2 | `plans_and_queues_are_bounded` | I7 | D | exists: prop | — | `request_heap_entries` |
| L2 | `a_ready_batch_frame_is_exact` | I13 | B | exists: prop | — | violations I13 |
| L2 | `the_engine_agrees_with_the_reference_model_step_by_step` | I2, I5, I7, I11, the ledger, P2.1, P3.1 (§4) | D, Hl, B | new (`budget_model.rs`) | 3 | `invariant_violations_total` |
| L2 | `a_generated_path_never_breaks_a_per_step_check` | every record-local check of spec §16.2, over the generator of §3 | D, H, Hl, X90 | new | 3 | `invariant_violations_total` |
| L3 | `golden_decision_digests_hold` | I12 on four scenarios | D | exists: sim | — | the digest |
| L3 | `pattern_goldens_hold` | I12 for every pattern script and profile | D, H, Hl, X90, B | new | 3 | the digest |
| L3 | `five_simulated_minutes_keep_flat_latency_and_bounded_state` | I6, I7 | D | exists: sim (`soak.rs`) | — | `load_seconds`, `tracked_resources` |
| L3 | `one_simulated_hour_keeps_flat_latency_and_bounded_state` | I6, I7 (ignored: at the gates) | D | exists: sim (`soak.rs`) | — | the same |
| L3 | `an_hour_with_resizes_and_stereo_head_motion_keeps_flat_slopes` | I6, I7 under P5 and P6 (ignored: at the gates) | H, X90 | new | 3 | the same, `decide_work` |
| L3 | `a_memory_warning_mid_flight_trims_without_a_hole` | I8 under P2/P3 | H | exists: sim | — | `target_bytes`, `lost_tile_steps_total` |
| L3 | `thermal_serious_halves_uploads_without_a_hole` | I8 | H | exists: sim | — | `upload_plan_bytes`, `thermal_level` |
| L3 | `backgrounding_and_foregrounding_refill_without_a_hole` | I8 | H | exists: sim | — | `bytes`, `converged_seconds` |
| L3 | `a_lossy_stalling_network_never_blocks_the_view` | D1, I3 | D | exists: sim | — | `failures_total`, `cancels_total{reason="timeout"}` |
| L3 | `a_lying_store_index_costs_one_miss_per_lie` | §4b | D | exists: sim | — | `store_misses_total` |
| L3 | `a_300_frame_film_is_exact_every_frame` | I13 | B | exists: sim | — | `batch_steps_per_frame` |
| L3 | `a_film_does_not_depend_on_the_network` | I13 | B | exists: sim | — | the digest |
| L3 | `a_film_does_not_depend_on_how_frames_are_split` | I13 | B | exists: sim | — | the digest |
| L3 | `architecture_shared_arbiter_against_split_budgets` | D-A0's evidence (an experiment, ignored) | D, H, Hl | removed in Phase 3 with the fixed per-class split it re-split (D-A3 decided, 2026-10-20); its results are the architecture document's §3 and appendix | — | — |
| L3 | `architecture_pooled_ledger_against_fixed_split` | D-A3's evidence (an experiment, ignored) | D, H, Hl | done in Phase 3 (`fc31f81`), then removed with the fixed split once D-A3 was decided (pooled, 2026-10-20); its results are the architecture document's §7 | 3 | — |
| L3 | `a_remote_session_stays_within_its_declared_memory` | I2 and the ledger from `Ack` bytes, with decisions and acks a round trip late (D-A5) | R | new | 3 | `bytes`, `hard_limit_bytes` |
| L3 | `a_remote_film_is_exact_with_acks_a_round_trip_late` | I13 under a remote session | R, B | new | 3 | `batch_steps_per_frame` |
| L4 | `upload_plan_respects_bytes` | I18 with the plan's cap (spec §8) | D, H | new | 4.1, green at 5 | `upload_plan_bytes`, `frame_upload_bytes` |
| L4 | `headless_memory_warning_mid_zoom_is_never_black` | I8, I1 | H | new | 5 | black scan, `target_bytes` |
| L4 | `a_fallback_never_paints_over_its_drawn_siblings` | D3: the fallback pass replaces the stand-ins (`stand_ins.rs` rewritten; `no_two_surfaces.rs` kept) | D | new | 4.2 | the black scan, a two-surface scan |
| L5 | `ios_simulator_background_and_back_refills_without_a_hole` | I8 | H | new | 6 | black scan, `pressure_level` |
| L5 | `ios_simulator_memory_warning_trims_without_a_hole` | I8 | H | new | 6 | `target_bytes`, `lost_tile_steps_total` |
| L5 | `android_emulator_trim_levels_trim_without_a_hole` | I8 | H | new | 6 | `pressure_level`, `target_bytes` |
| L5 | `iphone_thirty_minute_tour_keeps_flat_curves` | I6, I7 on a device | H | new | 6 | `load_seconds`, `decide_seconds` |
| L5 | `macos_thirty_minute_replay_keeps_flat_curves` | I6, I7 on a desktop | D | new | 6 | the same |

The simulation's per-step check `Sim::check` also grows, in Phase 3, before any scenario:
`bare == 0` once the floor is in (D-A1, seen red on today's 4–8 bare tile-steps); per-eye
coverage against each eye's frustum (I17); the bytes uploaded per render frame within the
deadline (I18); P4.2's eviction rule. Every scenario then checks them at every step.

### 2.8 O — the signals move with their behaviour

Each test drives the behaviour and its control (the same script without the behaviour), and
asserts that the signal moves in one and stays still in the other. Each is then seen red
against a hand mutation that drops the signal's update (§9).

| Test | Signal | Behaviour | Phase |
|---|---|---|---|
| `the_lost_tiles_signal_counts_a_wanted_tile_with_no_cover` | `lost_tiles`, `lost_tile_steps_total` | I1 | 3 |
| `the_bare_items_signal_counts_an_item_without_its_drapes` | `bare_items`, `bare_item_steps_total` | I1 (D-A1) | 3 |
| `the_floor_signal_rises_once_mesh_and_drape_are_drawable` | `floor_in`, `floor_seconds` | P1.1 | 3 |
| `the_bytes_signal_splits_drawable_from_in_transit` | `bytes{class,state}` | I2 | 3 |
| `the_over_target_seconds_run_only_while_over` | `over_target_seconds_total` | I2, P3.2 | 3 |
| `the_converged_seconds_measure_from_the_stop` | `converged_seconds`, `unconverged_seconds` | I3 | 3 |
| `the_flaps_signal_counts_a_second_change_within_ten_seconds` | `flaps_total` | I4 | 3 |
| `the_cancels_signal_names_the_grace_and_the_timeout` | `cancels_total{reason}` | I5 | 3 |
| `the_wasted_starts_signal_counts_a_start_the_view_left` | `wasted_starts_total` | P2.2 | 3 |
| `the_starts_signal_splits_local_from_network` | `starts_total{class,source}` | I14, I15 | 3 |
| `the_local_waits_signal_counts_a_stored_tile_left_behind_the_network` | `local_waits_total` | I15 | 3 |
| `the_thrash_signal_counts_a_refetch_within_ten_seconds` | `thrash_total`, `rereads_total` | I14 | 3 |
| `the_evictions_signal_names_its_reason` | `evictions_total{class,reason}` | I11, P3.2 | 3 |
| `the_swaps_signal_counts_refines_and_coarsenings_and_never_a_gap` | `swaps_total{kind}`, `swap_gaps_total` | I9 | 3 |
| `the_underdraped_and_gap_signals_follow_the_drapes_worn` | `underdraped_tiles`, `imagery_over_terrain` | I10 | 3 |
| `the_fallback_depth_holds_the_deepest_climb_until_published` | `fallback_depth_max` | P2, P4.1 | 3 |
| `the_pressure_and_thermal_levels_follow_the_platform` | `pressure_level`, `thermal_level`, `target_bytes` | I8 | 3 |
| `the_decide_work_signal_follows_the_changes` | `decide_work`, `decide_work_total` | P6.4 | 3 |
| `the_decide_peak_holds_the_worst_step_until_published` | `decide_peak_seconds` | P6.4 | 3 |
| `the_deferrals_signal_counts_a_deadline_overrun` | `deadline_deferrals_total`, `frame_upload_bytes` | I18 | 3 |
| `the_eye_signals_name_the_eye_that_lost_ground` | `eye_lost_tiles{eye}`, `eye_bare_items{eye}` | I17 | 3 |
| `the_uploads_signal_counts_one_upload_per_resource_for_two_eyes` | `uploads_total` | I16 | 3 |
| `the_violations_signal_names_the_invariant_broken` | `invariant_violations_total{invariant}` | every record-local check, on fabricated records | 3 |
| `labels_stay_within_their_closed_sets` | every label (spec §16.5), over a soak | bounded cardinality | 3 |
| `the_step_topic_replays_to_the_same_digests` | `/budget/step`, `/budget/events`, `/views` | I12 from a tape (tuile-tape) | 4.2 |
| `the_views_topic_records_each_eyes_lens` | `/views` | P5.2, P5.5 | 4.2 |
| `the_prometheus_names_match_the_signals` | every `BudgetSignals` field, its name, unit and labels (tuile-metrics) | one vocabulary | 4.2 |
| `the_worker_posts_the_same_signals_the_native_meter_reads` | `ToPage::Signals` | the web sink | 4.2 |
| `the_field_log_holds_no_coordinate` | the field log's topics and fields (spec §16.9) | privacy | 4.2 |

## 3. L2: a continuous camera-path generator

The properties today draw each step's view from a seed: a random quadtree cut, unrelated to
the step before (`budget_properties.rs` `view`). That finds what any order of needs breaks,
not what a motion breaks. The pattern properties get paths instead:

- **Segments.** A case is a list of segments, each `still(d)`, `zoom(rate, d)` (rate up to ×10
  per second, in or out), `yaw(rate, d)` (up to 300°/s), `orbit(radius, period, d)`,
  `tilt(rate, d)`, `lens(width, height, scale)` (a resize, a rotation, a scale change, applied
  in one step), `eyes(ipd)` (one eye, or two at 63 mm to 630 m), `pressure(level)`; durations
  of 0.1–5 s; stepped at the profile's dt.
- **World.** The simulation's `world.rs` (its regions, traversal, lens and rig) moves into
  `tests/budget_common/`, so that the properties and the scenarios share one world; the
  properties run it at a smaller depth to stay fast.
- **Network.** Arrivals come from the seeded `SimNet` with the case's own seed, plus the
  existing random failures and store misses.
- **Shrinking.** proptest shrinks to fewer and shorter segments, so a counterexample reads as
  a short camera script, replayable as a scenario.
- **Vacuity.** A property that needs the floor in or a frame complete rejects the cases that
  never get there (about a third of today's random scripts do); the reject cap grows with
  the case count (`config()` already does this).

### The reference model (`proptest-state-machine`)

A second, small, obviously correct model of the budget's bookkeeping, run beside the engine
transition by transition: `Step(path segment)`, `Load`, `Upload`, `Fail`, `StoreMiss`,
`Pressure`, `Wait`. It does not decide (it re-implements no priority, no eviction order);
it holds, per resource, the state the engine's decisions and the host's events imply, and
after every transition it checks the engine against it:

- the ledger equals the model's sum per class and state;
- every eviction is outside the model's protected set (drawn ∪ worn ∪ in transit ∪ floor ∪
  the chain of the drawn cut ∪ the drawable half of a swap in progress);
- every delivery carries the model's current generation;
- every drawn item is drawable in the model and dressed;
- no resource has two loads in flight; no class is over its hard limit;
- a refining parent's children all appear in one step; a coarsening parent appears before
  its held children go.

The model is the oracle of safety; the engine stays the only implementation of policy.
`proptest-state-machine` is a dev-dependency of `tuile-core`, outside every wasm path.

## 4. L3: what the simulation gains

- **The lens** (§13): `world.rs` replaces `FOCAL` and the cone of `sight` with a frustum from
  four angles, the pitch and the aspect; the SSE and the texel footprint take the lens's
  focal in logical pixels.
- **The rig**: N eyes with their own poses; the traversal returns the union (§13.3); `Sim`
  keeps each eye's frustum for the per-eye check.
- **Scripts**: orbit, tilt, head motion (jitter, look-around, flicks, walking), resize and
  rotation events, split screen, the cold zoom-in (P1–P6, §12).
- **Profiles**: `Profile::headset(hz)` and `Profile::web()` (the executor a step behind the
  budget), beside desktop, handheld, handheld-low and film.
- **The executor**: `SimGpu` uploads the plan through `FrameDeadline::take` per render frame
  (today it uploads the whole plan every step), reports deferrals, and acks what it
  uploaded.
- **Goldens**: one digest per pattern script and profile (`pattern_goldens_hold`,
  `TUILE_BLESS_BUDGET=1` to re-record, a reviewed update).
- **The soak**: the tour gains resizes and stereo head motion.

## 5. L4: the headless real-GPU runs

The headless viewer and its control API (spec §4c, Phase 4.1) run the pattern scripts of
§12 on the real app: the same frame loop, the real `wgpu` (Metal on the Mac, lavapipe on
Linux), the real store, a recorded or simulated network, an offscreen surface of the lens's
size and scale, one surface per eye in stereo, a stepped clock and an injected profile
(device memory, display rate).

Each run checks, every frame: the black scan of each eye's picture (where the ground should
be, as `tuile-scan-black` does), the budget's signals through `budget()`, the convergence
bound, no class over its hard limit, and the decision digest against the L4 golden of the
same script and recorded network. The L3 goldens do not carry over: the simulation's world
and traversal are not the viewer's.

Written in 4.1 against today's viewer, so the tests that today's code fails start red
(`#[ignore = "red until the cut-over"]`), seen failing; they turn green at Phase 5.

The existing GPU tests (`no_black_ground.rs`, `fast_zoom_never_empties.rs`, `headless.rs`,
`no_two_surfaces.rs`, `imagery_passes.rs`, `no_seams.rs`, `eager_batches.rs`, `overlay.rs`)
are ported at the cut-over; `stand_ins.rs` is rewritten around the fallback pass (D3). In
CI they run with `TUILE_REQUIRE_GPU=1`, so a missing adapter fails instead of skipping.

## 6. L5: the devices

| Platform | Driven by | Patterns | Read through |
|---|---|---|---|
| **macOS** | the app (`scripts/try-viewer.sh mac`) with its control socket for moves, resizes and `pressure`; AppleScript through System Events for the window's own resize; `screencapture` for the worst frame | P1–P5 | the trace (`TUILE_TRACE`) scanned by `tuile-scan-black`; `budget()` live |
| **iOS simulator**, normal and low memory | an XCUITest target in `crates/tuile-viewer/ios/project.yml`: `XCUIDevice.shared.orientation` for rotations, `press(.home)` then `activate()` for the background, `pinch(withScale:velocity:)` and `rotate(_:withVelocity:)` for gestures; low memory by injected device memory at launch (a tuning value, `launchEnvironment`); a memory warning through the control socket's `pressure(warning)`, which drives the same handler as winit's `memory_warning` — the Simulator's own "Simulate Memory Warning" stays a manual check of the platform path; `xcrun simctl io booted screenshot` | P1–P5, I8 | the trace and the field log from the app's container (`simctl get_app_container`) |
| **iPhone** | the same XCUITest target (`xcodebuild test` on the device), or the touch script (`TUILE_TOUCH_SCRIPT=tour`) for the 30-minute session, launched with `xcrun devicectl device process launch`; a debug build, since release builds carry no control socket | P1–P5, I6, I8 | the field log and the trace, pulled with `xcrun devicectl device copy from` |
| **Android emulator** | `scripts/try-viewer.sh android`; `adb emu rotate`; `adb shell am send-trim-memory <package> <level>` for each `onTrimMemory` level; `adb forward` to the control socket; gestures through the socket or the touch script; `adb exec-out screencap -p` | P1–P5, I8; P6 with the stereo profile on a phone surface | the trace and the field log through `adb exec-out run-as <package>` |
| **Headset** (later) | a Quest is Android: the Android harness with the stereo profile; the OpenXR shell is outside this plan | P6 | as Android |

Every device run is judged by the assertions of L4: black scans, the signals, the bounds. A
run's worst frame is opened by eye, as every render is.

## 7. CI and the gates

| When | What runs |
|---|---|
| **Every PR** | L1 (debug); L2 at 10 000 cases; L3 in release (the scenarios, the goldens, the five-minute soak), as the `budget-sim` job of `feat/budget-core` does today; O with them; L4 on the Linux job, lavapipe, `TUILE_REQUIRE_GPU=1`; the wasm guard. Whether the macOS runner gives `wgpu` a Metal adapter is checked in Phase 4.1; if it does not, the Mac's L4 runs at the gates |
| **Each phase gate** (validation of the plan, not a standing apparatus) | `cargo-mutants` on the budget module and the hand mutations (§9); L2 at 1 000 000 cases; the one-hour soaks; the L5 device runs; the architecture experiments when D-A3 is to be closed. Heavy runs go to the remote builders, one at a time, never two on the laptop |
| **Nightly** | decided after the gates have run, from what they found |

## 8. What each level does not cover

- **L1** checks one rule against a hand-built need. It proves the rule, not that the rules
  compose, and nothing about the traversal, time-continuous motion or real costs.
- **L2** samples, it does not prove: 10 000 cases of a small tree (depth 4 today). The
  generated paths are only as varied as their segment kinds; the reference model is only as
  strong as its simplicity, and checks safety, not quality (blur, convergence speed). About
  a third of the cases are vacuous and rejected.
- **L3** is a flat synthetic world: no sphere, no real terrain errors, no 3D Tiles content
  (the simulation charges `TerrainMesh` and `Drape` only, so `Tiles3dMesh` and `Overlay` are
  untested beyond L1 and L2), costs from a formula, a traversal (`world.rs`) that stands in
  for the real one — a bug in `crates/tuile-core/src/traversal.rs` is not caught here. Its
  `decide` timings are one machine's wall clock.
- **L4** is the real viewer on a real GPU, but with a recorded or simulated network, an
  offscreen surface (no compositor, no vsync, no real display rate), lavapipe or the Mac's
  Metal rather than a phone's driver, and two offscreen eyes rather than an OpenXR runtime.
  The black scan finds holes, not blur or the wrong imagery.
- **L5** has the real OS signals and devices, and none of the determinism: real networks,
  real heat, few runs, read statistically. Some steps stay manual (the Simulator's memory
  warning, an iPad's split screen, the iPhone session). The low-memory simulator is an
  injected budget, not a real jetsam limit; the Android emulator's GPU is the host's. No
  headset until the OpenXR shell exists.
- **O** shows that each signal moves with its behaviour. It does not show that a dashboard is
  readable or an alert worth waking for; a 500 ms publication still aliases what one step
  does, which is why one-step quantities have counters.
- **Mutation** (§9): `cargo-mutants` mutates syntax (operators, constants, returned values,
  deleted calls), not the semantic faults the hand list holds (arrivals applied in arrival
  order, relaxation per frame); equivalent mutants need a person's triage; a full run is
  hours.

## 9. Mutation: validating the plan

Mutation testing here validates the plan — it checks that the tests written from the spec
catch the faults the spec forbids. It is not a CI apparatus.

**At each phase gate** (3, 4.2, 5):

```bash
# On the remote builder, one heavy run at a time; never on the laptop with another build.
CARGO_INCREMENTAL=0 PROPTEST_CASES=256 cargo mutants -p tuile-core \
  -f 'crates/tuile-core/src/budget/**/*.rs' --profile release -j 2 \
  -o target/mutants-budget \
  -- --test budget_core --test budget_imagery --test budget_properties \
     --test budget_model --test budget_signals --test budget_sim
# budget_model and budget_signals are Phase 3's new suites; until they exist, leave them out.
```

- Scope: `crates/tuile-core/src/budget/`, later the seams the cut-over adds (the runtime's
  budget wiring, the pump's executor, `FrameDeadline`).
- `.cargo/mutants.toml` (Phase 3) keeps the scope (`examine_globs`) and the test binaries
  (`additional_cargo_test_args`), so the gate's command is `cargo mutants`.
- **Every missed mutant** is either killed by a new test, written here first, or shows a
  hole in the spec or this plan, which is fixed and goes back to Laurent's review (a decision
  gate of the plan). An equivalent mutant is recorded with the reason it is equivalent.
- The run's summary (caught, missed, timeout, unviable; each miss and its fate) is added to
  this document, per gate.

**The baseline: 24 hand mutations**, from commit `71874c5` (`0ba061f` before the rebase).
Each piece of the implementation was reverted once and the suites `budget_core`,
`budget_imagery` and `budget_properties` run at 64 property cases; all 24 were killed. They
are re-run at every gate.

| | Mutation | Tests failed | Among them |
|---|---|---|---|
| M1 | eviction ignores the drawn set | 11 | `hard_limit::no_delivery_ever_crosses_the_hard_limit`, `pressure::background_keeps_only_the_floor_and_the_drawn_set` |
| M2 | the floor evictable | 3 | `the_pinned_floor_is_never_evicted`, `protected_resources_are_never_evicted` |
| M3 | no cancel grace | 2 | `cancellation::a_load_whose_tile_returns_within_the_grace_is_kept` |
| M4 | no generation check on load | 1 | `generations::a_stale_generation_is_never_delivered` |
| M5 | no class quota | 9 | `quotas_hold_per_class`, `requests::an_urgent_request_preempts_in_flight_preload_rather_than_wait` |
| M6 | local reads share the network quota | 4 | `tiers::local_reads_never_wait_behind_network_work`, `nothing_held_is_fetched_again` |
| M7 | the ledger forgets GPU bytes | 21 | `batch::release_beyond_keeps_the_last_frame_and_is_deterministic`, `bounded::tracked_state_stays_bounded_over_a_long_pan` |
| M8 | the upload plan uncapped | 3 | `upload::the_upload_plan_never_exceeds_its_bytes_except_its_first_item`, `plans_and_queues_are_bounded` |
| M9 | the best drape is any drape | 4 | `the_best_drape_is_never_blurrier_than_necessary`, `at_equal_level_the_own_drape_wins` |
| M10 | no imagery hysteresis | 1 | `the_need_does_not_flap_within_half_a_level` |
| M11 | no imagery cap | 4 | `imagery_stays_within_three_levels_above_terrain`, `requests::a_drape_is_never_requested_beyond_the_cap_over_terrain` |
| M12 | arrivals applied in arrival order | 1 | `generations::contradictory_arrivals_resolve_the_same_whatever_their_order` (it survived the first pass) |
| M13 | relaxation per frame | 1 | `pressure::relaxation_rises_with_time_not_frames` |
| M14 | the hard limit ignored | 2 | `hard_limit::no_delivery_ever_crosses_the_hard_limit`, `no_class_ever_crosses_its_hard_limit` |
| M15 | no held descendants | 1 | `draw::on_zoom_out_the_children_stay_until_the_parent_is_drawable` |
| M16 | no refresh | 2 | `refresh::a_refresh_never_preempts`, `refresh::an_epoch_change_refreshes_resident_drapes_without_a_hole` |
| M17 | presence ignored | 5 | `nothing_held_is_fetched_again`, `tiers::a_stored_tile_is_read_locally_without_a_network_slot` |
| M18 | no row alignment | 2 | `ledger::an_unaligned_row_stages_at_the_next_256_bytes` |
| M19 | the stats digest constant | 1 | `stats_digest::the_stats_digest_follows_every_field` |
| M20 | a drape never re-requested once one is worn | 3 | `draw::a_drawn_tiles_drape_follows_its_need_both_ways`, `requests::a_drawable_tile_wearing_a_coarser_drape_asks_at_the_drape_tier` |
| M21 | no preemption | 1 | `requests::an_urgent_request_preempts_in_flight_preload_rather_than_wait` |
| M22 | the halves of a swap unprotected | 1 | `only_the_protected_set_overflows` |
| M23 | NaN not rejected | 1 | `determinism::a_non_finite_need_counts_as_zero_and_is_reported` |
| M24 | no load timeout | 1 | `cancellation::a_load_that_never_answers_is_timed_out_and_retried` |

Read against this plan:
- **Ten are killed by one test only** (M4, M10, M12, M13, M15, M19, M21, M22, M23, M24). One
  weakened test and the fault passes. Phase 3 gives each a second killer at another level
  (L2 or L3), recorded in this table when it exists.
- **The simulation was not run** against them. Whether the scenarios kill them is unknown;
  the Phase 3 gate runs L3 too.

**New hand mutations for the new behaviour** (Phase 3, each seen killed):

| | Mutation | Must be killed by |
|---|---|---|
| M25 | the floor drawn without its drape | `draw::the_floor_is_drawn_only_with_its_drape`, `a_cold_start_draws_no_bare_ground` |
| M26 | a bare item allowed once the floor is in | `every_drawn_item_wears_its_drapes_once_the_floor_is_in` |
| M27 | a tile seen by two eyes charged per eye | `two_eyes_cost_what_their_union_costs`, `a_stereo_pair_costs_its_union` |
| M28 | one eye drawn from the previous step's draw set | `every_eye_is_covered_by_the_one_draw_set` |
| M29 | the executor ignores the deadline | `uploads_fit_the_refresh_deadline_at_72_90_and_120_hz` |
| M30 | the lens ignored (the fixed focal back) | `the_footprint_follows_the_lens_not_a_fixed_focal`, `a_portrait_landscape_rotation_never_shows_a_hole` |
| M31 | the scale factor counted (physical pixels) | `the_need_is_invariant_under_a_scale_change` |
| M32 | pitch ignored by the frustum | `an_asymmetric_view_culls_by_its_four_angles`, `a_tilt_to_the_horizon_converges_without_a_hole` |
| M33 | the ring not listed | `newly_visible_ground_is_covered_the_step_it_appears` |
| M34 | `decide` rescans everything it tracks | `decide_work_grows_with_changes_not_with_the_view` |
| M35 | a held child dropped before its parent is dressed | `draw::held_children_go_only_once_the_parent_is_drawable_and_dressed` |
| M36 | a layer falls back on another layer's drape | `draw::each_layer_falls_back_on_its_own` |
| M37 | each signal's update dropped, one at a time | its O test (§2.8) |

## 10. Phases

- **Phase 3** (`feat/budget-core`): every L1, L2, L3 and O cell marked 3, test first;
  `Sim::check`'s new checks first of all, since D-A1's test must be seen red on today's
  bare floor; the gate of §9.
- **Phase 4.1**: the headless harness and the L4 cells marked 4.1, red where today's viewer
  fails.
- **Phase 4.2** (the cut-over): the ported GPU tests, `a_fallback_never_paints_over_its_drawn_siblings`,
  the O cells of the sinks (tape, Prometheus names, the worker's channel, the field log).
- **Phase 5**: every L4 cell, every pattern × {D, H, Hl, X90}, green.
- **Phase 6**: every L5 cell.

## 11. Open points this plan depends on

- **D-P1**, decided on evidence: the handheld bound is set from the L5 iPhone sessions recorded
  in MCAP, which also calibrate the simulation's network model;
  `a_still_camera_converges_within_four_seconds_on_a_phone` stays red and ignored until then.
- **D-L1**, logical pixels: P5.5's cells assume it.
- **D-A3**, the pooled ledger: decided on 2026-10-20 (pooled, architecture §7); the goldens
  were re-recorded once, in the commit that made the pool the policy.
- **D3's wording** (`Role::Held` against `Role::Fallback`, spec §10): the cells of P1 and P3
  assume both roles stay as `decisions.rs` defines them.
- **P2's fallback climb** has no bound yet; the first green run proposes one, for review.
- **`ClientMessage::Held`** (open, D-A5): a remote consumer with local tiers would need a cell
  of its own (I14 across a session); none is written until it is decided.
- **D-T1**, the session's transport: the remote-session cells simulate a round trip, not a
  transport; the transport's own tests come with tuile-server (M2).

## 12. Counts

Cells are the rows of §2: one test, run under the profiles of its row. A test that proves
two patterns has a row in each — only `a_zoom_out_to_200_km_and_back_never_shows_a_hole`,
in P2 and in P3 — so 218 cells are 217 distinct tests. Counted by script from the tables
above; the last column counts each row once per profile it runs under.

| Level | Existing | New | Later | Cells | Runs (cells × profiles), existing / new |
|---|---|---|---|---|---|
| L1 | 34 | 20 | 0 | 54 | 35 / 25 |
| L2 | 14 | 16 | 0 | 30 | 16 / 36 |
| L3 | 24 | 38 | 0 | 62 | 26 / 85 |
| L4 | 2 | 17 | 0 | 19 | 2 / 28 |
| L5 | 0 | 23 | 1 | 24 | 0 / 24 |
| O | 0 | 29 | 0 | 29 | 0 / 29 |
| **All** | **74** | **143** | **1** | **218** | **79 / 227** |

By pattern, existing / new: P1 13 / 20; P2 12 / 14; P3 13 / 11; P4 8 / 14; P5 0 / 19;
P6 2 / 17 (+1 later); cross-cutting 26 / 19; observability 0 / 29. P5 has no existing test
at all: the budget has never been tested against a change of viewport.
