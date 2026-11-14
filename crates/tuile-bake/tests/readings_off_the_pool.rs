// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A ground's reading runs off the rayon pool.**
//!
//! One ground at a time is out for reading ([`tuile_bake::settle::READING`]).
//! Spawned on the global rayon pool, that one reading was all the work the
//! pool's idle threads were handed, and they spun for more: a fifth of a 4K
//! bake's CPU went to rayon's idle loop, on a bake using one core of eight.
//! The readings now go down a channel to a thread of their own, blocked
//! there while there is none; what a reading runs on the pool is its own
//! work. Settling and reading again both read off the pool, and read what
//! they read before.

#[path = "bake_memory_support/mod.rs"]
mod flight;
#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use rayon::prelude::*;
use tuile_bake::settle::{read_each, settle_each};
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle};
use tuile_film::ground::Ground;

const FRAMES: u32 = 6;
const VIEWPORT: (f64, f64) = (640.0, 360.0);

fn session() -> Session {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(flight::MadeUp { steps: 16 })));
    config.session.traversal.maximum_screen_space_error = 4.0;
    config.session.finish = Finish::Identity;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    session
}

/// What a frame reads, and which thread of the pool read it, if one did.
/// It uses the pool itself, as a reading may.
fn reading(i: u32, ground: &Ground) -> (Option<u64>, Option<usize>) {
    let on = rayon::current_thread_index();
    let summed: u64 = (0..64u64).into_par_iter().sum();
    assert_eq!(summed, 2016, "a reading's own work on the pool");
    (flight::under(i, ground), on)
}

#[test]
fn readings_run_off_the_rayon_pool() {
    let numbers: Vec<u32> = (0..FRAMES).collect();
    let views = |i: u32, _: &Session| vec![pose_view(&flight::pose(i), VIEWPORT)];
    let mut drawer = GroundDrawer::new();
    let frames = settle_each(
        &mut session(),
        &mut drawer,
        flight::DRAWN,
        &numbers,
        views,
        |i, ground: &Arc<Ground>| reading(i, ground),
        |_| true,
    )
    .expect("settled");
    for s in &frames {
        assert_eq!(
            s.value.1, None,
            "frame {} settled: read on thread {:?} of the rayon pool",
            s.number, s.value.1
        );
    }

    let kept: Vec<(u32, &tuile_bake::Selected)> = frames
        .iter()
        .filter_map(|s| Some((s.number, s.selected.as_ref()?)))
        .collect();
    let again = read_each(&mut drawer, flight::DRAWN, &kept, |i, ground| {
        reading(i, ground)
    });
    assert_eq!(again.len(), frames.len());
    for (s, ((height, on), _, _)) in frames.iter().zip(again) {
        assert_eq!(
            on, None,
            "frame {} read again on thread {on:?} of the rayon pool",
            s.number
        );
        assert_eq!(
            height, s.value.0,
            "frame {} read again as settled",
            s.number
        );
    }
}
