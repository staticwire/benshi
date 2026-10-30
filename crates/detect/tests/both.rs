//! Two watchers read as one, against the shared contract.
//!
//! Over two replays, whose sources have identities of their own, as the
//! watchers given to `Both` must.

// `Both` is built for Linux with the adapters, so on any other target this
// binary has nothing to compile rather than nothing to run.
#![cfg(target_os = "linux")]

mod contract;
mod fakes;

use benshi_core::SourceInfo;
use benshi_core::clock::{Clock, TestClock};
use benshi_core::trace::{Trace, TraceHeader};
use benshi_detect::both::Both;
use benshi_detect::replay::ReplayWatcher;

use fakes::{DEADLINE, TICK, a_full_player, a_streaming_player, a_title_only_player, reading};

/// When the recordings here claim to have been taken.
const RECORDED_AT: &str = "2026-09-18T00:00:00Z";

/// A replay of `rounds` rounds over `sources`, round by round.
fn a_replay_of(sources: Vec<SourceInfo>, rounds: u32) -> ReplayWatcher {
    let clock = TestClock::new();
    let mut snapshots = Vec::new();

    for round in 1..=rounds {
        clock.advance(TICK);
        let now = clock.now();
        snapshots.extend(sources.iter().map(|source| reading(source, round, now)));
    }

    ReplayWatcher::new(Trace {
        header: TraceHeader::new(RECORDED_AT.to_owned(), sources),
        snapshots,
    })
    .expect("a recording of declared sources replays")
}

#[tokio::test]
async fn two_watchers_read_as_one_satisfy_the_contract() {
    let watcher = Both::new(
        a_replay_of(vec![a_full_player(), a_title_only_player()], 4),
        a_replay_of(vec![a_streaming_player()], 4),
    );

    contract::verify(contract::Subject {
        watcher,
        deadline: DEADLINE,
    })
    .await;
}
