//! What a sequence of readings says about playback.
//!
//! The timeline folds readings into [`Progress`]: how much of the media has
//! actually been watched, where playback currently is, and whether the step
//! that produced this answer was a seek.
//!
//! **Nothing here takes a [`Clock`](crate::clock::Clock), and that is the one
//! thing to know before reading further.** Every reading carries the moment it
//! was taken, stamped from the clock the adapter was given, so the timeline
//! advances on readings and never asks what time it is: twenty seconds of
//! pause are twenty seconds in those timestamps and no wall-clock time at all.
//! A clock here would be a second clock, free to disagree with the one the
//! readings came from - the answer would then depend on when `advance` was
//! called, and replaying a recording would no longer reproduce the playback it
//! recorded.

use std::time::Duration;

use crate::clock::Timestamp;
use crate::{Known, PlayState, PlayerSnapshot};

/// What one reading left the timeline in.
///
/// The position and the watched time are two facts and not one. A viewer who
/// seeks to the last minute has playback at the end and has watched nothing;
/// the decision of whether an episode was watched is about the second, and
/// collapsing the two is what lets a single seek mark an episode.
///
/// `watched` is a [`Duration`] rather than a fraction because a fraction needs
/// a duration to be a fraction of, and a duration is [`Known`]: a player is
/// free not to report one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Where playback is, as the source reported it.
    pub position: Known<Duration>,
    /// How much of the media has been watched, accumulated over the readings.
    pub watched: Duration,
    /// How long the media is, as far as it can be known.
    pub duration: Known<Duration>,
    /// Whether the step that produced this answer was a seek.
    pub seeked: bool,
}

/// The part of a reading the next one is measured against.
///
/// Only the two fields the fold compares, rather than the whole reading, so
/// that what the timeline remembers is visible at a glance.
#[derive(Debug, Clone, Copy)]
struct Previous {
    /// When it was taken, on the clock its source was read with.
    observed_at: Timestamp,
    /// What it reported, which is what the interval after it counts as.
    state: PlayState,
}

/// The state a sequence of readings is folded through.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Watched time accumulated over every reading so far.
    watched: Duration,
    /// The reading this one will be measured against, absent until the first.
    previous: Option<Previous>,
}

