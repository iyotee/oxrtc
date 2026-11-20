//! The end-to-end harness itself: what two tests running at once must never
//! share.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};

/// Two tests of one binary run at once, and each writes its programs'
/// configurations and logs into the directory it was given. Named after the
/// process and the clock alone, two directories drawn in the same instant
/// were one: the Windows job of 52e3474 had two go2rtc cameras read one
/// `go2rtc.yaml` and write one log, and the test whose file was overwritten
/// waited 10 s on a port no go2rtc had been told
/// (`openivr_layout_with_its_deduplicator_makes_every_sound_with_no_ffmpeg`,
/// "Only one usage of each socket address" on the other test's port).
#[test]
fn directories_drawn_at_the_same_instant_are_never_one() {
    const THREADS: usize = 32;
    const EACH: usize = 50;
    let start = Arc::new(Barrier::new(THREADS));
    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                (0..EACH).map(|_| common::temp_dir("harness")).collect::<Vec<_>>()
            })
        })
        .collect();
    let drawn: Vec<_> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
    let distinct: BTreeSet<_> = drawn.iter().collect();
    let shared = drawn.len() - distinct.len();
    for dir in &distinct {
        let _ = std::fs::remove_dir_all(dir);
    }
    assert_eq!(shared, 0, "{shared} of {} directories were given twice", drawn.len());
    assert!(drawn.iter().all(|dir| !dir.exists()), "each was created, and removed here");
}
