//! The contract rejects a watcher that breaks it.
//!
//! Every clause gets a watcher that breaks it and nothing else, and a test that
//! asserts the suite notices. A check no implementation can fail is a
//! description rather than a check, and the only way to tell the difference is
//! to break it on purpose.
//!
//! The `expected` string ties each test to the clause it proves, so renaming a
//! clause without updating its proof stops the build.

// The trait's methods are async, so an implementation cannot drop the keyword
// even when its body has nothing to await. A fake answers from memory, which is
// the whole point of a fake.
#![allow(clippy::unused_async_trait_impl)]

mod contract;
mod fakes;

use std::time::Duration;

use benshi_core::clock::{Clock, TestClock, Timestamp};
use benshi_core::path::RawPath;
use benshi_core::{AppName, Known, MediaRef, PlayerSnapshot};
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};

use fakes::{DEADLINE, TICK, a_full_player, a_streaming_player, a_title_only_player, reading};

/// The outcome of a poll that found nothing open.
fn no_readings() -> PollOutcome {
    PollOutcome {
        snapshots: Vec::new(),
        failures: Vec::new(),
    }
}

/// The outcome of a poll that produced one reading and no failure.
fn one_reading(snapshot: PlayerSnapshot) -> PollOutcome {
    PollOutcome {
        snapshots: vec![snapshot],
        failures: Vec::new(),
    }
}

/// Answers a different listing every time it is asked.
#[derive(Default)]
struct UnstableListing {
    calls: u32,
}

impl PlayerWatcher for UnstableListing {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        self.calls += 1;
        Ok(if self.calls % 2 == 1 {
            vec![a_full_player()]
        } else {
            vec![a_full_player(), a_title_only_player()]
        })
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        Ok(no_readings())
    }
}

/// Lists one source twice under the same identity.
struct DuplicateIdentity;

impl PlayerWatcher for DuplicateIdentity {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player(), a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        Ok(no_readings())
    }
}

/// Recomputes capabilities from whatever the last round happened to contain.
#[derive(Default)]
struct InferredCapabilities {
    calls: u32,
}

impl PlayerWatcher for InferredCapabilities {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        self.calls += 1;
        let mut source = a_full_player();
        if self.calls > 1 {
            source.capabilities.position = false;
        }
        Ok(vec![source])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        Ok(no_readings())
    }
}

/// Supplies an identity but no application name.
struct NamelessApplication;

impl PlayerWatcher for NamelessApplication {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        let mut source = a_full_player();
        source.app = AppName(String::new());
        Ok(vec![source])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        Ok(no_readings())
    }
}

/// Keeps the identity and changes the application under it.
#[derive(Default)]
struct WanderingApplication {
    calls: u32,
}

impl PlayerWatcher for WanderingApplication {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        self.calls += 1;
        let mut source = a_full_player();
        if self.calls > 1 {
            source.app = AppName("something-else".to_owned());
        }
        Ok(vec![source])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        Ok(no_readings())
    }
}

/// Emits a reading for a source it never listed.
#[derive(Default)]
struct UnlistedSnapshot {
    clock: TestClock,
}

impl PlayerWatcher for UnlistedSnapshot {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.clock.advance(TICK);
        let snapshot = reading(&a_title_only_player(), 1, self.clock.now());
        Ok(one_reading(snapshot))
    }
}

/// Declares it can name what it opened and reports a window title.
#[derive(Default)]
struct TitleForADeclaredLocation {
    clock: TestClock,
}

impl PlayerWatcher for TitleForADeclaredLocation {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.clock.advance(TICK);
        let mut snapshot = reading(&a_full_player(), 1, self.clock.now());
        snapshot.media = MediaRef::Title("mpv".to_owned());
        Ok(one_reading(snapshot))
    }
}

/// Reports a path of no bytes, which names no file.
#[derive(Default)]
struct EmptyPathForADeclaredLocation {
    clock: TestClock,
}