impl Timeline {
    /// A timeline that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            watched: Duration::ZERO,
            previous: None,
        }
    }

    /// Fold one reading in, and answer with what it leaves behind.
    ///
    /// The position and the length are the source's own answers, carried
    /// through as they arrived. The watched time and the seek are the
    /// timeline's, and no reading carries either: a seek is a break between two
    /// consecutive readings, and watched time is what a sequence of them adds
    /// up to.
    ///
    /// The watched time grows by the interval between this reading and the one
    /// before it when that earlier reading was playing, and by nothing at all
    /// otherwise. The earlier one and not this one, because the interval was
    /// spent in the state it began in, and a reading says what was true when it
    /// was taken rather than what became true during the interval after it.
    ///
    /// A pause therefore costs up to one interval at each end, in opposite
    /// directions. Both ends are in this crate's recording and were measured:
    /// the interval into the pause is counted although the position did not
    /// move, and the interval out of it is not counted although the position
    /// moved a full second. Neither end grows with the pause and they work
    /// against each other, so what a pause costs stays within one interval
    /// however long it lasts, and no shorter rule does better without asking a
    /// reading when, inside the interval before it, the viewer reached for the
    /// keyboard.
    pub fn advance(&mut self, reading: &PlayerSnapshot) -> Progress {
        if let Some(previous) = self.previous
            && previous.state == PlayState::Playing
        {
            self.watched += reading.observed_at.since(previous.observed_at);
        }
        self.previous = Some(Previous {
            observed_at: reading.observed_at,
            state: reading.state,
        });

        Progress {
            position: reading.position,
            watched: self.watched,
            duration: reading.duration,
            seeked: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Progress, Timeline};
    use crate::clock::{Clock, TestClock, Timestamp};
    use crate::path::RawPath;
    use crate::trace::Trace;
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::time::Duration;

    /// One mpv playing one episode: 60 readings a second apart, of which 40 are
    /// playing and 20 are a single pause, and one backward seek near the end.
    const RECORDING: &str = include_str!("../tests/fixtures/mpv-one-episode.jsonl");

    // What the recording holds, measured from it on 2026-09-18. One note for
    // all three constants below, so not a doc comment on the first of them.
    //
    // The readings are a second apart but not exactly: the shortest interval
    // is 998.109148 ms and the longest 1002.104074 ms, so a test that assumes
    // a round second asserts something the recording does not contain. Of the
    // 59 intervals, 39 begin while playback is running and span 39.001189719 s
    // together, and 20 begin paused and span 19.998711057 s; the two add up to
    // the 58.999900776 s the recording covers.
    //
    // Nanoseconds and not microseconds, because the first measurement of this
    // divided them away and the total below was wrong by 719 of them.
    const PLAYING_INTERVALS: usize = 39;
    const PAUSED_INTERVALS: usize = 20;
    const WATCHED_IN_FULL: Duration = Duration::from_nanos(39_001_189_719);
    const SHORTEST_INTERVAL: Duration = Duration::from_nanos(998_109_148);
    const LONGEST_INTERVAL: Duration = Duration::from_nanos(1_002_104_074);

    /// A run of readings a second apart, each in the state given for it.
    ///
    /// Built through a [`TestClock`] because a [`Timestamp`] comes only from a
    /// clock, which is the same reason the timeline needs none of its own. The
    /// position is left unreported: these readings exist to exercise the fold
    /// over states and moments, and a test about positions builds its own.
    fn readings_every_second(states: &[PlayState]) -> Vec<PlayerSnapshot> {
        let clock = TestClock::new();
        states
            .iter()
            .map(|state| {
                let observed_at = clock.now();
                clock.advance(Duration::from_secs(1));
                PlayerSnapshot {
                    state: *state,
                    observed_at,
                    ..reading(Known::NotReported, Known::NotReported)
                }
            })
            .collect()
    }

    /// One reading of one file, with everything the test is not about fixed.
    fn reading(position: Known<Duration>, duration: Known<Duration>) -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/show-03.mkv".to_vec())),
            state: PlayState::Playing,
            position,
            duration,
            observed_at: Timestamp::epoch(),
        }
    }

    #[test]
    fn the_first_reading_leaves_nothing_watched_however_far_in_it_starts() {
        // The reason `Progress` carries both fields. A viewer who opens a file
        // at 9:50 of 10:00 has watched none of it, so the two answers differ on
        // the very first reading. An implementation that reads the watched time
        // off the position agrees with every later reading of a file played
        // from the start, and is wrong about exactly the case that matters.
        let mut timeline = Timeline::new();

        let progress = timeline.advance(&reading(
            Known::Value(Duration::from_secs(590)),
            Known::Value(Duration::from_secs(600)),
        ));

        assert_eq!(progress.watched, Duration::ZERO);
        assert_eq!(progress.position, Known::Value(Duration::from_secs(590)));
    }

    #[test]
    fn a_length_that_was_not_reported_stays_unreported_rather_than_zero() {
        // Zero is a length no file has, so publishing it as one would make
        // every consumer special-case a sentinel. Which absence it was survives
        // too: a player that did not say and a source that cannot say are
        // different facts, and the fallback for a missing length has to answer
        // for each.
        let mut timeline = Timeline::new();

        let silent = timeline.advance(&reading(
            Known::Value(Duration::from_secs(12)),
            Known::NotReported,
        ));
        let incapable = timeline.advance(&reading(
            Known::Value(Duration::from_secs(12)),
            Known::Unsupported,
        ));

        assert_eq!(silent.duration, Known::NotReported);
        assert_eq!(incapable.duration, Known::Unsupported);
    }

    #[test]
    fn progress_is_a_value_a_caller_can_spell_out_and_compare() {
        // `Progress` is a value rather than a handle on the timeline: every
        // field is public and owned, so the whole answer compares equal to one
        // written out by hand. The seek is part of that answer, and one reading
        // on its own is never a seek - a seek is a break between two of them.
        let mut timeline = Timeline::new();
        let progress = timeline.advance(&reading(
            Known::Value(Duration::from_secs(12)),
            Known::Value(Duration::from_secs(600)),
        ));

        assert_eq!(
            progress,
            Progress {
                position: Known::Value(Duration::from_secs(12)),
                watched: Duration::ZERO,
                duration: Known::Value(Duration::from_secs(600)),
                seeked: false,
            }
        );
    }

    #[test]
    fn an_interval_that_began_paused_adds_no_watched_time() {
        // The recording holds a real pause, and this is the half of the fold
        // that is easy to leave out: asserting that time is counted is the
        // natural shape of a test, and asserting that it is not has to be
        // remembered. The count at the end is what stops the assertion from
        // passing because it was never reached.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");
        let mut timeline = Timeline::new();
        let mut watched = Duration::ZERO;
        let mut checked = 0;

        for (index, reading) in trace.snapshots.iter().enumerate() {
            let progress = timeline.advance(reading);
            if index > 0 && trace.snapshots[index - 1].state == PlayState::Paused {
                assert_eq!(
                    progress.watched, watched,
                    "the interval ending at reading {index} began paused and was counted"
                );
                checked += 1;
            }
            watched = progress.watched;
        }

        assert_eq!(checked, PAUSED_INTERVALS, "the recording holds the pause");
    }

    #[test]
    fn an_interval_that_began_playing_adds_the_time_it_spanned() {
        // The other half. Without it every assertion above is satisfied by a
        // timeline that counts nothing at all. The interval is compared against
        // the readings' own timestamps rather than a round second, because the
        // recording's intervals are a few milliseconds off one.
        //
        // Two assertions and not one, because the first measures the interval
        // with the same `since` the fold measures it with: make `since` answer
        // zero and the fold adds nothing, the expectation expects nothing, and
        // it stays green. The band holds the other half of this test's name.
        // It is the recording's own shortest and longest interval, so it fails
        // for a growth of zero, of a round second, or of a position.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");
        let mut timeline = Timeline::new();
        let mut watched = Duration::ZERO;
        let mut checked = 0;

        for (index, reading) in trace.snapshots.iter().enumerate() {
            let progress = timeline.advance(reading);
            if index > 0 {
                let earlier = &trace.snapshots[index - 1];
                if earlier.state == PlayState::Playing {
                    assert_eq!(
                        progress.watched,
                        watched + reading.observed_at.since(earlier.observed_at),
                        "the interval ending at reading {index} began playing"
                    );
                    let grew = progress.watched.saturating_sub(watched);
                    assert!(
                        (SHORTEST_INTERVAL..=LONGEST_INTERVAL).contains(&grew),
                        "reading {index} grew the watched time by {grew:?}"
                    );
                    checked += 1;
                }
            }
            watched = progress.watched;
        }

        assert_eq!(checked, PLAYING_INTERVALS, "the recording holds playback");
    }

    #[test]
    fn the_recording_adds_up_to_the_playing_time_it_holds() {
        // The two rules above, summed over the whole recording. Neither of
        // them would notice a `since` that answered zero: one asserts that
        // nothing is added, and the other measures the interval it expects
        // with the same `since` the fold measures it with. This total is a
        // literal taken off the file, and it would.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");
        let mut timeline = Timeline::new();

        // A plain loop, because folding with a side effect through an iterator
        // chain is a trap here: `.map(..).next_back()` reads as "the last
        // answer" and advances the timeline exactly once, over the last
        // reading. It was written that way first and the total came out zero.
        let mut watched = Duration::ZERO;
        for reading in &trace.snapshots {
            watched = timeline.advance(reading).watched;
        }

        assert_eq!(watched, WATCHED_IN_FULL);
    }

    #[test]
    fn a_recorded_pause_of_twenty_seconds_costs_no_wall_clock_time() {
        // The reason the timeline takes no clock, stated as a measurement. The
        // recording is checked for the pause first: without that, a timeline
        // folding nothing would satisfy the timing claim perfectly. Nineteen
        // seconds and not the twenty this test is named for, because the
        // twenty intervals that begin paused span 19.998711057 s together, and
        // a round twenty is one more number the recording does not hold.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");
        let paused: Duration = trace
            .snapshots
            .windows(2)
            .filter(|pair| pair[0].state == PlayState::Paused)
            .map(|pair| pair[1].observed_at.since(pair[0].observed_at))
            .sum();
        assert!(
            paused >= Duration::from_secs(19),
            "the recording holds only {paused:?} of pause"
        );

        let started = std::time::Instant::now();
        let mut timeline = Timeline::new();
        let mut watched = Duration::ZERO;
        for reading in &trace.snapshots {
            watched = timeline.advance(reading).watched;
        }
        let spent = started.elapsed();

        assert!(watched > Duration::ZERO, "the fold counted nothing");
        assert!(
            spent < Duration::from_millis(50),
            "the fold spent {spent:?}"
        );
    }

    #[test]
    fn an_interval_that_began_stopped_adds_no_watched_time() {
        // The recording holds no stopped reading, so a rule written as "count
        // it unless the source was paused" passes everything above while
        // counting a player with nothing open as watching.
        let readings = readings_every_second(&[
            PlayState::Stopped,
            PlayState::Stopped,
            PlayState::Playing,
            PlayState::Playing,
        ]);
        let mut timeline = Timeline::new();

        let watched: Vec<Duration> = readings
            .iter()
            .map(|reading| timeline.advance(reading).watched)
            .collect();

        assert_eq!(
            watched,
            [
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(1)
            ]
        );
    }
}
