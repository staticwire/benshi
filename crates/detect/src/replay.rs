//! A watcher whose platform is a file.
//!
//! Replaying a recording through the interface a platform implements is what
//! makes a trace worth keeping: everything above the adapter - policy, the bus,
//! the session state, whatever a later milestone puts on top - runs against a
//! real session without one being open, and does it in milliseconds.
//!
//! **The readings are handed back exactly as recorded, one per round.** A trace
//! cannot say which readings shared a round: it holds only the readings policy
//! admitted, so a round's readings need not all be in the file to begin with.
//! Nor do the timestamps group them, because the MPRIS adapter stamps each
//! reading when its own source answered rather than when the round began.
//! Grouping them again would be a guess, and a replay that guesses is worth
//! nothing to the tests built on it.
//!
//! **The listing comes from the header and is never derived from the readings.**
//! What a source is able to report and which application it belongs to are
//! facts a reading does not carry, which is why a trace records them.

use std::vec::IntoIter;

use benshi_core::trace::Trace;
use benshi_core::{PlayerId, PlayerSnapshot, SourceInfo};

use crate::{PlayerWatcher, PollOutcome, WatchError};

/// Why a recording cannot be replayed.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// A reading names a source the header does not declare.
    ///
    /// The header is written once, before the first reading, so a player opened
    /// during a recording is never in it. Such a file is a true record of what
    /// was published and parses like any other; what nothing in it says is how
    /// to describe that source, and a round that carried its reading without
    /// describing it would break the contract every watcher is held to.
    #[error("line {line} is a reading from {player}, which the trace does not declare")]
    UndeclaredSource {
        /// Which line of the file, counting the header as line 1.
        line: usize,
        /// The source whose reading cannot be described.
        player: PlayerId,
    },
}

/// A recording, handed back through the interface a platform implements.
#[derive(Debug)]
pub struct ReplayWatcher {
    /// What the recording declared, unchanged for the whole replay.
    declared: Vec<SourceInfo>,
    /// The readings still to be handed back, in the order they were taken.
    remaining: IntoIter<PlayerSnapshot>,
}

impl ReplayWatcher {
    /// A watcher over `trace`.
    ///
    /// # Errors
    ///
    /// [`ReplayError::UndeclaredSource`] naming the line, when a reading comes
    /// from a source the header does not declare. Refused here rather than at
    /// the round that reaches it, because a replay that fails halfway has
    /// already published readings a caller has acted on.
    pub fn new(trace: Trace) -> Result<Self, ReplayError> {
        let Trace { header, snapshots } = trace;

        for (index, reading) in snapshots.iter().enumerate() {
            if !header
                .sources
                .iter()
                .any(|source| source.player == reading.player)
            {
                return Err(ReplayError::UndeclaredSource {
                    // The header is line 1, so the first reading is line 2.
                    line: index + 2,
                    player: reading.player.clone(),
                });
            }
        }

        Ok(Self {
            declared: header.sources,
            remaining: snapshots.into_iter(),
        })
    }

    /// Move `reading`'s source to the state it reads, and describe them all.
    ///
    /// A round describes what it read, from the same observation the reading
    /// came from. The state is the one part of a description a reading also
    /// carries, so it is the one part that moves - and it moves in the listing
    /// itself rather than in a copy, so that [`PlayerWatcher::sources`] cannot
    /// answer with a state the replay has already left behind.
    ///
    /// # Panics
    ///
    /// If the reading's source is not declared, which [`ReplayWatcher::new`]
    /// has already refused. Reaching it means that check no longer holds, and
    /// carrying on would publish a round describing a player it never listed.
    fn describing(&mut self, reading: &PlayerSnapshot) -> Vec<SourceInfo> {
        let described = self
            .declared
            .iter_mut()
            .find(|source| source.player == reading.player)
            .expect("new() refuses a reading from a source the trace does not declare");
        described.state = reading.state;

        self.declared.clone()
    }
}

