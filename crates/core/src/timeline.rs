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

use crate::{Known, PlayerSnapshot};

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

/// The state a sequence of readings is folded through.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Watched time accumulated over every reading so far.
    watched: Duration,
}

impl Timeline {
    /// A timeline that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            watched: Duration::ZERO,
        }
    }

    /// Fold one reading in, and answer with what it leaves behind.
    ///
    /// The position and the length are the source's own answers, carried
    /// through as they arrived. The watched time and the seek are the
    /// timeline's, and no reading carries either: a seek is a break between two
    /// consecutive readings, and watched time is what a sequence of them adds
    /// up to.
    pub fn advance(&mut self, reading: &PlayerSnapshot) -> Progress {
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
    use crate::clock::Timestamp;
    use crate::path::RawPath;
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::time::Duration;

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
}
