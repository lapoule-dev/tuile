// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The grounds out for reading are bounded by what they weigh.**
//!
//! A count of grounds bounds grounds of one size. A 4K frame's ground
//! weighs two gigabytes and more, and eight of them out at once — readings
//! lagging behind the building, as a 4K bake's audits do — is a farm worker
//! killed for memory. Here the budget is set below one ground's weight
//! (`TUILE_GROUND_READING_MIB=1`), the readings lag, and never more than one
//! ground is out; what is read is what reading each frame in turn reads.

#[path = "bake_memory_support/mod.rs"]
mod flight;
#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tuile_bake::settle::{read_each, settle_each};
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle};
use tuile_film::ground::Ground;

const FRAMES: u32 = 24;
const VIEWPORT: (f64, f64) = (1280.0, 720.0);
/// How long a reading waits, at most, for a second ground to be out.
const LAG: Duration = Duration::from_millis(1500);

fn session() -> Session {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(flight::MadeUp { steps: 32 })));
    config.session.traversal.maximum_screen_space_error = 4.0;
    config.session.finish = Finish::Identity;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    session
}

/// Reads `ground` once a second ground is out, or after [`LAG`]: a settle
/// that lets a second one out meets its reading; one that keeps them to the
/// budget waits for each.
fn lagging(i: u32, ground: &Ground, out: &AtomicU64, bytes: &AtomicU64) -> Option<u64> {
    let m = tuile_core::metrics::metrics();
    let deadline = Instant::now() + LAG;
    loop {
        out.fetch_max(m.ground_reads_pending.get(), Ordering::SeqCst);
        bytes.fetch_max(m.ground_reads_pending_bytes.get(), Ordering::SeqCst);
        if m.ground_reads_pending.get() > 1 || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    flight::under(i, ground)
}

/// The only test of this binary: the budget is the process's.
#[test]
fn grounds_out_for_reading_weigh_no_more_than_the_budget() {
    // Below one ground's weight: one ground out at a time.
    std::env::set_var("TUILE_GROUND_READING_MIB", "1");
    rayon::ThreadPoolBuilder::new()
        .num_threads(16)
        .build_global()
        .expect("the first pool of this binary");
    let numbers: Vec<u32> = (0..FRAMES).collect();
    let views = |i: u32, _: &Session| vec![pose_view(&flight::pose(i), VIEWPORT)];

    // The reference: each frame settled, built and read in turn.
    let mut reference_session = session();
    let mut reference_drawer = GroundDrawer::new();
    let mut weight = 0;
    let reference: Vec<(u64, Option<u64>)> = numbers
        .iter()
        .map(|&i| {
            reference_session
                .frame(views(i, &reference_session))
                .expect("a frame");
            let selected = reference_session.selected_ground().expect("kept");
            let ground =
                reference_drawer.ground(&selected, flight::DRAWN.how, flight::DRAWN.stitch);
            weight = weight.max(reference_drawer.built() as u64);
            (
                reference_session.selection_digest().expect("live"),
                flight::under(i, &ground),
            )
        })
        .collect();
    assert!(
        weight > 1 << 20,
        "a ground weighs more than the budget: {weight} bytes"
    );

    let (out, bytes) = (AtomicU64::new(0), AtomicU64::new(0));
    let frames = settle_each(
        &mut session(),
        &mut GroundDrawer::new(),
        flight::DRAWN,
        &numbers,
        views,
        |i, ground: &Arc<Ground>| lagging(i, ground, &out, &bytes),
        |_| true,
    )
    .expect("settled");
    let read: Vec<(u64, Option<u64>)> = frames.iter().map(|s| (s.selection, s.value)).collect();
    assert_eq!(read, reference, "read as reading them in turn reads");
    let (out, bytes) = (out.load(Ordering::SeqCst), bytes.load(Ordering::SeqCst));
    eprintln!("settling: at most {out} grounds out, {bytes} bytes; a ground weighs up to {weight}");
    assert_eq!(
        out, 1,
        "{out} grounds out at once, over a budget below one ground's weight"
    );
    assert!(
        bytes <= weight,
        "{bytes} bytes out, more than one ground's {weight}"
    );

    // Read again from the selections kept: bounded alike.
    let kept: Vec<(u32, &tuile_bake::Selected)> = frames
        .iter()
        .filter_map(|s| Some((s.number, s.selected.as_ref()?)))
        .collect();
    let (out, bytes) = (AtomicU64::new(0), AtomicU64::new(0));
    let again = read_each(
        &mut GroundDrawer::new(),
        flight::DRAWN,
        &kept,
        |i, ground| lagging(i, ground, &out, &bytes),
    );
    let out = out.load(Ordering::SeqCst);
    eprintln!("reading again: at most {out} grounds out");
    assert_eq!(out, 1, "{out} grounds out at once reading again");
    for ((i, _), (height, _, _)) in kept.iter().zip(again) {
        assert_eq!(height, reference[*i as usize].1, "frame {i} read again");
    }
}