// A recording answers from memory, and the trait asks for a future either way.
#[allow(clippy::unused_async_trait_impl)]
impl PlayerWatcher for ReplayWatcher {
    /// What the recording declared, each source in the state it last read.
    ///
    /// Membership, capabilities and applications are the recording's and do not
    /// move: they are what the platform said when it began, and a replay has
    /// nothing newer to say about them. The state does move, because a reading
    /// carries one and this question is asked about now rather than about then.
    /// Before the first round, and for a source that never produced a reading,
    /// the answer is the one the recording started with.
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(self.declared.clone())
    }

    /// The next reading, or an empty round once there are none left.
    ///
    /// A recording that has run out is not a failure and not the end of a
    /// platform: it is a session where nothing is playing any more, which is
    /// exactly what a round with no readings means.
    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        let Some(reading) = self.remaining.next() else {
            return Ok(PollOutcome {
                sources: self.declared.clone(),
                snapshots: Vec::new(),
                failures: Vec::new(),
            });
        };

        Ok(PollOutcome {
            sources: self.describing(&reading),
            snapshots: vec![reading],
            failures: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ReplayError, ReplayWatcher};
    use crate::PlayerWatcher;
    use benshi_core::clock::{Clock, TestClock, Timestamp};
    use benshi_core::path::RawPath;
    use benshi_core::trace::{Trace, TraceHeader};
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, SourceInfo,
    };
    use std::time::Duration;

    /// The source every recording here declares.
    const PLAYER: &str = "mpv.instance1701";

    fn a_source(identity: &str) -> SourceInfo {
        SourceInfo {
            player: PlayerId(identity.to_owned()),
            app: AppName("mpv".to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    /// A moment `after` seconds past a clock's epoch.
    fn a_moment(after: u64) -> Timestamp {
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(after));
        clock.now()
    }

    fn a_reading(identity: &str, index: u64, state: PlayState) -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId(identity.to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/Show - 03.mkv".to_vec())),
            state,
            position: Known::Value(Duration::from_secs(index)),
            duration: Known::Value(Duration::from_secs(1420)),
            observed_at: a_moment(index),
        }
    }

    fn a_recording(snapshots: Vec<PlayerSnapshot>) -> Trace {
        Trace {
            header: TraceHeader::new("2026-09-18T00:00:00Z".to_owned(), vec![a_source(PLAYER)]),
            snapshots,
        }
    }

    #[tokio::test]
    async fn the_readings_come_back_in_the_order_they_were_taken() {
        let recorded = vec![
            a_reading(PLAYER, 0, PlayState::Playing),
            a_reading(PLAYER, 1, PlayState::Paused),
            a_reading(PLAYER, 2, PlayState::Playing),
        ];
        let mut watcher = ReplayWatcher::new(a_recording(recorded.clone())).expect("it replays");

        let mut replayed = Vec::new();
        for _round in 0..recorded.len() {
            replayed.extend(watcher.poll().await.expect("a round").snapshots);
        }

        assert_eq!(replayed, recorded);
    }

    #[tokio::test]
    async fn a_round_describes_its_reading_as_the_reading_reads() {
        // The contract's clause, held at the source rather than only through
        // the suite: a listing copied from the header unchanged would describe
        // a paused player as playing, and a consumer told about two moments has
        // no way of noticing.
        let mut watcher =
            ReplayWatcher::new(a_recording(vec![a_reading(PLAYER, 0, PlayState::Paused)]))
                .expect("it replays");

        let round = watcher.poll().await.expect("a round");

        assert_eq!(round.snapshots[0].state, PlayState::Paused);
        assert_eq!(round.sources[0].state, PlayState::Paused);
    }

    #[tokio::test]
    async fn a_recording_that_has_run_out_reports_a_session_with_nothing_playing() {
        // Not a failure and not the end of the platform. A watcher that errored
        // here would have the supervisor restart the task, and a restart would
        // replay the whole recording again.
        let mut watcher =
            ReplayWatcher::new(a_recording(vec![a_reading(PLAYER, 0, PlayState::Playing)]))
                .expect("it replays");

        watcher.poll().await.expect("the one reading");
        let after = watcher
            .poll()
            .await
            .expect("a round after the readings run out");

        assert!(after.snapshots.is_empty(), "{:?}", after.snapshots);
        assert!(after.failures.is_empty(), "{:?}", after.failures);
        assert_eq!(after.sources, vec![a_source(PLAYER)]);
    }

    #[tokio::test]
    async fn the_listing_follows_the_last_reading_replayed() {
        // Asked about now, not about when the recording began. A listing left
        // at the header's state would answer `Playing` for a player whose last
        // replayed reading was `Paused`, which no adapter would do and which
        // the contract does not check.
        let mut watcher = ReplayWatcher::new(a_recording(vec![
            a_reading(PLAYER, 0, PlayState::Playing),
            a_reading(PLAYER, 1, PlayState::Paused),
        ]))
        .expect("it replays");

        assert_eq!(
            watcher.sources().await.expect("a listing")[0].state,
            PlayState::Playing,
            "before a round, the answer is the one the recording started with"
        );
        watcher.poll().await.expect("the first reading");
        watcher.poll().await.expect("the paused reading");

        assert_eq!(
            watcher.sources().await.expect("a listing")[0].state,
            PlayState::Paused
        );
    }

    #[tokio::test]
    async fn the_listing_is_the_one_the_recording_declared() {
        // Never derived from the readings. A source that produced none is still
        // listed, which is what a recording of a player that sat idle looks
        // like, and what a denied one looks like too.
        let idle = "MpcQt";
        let trace = Trace {
            header: TraceHeader::new(
                "2026-09-18T00:00:00Z".to_owned(),
                vec![a_source(PLAYER), a_source(idle)],
            ),
            snapshots: vec![a_reading(PLAYER, 0, PlayState::Playing)],
        };
        let mut watcher = ReplayWatcher::new(trace).expect("it replays");

        let listed = watcher.sources().await.expect("a listing");

        assert_eq!(listed, vec![a_source(PLAYER), a_source(idle)]);
    }

    #[test]
    fn a_reading_from_a_source_the_recording_never_declared_is_refused() {
        // The refusal the trace format deliberately does not make: such a file
        // is a true record and parses, and this is the one consumer that cannot
        // use it. Refused whole rather than at the round that reaches it, so
        // that nothing has been published by the time it is known.
        let opened_later = "vlc";
        let failure = ReplayWatcher::new(a_recording(vec![
            a_reading(PLAYER, 0, PlayState::Playing),
            a_reading(opened_later, 1, PlayState::Playing),
        ]))
        .expect_err("a source nothing declared cannot be described");

        assert!(
            matches!(&failure, ReplayError::UndeclaredSource { line: 3, player } if player.0 == opened_later),
            "got {failure:?}"
        );
        let said = failure.to_string();
        assert!(said.contains("line 3"), "got {said}");
        // This message names its subject, unlike a `WatchError`: whoever gets
        // it holds the trace and not the pair the reading came in. Named
        // plainly, though - the identity goes through `Display`, and nothing
        // else here would notice if that went back to the debug form.
        assert!(said.contains(opened_later), "got {said}");
        assert!(
            !said.contains("PlayerId"),
            "a Rust type name reached a message: {said}"
        );
    }
}
