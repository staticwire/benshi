//! The replay watcher against the shared contract.
//!
//! A replay that cannot satisfy the contract is not replaying faithfully: the
//! whole point of handing readings back through the platform interface is that
//! everything above it cannot tell the difference, and the contract is what
//! "cannot tell the difference" means.
//!
//! The recordings here are built from the same fixtures the correct fake is
//! built from, so a reading and the declaration it follows agree by
//! construction. That is a property of the recording rather than of the
//! watcher: a replay hands back what it was given, so a trace that contradicts
//! its own listing replays the contradiction. Producing one takes a text editor
//! - the adapter that writes a recording passes this suite live.

mod contract;
mod fakes;

use benshi_core::SourceInfo;
use benshi_core::clock::{Clock, TestClock};
use benshi_core::trace::{Trace, TraceHeader};
use benshi_detect::replay::ReplayWatcher;

use fakes::{DEADLINE, TICK, a_full_player, a_streaming_player, a_title_only_player, reading};

/// When the recordings here claim to have been taken.
///
/// Never read: the timeline a replay follows is in the readings.
const RECORDED_AT: &str = "2026-09-18T00:00:00Z";

/// A recording of `rounds` rounds over `sources`, round by round.
///
/// Round by round because that is the order a recorder writes: a round produces
/// a reading per source, and the next round follows the whole of the last.
fn a_recording_of(sources: Vec<SourceInfo>, rounds: u32) -> Trace {
    let clock = TestClock::new();
    let mut snapshots = Vec::new();

    for round in 1..=rounds {
        clock.advance(TICK);
        let now = clock.now();
        snapshots.extend(sources.iter().map(|source| reading(source, round, now)));
    }

    Trace {
        header: TraceHeader::new(RECORDED_AT.to_owned(), sources),
        snapshots,
    }
}

#[tokio::test]
async fn the_replay_watcher_satisfies_the_contract() {
    let recording = a_recording_of(
        vec![a_full_player(), a_streaming_player(), a_title_only_player()],
        4,
    );

    let watcher = ReplayWatcher::new(recording).expect("a recording of declared sources replays");

    contract::verify(contract::Subject {
        watcher,
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
async fn a_replay_of_one_source_satisfies_the_contract() {
    // One clause of the suite compares two consecutive rounds, and it bites
    // only when both carry the same source; everything else the suite compares
    // across calls is two listings, which a replay returns unchanged. A replay
    // hands back one reading per round, so over three sources those two rounds
    // are two different players and the comparison finds nothing to compare.
    // This run exists to make it find something: every round is the same
    // player, so "timestamps never go backwards" is a check here rather than a
    // no-op.
    let recording = a_recording_of(vec![a_full_player()], 4);

    let watcher = ReplayWatcher::new(recording).expect("a recording of declared sources replays");

    contract::verify(contract::Subject {
        watcher,
        deadline: DEADLINE,
    })
    .await;
}