impl PlayerWatcher for EmptyPathForADeclaredLocation {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.clock.advance(TICK);
        let mut snapshot = reading(&a_full_player(), 1, self.clock.now());
        snapshot.media = MediaRef::LocalFile(RawPath::from_bytes(Vec::new()));
        Ok(one_reading(snapshot))
    }
}

/// Reports an address of no characters, which names nothing.
#[derive(Default)]
struct EmptyAddressForADeclaredLocation {
    clock: TestClock,
}

impl PlayerWatcher for EmptyAddressForADeclaredLocation {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_streaming_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.clock.advance(TICK);
        let mut snapshot = reading(&a_streaming_player(), 1, self.clock.now());
        snapshot.media = MediaRef::Remote(String::new());
        Ok(one_reading(snapshot))
    }
}

/// Cannot report a position and reports zero instead of saying so.
#[derive(Default)]
struct ZeroForAnAbsentPosition {
    clock: TestClock,
}

impl PlayerWatcher for ZeroForAnAbsentPosition {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_title_only_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.clock.advance(TICK);
        let mut snapshot = reading(&a_title_only_player(), 1, self.clock.now());
        snapshot.position = Known::Value(Duration::ZERO);
        Ok(one_reading(snapshot))
    }
}

/// Passes on a length the player reported that is shorter than the position.
#[derive(Default)]
struct DurationBelowPosition {
    clock: TestClock,
}

impl PlayerWatcher for DurationBelowPosition {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.clock.advance(TICK);
        let mut snapshot = reading(&a_full_player(), 1, self.clock.now());
        snapshot.position = Known::Value(Duration::from_mins(15));
        snapshot.duration = Known::Value(Duration::from_mins(1));
        Ok(one_reading(snapshot))
    }
}

/// Stamps the second reading earlier than the first.
#[derive(Default)]
struct BackwardsClock {
    clock: TestClock,
    round: u32,
}

impl PlayerWatcher for BackwardsClock {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.round += 1;
        self.clock.advance(TICK);
        let at = if self.round == 1 {
            self.clock.now()
        } else {
            Timestamp::epoch()
        };
        Ok(one_reading(reading(&a_full_player(), 1, at)))
    }
}

/// Takes far longer than the deadline it was given.
struct HangingPoll;

impl PlayerWatcher for HangingPoll {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![a_full_player()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(no_readings())
    }
}

#[tokio::test]
#[should_panic(expected = "a listing is repeatable")]
async fn a_listing_that_changes_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: UnstableListing::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "identities are unique within a listing")]
async fn a_repeated_identity_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: DuplicateIdentity,
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "capabilities are stable per source")]
async fn capabilities_inferred_from_a_reading_fail_the_contract() {
    contract::verify(contract::Subject {
        watcher: InferredCapabilities::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "every source names its application")]
async fn a_source_without_an_application_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: NamelessApplication,
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "an identity keeps its application")]
async fn an_application_that_changes_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: WanderingApplication::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "snapshots come only from listed sources")]
async fn a_reading_from_nowhere_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: UnlistedSnapshot::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "a declared location arrives as a location")]
async fn a_title_where_a_location_was_promised_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: TitleForADeclaredLocation::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "a reported path is not empty")]
async fn an_empty_path_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: EmptyPathForADeclaredLocation::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "a reported address is not empty")]
async fn an_empty_address_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: EmptyAddressForADeclaredLocation::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "an absent position is absent, not zero")]
async fn zero_standing_in_for_an_absent_position_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: ZeroForAnAbsentPosition::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "a duration is never shorter than its position")]
async fn a_length_below_the_position_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: DurationBelowPosition::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "timestamps never go backwards")]
async fn a_reading_time_that_moves_backwards_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: BackwardsClock::default(),
        deadline: DEADLINE,
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "a poll respects its deadline")]
async fn a_poll_that_overruns_fails_the_contract() {
    contract::verify(contract::Subject {
        watcher: HangingPoll,
        deadline: Duration::from_millis(50),
    })
    .await;
}
