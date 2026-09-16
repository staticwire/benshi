//! The contract accepts a watcher that satisfies it.
//!
//! The suite has to run against something before the first platform adapter
//! exists, or it is written blind. This is that something: deliberately the
//! simplest watcher that satisfies every clause.

// The trait's methods are async, so an implementation cannot drop the keyword
// even when its body has nothing to await. A fake answers from memory, which is
// the whole point of a fake.
#![allow(clippy::unused_async_trait_impl)]

mod contract;
mod fakes;

use benshi_core::clock::{Clock, TestClock};
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};

use fakes::{DEADLINE, TICK, a_full_player, a_streaming_player, a_title_only_player, reading};

/// A watcher that satisfies every clause of the contract.
struct CorrectWatcher {
    listing: Vec<SourceInfo>,
    clock: TestClock,
    round: u32,
}

impl CorrectWatcher {
    fn new() -> Self {
        Self {
            listing: vec![a_full_player(), a_streaming_player(), a_title_only_player()],
            clock: TestClock::new(),
            round: 0,
        }
    }
}

impl PlayerWatcher for CorrectWatcher {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(self.listing.clone())
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.round += 1;
        self.clock.advance(TICK);
        let now = self.clock.now();

        Ok(PollOutcome {
            snapshots: self
                .listing
                .iter()
                .map(|source| reading(source, self.round, now))
                .collect(),
            failures: Vec::new(),
        })
    }
}

#[tokio::test]
async fn a_correct_watcher_satisfies_the_contract() {
    contract::verify(contract::Subject {
        watcher: CorrectWatcher::new(),
        deadline: DEADLINE,
    })
    .await;
}
