//! A recorded trace replays to the readings that were recorded.
//!
//! The test that makes every later milestone's fixtures worth having. Nothing
//! else in the suite compares a real recording against what the daemon does
//! with it: the fixture was taken from a live session bus, through the
//! production adapter, and every part below this test except the platform is
//! the article that runs in the binary.
//!
//! What it proves is narrow and load-bearing. A reading that a recording
//! preserved but a replay altered - a rounded position, a dropped nanosecond, a
//! reordering - would make every fixture in later milestones prove a timeline
//! that never happened, and nothing downstream of the file could tell.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use benshi_core::policy::PolicyTable;
use benshi_core::trace::Trace;
use benshi_core::{Known, PlayState, PlayerId, PlayerSnapshot};
use benshi_daemon::bus::{BusEvent, EventBus};
use benshi_daemon::detection::{Detection, Seen};
use benshi_detect::replay::ReplayWatcher;
use tokio::time::timeout;

/// The recording under test, taken on 2026-09-18 from a live session bus.
///
/// One mpv playing a ten-minute file, paused for twenty seconds and seeked
/// twenty back. Two other players were open and are declared in its header
/// without a single reading between them, which is the shape a listing is for.
///
/// Kept in `benshi-core`, which defines the `Trace` this file is a sample of,
/// so that a test in either crate can read it. `include_str!` resolves a path
/// and not a dependency - it would reach either way - so the direction is the
/// one the crates already have: the daemon reaches into `core`, never the
/// reverse.
const FIXTURE: &str = include_str!("../../core/tests/fixtures/mpv-one-episode.jsonl");

/// The detection loop's period, which the run pays once per reading.
///
/// Short because a replay answers from memory: a longer one would buy nothing
/// and would be real time the run spends waiting.
const PERIOD: Duration = Duration::from_millis(1);

/// How long the test waits before calling the replay lost.
///
/// Only ever spent on a failure. A replay that stops early has to fail the run
/// rather than hang it.
const PATIENCE: Duration = Duration::from_secs(10);

/// Run the daemon's detection loop over `trace` and collect what reaches the
/// bus, stopping once as many readings have arrived as the trace holds.
///
/// The policy table is the default one a user gets, not an allow-everything
/// fake: whether a replayed reading is admitted is part of what this proves.
async fn replayed(trace: Trace) -> Vec<PlayerSnapshot> {
    let expected = trace.snapshots.len();
    let watcher = ReplayWatcher::new(trace).expect("the fixture is replayable");

    let bus = Arc::new(EventBus::new());
    let mut events = bus.subscribe();
    let mut detection = Detection::new(
        watcher,
        Arc::clone(&bus),
        Arc::new(RwLock::new(PolicyTable::allowing_video_players())),
        Seen::new(),
        PERIOD,
    );

    let running = tokio::spawn(async move { detection.run().await });

    let collecting = timeout(PATIENCE, async {
        let mut seen = Vec::new();
        while seen.len() < expected {
            match events.recv().await.expect("the bus outlives the replay") {
                BusEvent::Snapshot(reading) => seen.push(reading),
                // Named one by one rather than caught by a wildcard, so that an
                // event added later stops this test instead of being passed
                // over in the same silence a dropped reading would be.
                BusEvent::SourcesChanged { .. }
                | BusEvent::SourceFailed { .. }
                | BusEvent::State(_) => {}
            }
        }

        seen
    })
    .await;

    running.abort();
    collecting.unwrap_or_else(|_| panic!("the replay published fewer than {expected} readings"))
}

#[tokio::test]
async fn a_recorded_trace_replays_to_the_readings_it_holds() {
    let recorded = Trace::from_jsonl(FIXTURE).expect("the fixture parses");

    let observed = replayed(recorded.clone()).await;

    assert_eq!(observed, recorded.snapshots);
}

#[tokio::test]
async fn the_fixture_is_worth_replaying() {
    // The test above compares the replay against whatever the fixture holds, so
    // a fixture reduced to nothing would pass it while proving nothing. These
    // are the properties it was recorded for, checked where a replacement
    // recording has to satisfy them too.
    let recorded = Trace::from_jsonl(FIXTURE).expect("the fixture parses");
    // Kept per source, because a position moving backwards is a seek only when
    // both readings came from one player. Over a recording of two active
    // players, one flat sequence would move backwards at every interleave and
    // the check below could not fail.
    let mut positions: BTreeMap<&PlayerId, Vec<Duration>> = BTreeMap::new();
    for reading in &recorded.snapshots {
        if let Known::Value(position) = reading.position {
            positions.entry(&reading.player).or_default().push(position);
        }
    }

    assert!(
        recorded.snapshots.len() > 30,
        "a recording of {} readings is too short to show anything",
        recorded.snapshots.len()
    );
    assert!(
        recorded
            .snapshots
            .iter()
            .any(|reading| reading.state == PlayState::Paused),
        "no reading is paused, so the pause was not recorded"
    );
    assert!(
        positions
            .values()
            .any(|reported| reported.windows(2).any(|pair| pair[1] < pair[0])),
        "no source's position moves backwards, so the seek was not recorded"
    );
    assert!(
        positions
            .values()
            .flatten()
            .any(|position| position.subsec_nanos() != 0),
        "every position is a whole second, so the file has been rounded"
    );
    // The listing is the half a reading cannot carry, and the fixture was taken
    // with two other players open precisely so that it holds a declared source
    // with nothing to say.
    let reading_sources: Vec<&PlayerId> = recorded
        .snapshots
        .iter()
        .map(|reading| &reading.player)
        .collect();
    assert!(
        recorded
            .header
            .sources
            .iter()
            .any(|source| !reading_sources.contains(&&source.player)),
        "every declared source produced readings, so nothing here shows a listing outliving one"
    );
}
