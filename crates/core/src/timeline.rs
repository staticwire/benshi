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

use std::mem;
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
    ///
    /// The source's own answer, except where its own reading contradicts it. A
    /// length below the position it arrived with, and a length of zero, arrive
    /// here as [`Known::NotReported`]: both are impossible, and a consumer
    /// handed one has no way to tell it from a true one.
    pub duration: Known<Duration>,
    /// Whether the step that produced this answer was a seek.
    pub seeked: bool,
}

/// How far a position may sit from where the reading before it implies, before
/// the step between the two is called a seek.
///
/// Three bounds decide this number and every one of them was measured or read
/// out of a default that ships, rather than chosen for looking round.
///
/// **The adapter's own skew is the floor.** A reading is stamped when the
/// platform's answer arrives, and the position inside that answer was true when
/// the player read it, up to one call earlier. The MPRIS adapter gives a call
/// half a second before giving up on it, so two consecutive readings taken at
/// opposite ends of that range differ from the elapsed time by up to half a
/// second with nothing at all having happened.
///
/// **A player's own accuracy is well below that floor**, at least for the one
/// player there is a recording of: over the 56 intervals of it whose readings
/// agree about the state and hold no seek, mpv's position never sat more than
/// 1.890852 ms from the elapsed time. That is the best case of one player and
/// the threshold is not set from it.
///
/// **The jump an arrow key makes is the ceiling.** Read on 2026-09-18 out of
/// the defaults each player ships: a bare arrow moves mpv five seconds, and VLC
/// does not seek on a bare arrow at all, its smallest arrow-key jump being
/// three seconds on shift. Anything below three seconds catches both.
///
/// Smaller deliberate seeks exist and this number does not claim to catch them.
/// mpv binds shift and an arrow to an exact one second step, and catching that
/// would need a threshold under one second, less than twice the floor. A one
/// second correction is deliberately left to look like drift.
///
/// Two seconds sits four times above the floor and a third below the ceiling.
/// It also leaves room for a file played faster than normal, which no adapter
/// reports and which looks exactly like a position running ahead of the clock:
/// at a one second cadence, three times speed puts it exactly two seconds ahead
/// each interval. The comparison is strict so that exactly two seconds is not a
/// seek, and reading the playback rate is the real answer, not written yet.
pub const SEEK_THRESHOLD: Duration = Duration::from_secs(2);

/// The longest interval between two readings the timeline counts as observed.
///
/// A longer one is a gap. Nothing is known about what happened inside it, so
/// nothing inside it is judged or counted, and the reading that ends it becomes
/// the anchor the next one is measured against.
///
/// **What produces one today is a source that stops answering**, since a round
/// that times out publishes no reading at all. The two other explanations that
/// come to mind are both wrong here, and are written down so that nobody
/// reaches for them twice. A machine suspended to memory produces no gap,
/// because [`Instant`](std::time::Instant) reads `CLOCK_MONOTONIC` on Linux and
/// that clock does not count suspended time. A restarted daemon produces none
/// either, because a new timeline has no earlier reading at all and its first
/// one is a first reading rather than the far end of a gap - though that one
/// becomes real as soon as this state outlives the process.
///
/// **A gap is discarded even when the position resumes exactly where playing
/// through it would have left it**, and that is the point rather than a
/// shortcoming. A minute of playback and a drag of the bar a minute forward end
/// at the same position, and over an interval nobody watched there is nothing
/// to tell them apart. Counting it would let one drag mark an episode, which is
/// the reason [`Progress`] carries a position and a watched time rather than
/// one number.
///
/// Ten seconds, from the two bounds around it. **Below**, a source that fails to
/// answer misses that round entirely, and the detection crate suggests a one
/// second cadence, so a handful of consecutive timeouts is a handful of seconds
/// and must stay observed or a merely slow player loses its progress.
/// **Above**, an interval at or under this is counted in full whether or not
/// anyone saw it, so this is also the most watched time a single hiccup can
/// add: ten seconds of a twenty-four minute episode is under one percent of it.
pub const OBSERVED_LIMIT: Duration = Duration::from_secs(10);

/// How much of the media has to have been watched, as a percentage of its
/// length, unless [`WatchedPolicy`] is built with another.
///
/// **What this number is for.** It is the trigger that registers a title, not a
/// record of how much was watched. The only question it answers is when to
/// write the title down, and it is asked of [`Progress::watched`] alone.
///
/// **Why a percentage and not a count of seconds.** The same number of seconds
/// is the whole of a short and a fifth of a film. A percentage is the only form
/// that means the same thing for both.
///
/// **Why this is far below the whole file.** Media ends in credits, and a series
/// repeats them at both ends: roughly a minute and a half of opening and the
/// same of closing in a twenty-four minute episode, plus a preview. A viewer who
/// skips both can never accumulate more than about seven eighths of the file, so
/// a high setting is unreachable for the ordinary way of watching a series.
///
/// **Why it is half rather than more.** Registering a title does not require
/// finishing it. A viewer who stops in the middle has still watched the thing,
/// and whether they abandoned it or were called away is not something any
/// reading can tell - so it is not asked.
///
/// **Nothing measured here fixes this number**: what fraction of a file a viewer
/// reaches before stopping is a fact about viewers, and the one recording this
/// crate replays is sixty readings of one playback.
pub const WATCHED_PERCENT: u8 = 50;

/// The smallest percentage [`WatchedPolicy::new`] accepts.
///
/// A floor on the configuration rather than on the decision. Below this the
/// setting stops describing watching at all: a fifth of a twenty-four minute
/// episode is four minutes and forty-eight seconds, which nobody reaches by
/// opening a file to look at it, and anything lower starts to be reachable that
/// way.
///
/// It does not protect very short media on its own, because a fifth of a three
/// minute short is thirty-six seconds. [`WATCHED_MINIMUM`] is what covers that.
pub const WATCHED_PERCENT_MIN: u8 = 20;

/// The largest percentage [`WatchedPolicy::new`] accepts.
///
/// **A hundred is not reachable in one pass over a file and is therefore not
/// offered.** Watched time accumulates from the first reading, which arrives
/// after playback has already started, so one pass ends short of the length. A
/// setting nothing can satisfy is a trap rather than a preference, and the same
/// argument takes the ceiling well below a hundred: skipping the opening of a
/// twenty-four minute episode leaves fifteen sixteenths of it to accumulate
/// from, and skipping the closing as well leaves seven eighths.
///
/// Four fifths is reachable for a viewer who skips both.
pub const WATCHED_PERCENT_MAX: u8 = 80;

/// The least watched time that can count, whatever percentage is in force.
///
/// **Very short media is what this is for.** A fifth of a three minute short is
/// thirty-six seconds, which is the length of a look rather than of a viewing,
/// and the percentage alone cannot tell the two apart. One minute can.
///
/// **It never demands more than [`WATCHED_PERCENT_MAX`] of the media itself**,
/// and that cap is not a nicety. Checked against MyAnimeList on 2026-09-19:
/// `Jigazou` runs twelve seconds, `Sora Iro no Tane` thirty and
/// `Doubutsu Sumo Taikai` fifty-two, so media shorter than this minimum exists
/// and is catalogued. Without the cap, playing one of those through would not
/// register it, and nothing would say why.
///
/// Below a handful of seconds the poll cadence decides rather than this number:
/// the first reading arrives a poll into playback, so a file of a few seconds
/// leaves only a few readings to accumulate from.
///
/// It is also the smallest fallback [`WatchedPolicy::new`] accepts, because both
/// answer the same question - the least watched time worth registering - and two
/// numbers for one question drift apart.
pub const WATCHED_MINIMUM: Duration = Duration::from_mins(1);

/// The watched time that counts where no usable length arrives, unless
/// [`WatchedPolicy`] is built with another.
///
/// **When there is no length there is nothing to take a percentage of**, and the
/// cases are real: a live stream has no end to report, a source may not declare
/// the capability at all, and a length its own reading contradicts is discarded
/// before it reaches here.
///
/// **A nominal length is not the answer.** Standing in a twenty-four minute
/// episode or a ninety minute film would be a guess about media the source
/// deliberately did not describe, and a guess that is wrong is worse here than
/// no answer: it writes a title down on evidence that was invented.
///
/// So a flat span of watched time, and five minutes for the same reason
/// [`WATCHED_PERCENT_MIN`] exists - long enough that nobody reaches it by
/// opening something to look at it, short enough that a stream somebody is
/// actually watching gets registered.
pub const WATCHED_FALLBACK: Duration = Duration::from_mins(5);

/// The largest fallback [`WatchedPolicy::new`] accepts.
///
/// Three hours is within a single sitting, so every accepted value is one a
/// viewer can actually reach. Above this the setting means "never register a
/// source that reports no length", which is a thing to say plainly rather than
/// to express as a number nothing meets.
pub const WATCHED_FALLBACK_MAX: Duration = Duration::from_hours(3);

/// Why a [`WatchedPolicy`] could not be built.
///
/// Refused rather than corrected. A percentage quietly moved into range makes a
/// configuration file disagree with the program reading it, and nothing later
/// can tell the user which number is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WatchedPolicyError {
    /// The percentage is outside [`WATCHED_PERCENT_MIN`] to
    /// [`WATCHED_PERCENT_MAX`].
    #[error(
        "a watched percentage of {percent} is outside {WATCHED_PERCENT_MIN} to {WATCHED_PERCENT_MAX}"
    )]
    Percent {
        /// What was asked for.
        percent: u8,
    },
    /// The fallback is outside [`WATCHED_MINIMUM`] to [`WATCHED_FALLBACK_MAX`].
    #[error(
        "a fallback of {fallback:?} is outside {WATCHED_MINIMUM:?} to {WATCHED_FALLBACK_MAX:?}"
    )]
    Fallback {
        /// What was asked for.
        fallback: Duration,
    },
}

/// When enough of the media has been watched to register the title.
///
/// Two numbers, because a source is free to report no length. The percentage
/// applies to the length in force; the fallback is a flat span of watched time
/// that answers where there is none.
///
/// **The decision is taken from the watched time and never from the position.**
/// A viewer who drags the bar to the last minute has playback at the end and has
/// watched nothing, and holding those two apart is what [`Progress`] is shaped
/// for.
///
/// Both numbers are checked when the policy is built and are private afterwards,
/// so every policy that exists is one whose answer is reachable. What each bound
/// is for is written on the constant that sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchedPolicy {
    /// Checked against [`WATCHED_PERCENT_MIN`] and [`WATCHED_PERCENT_MAX`].
    percent: u8,
    /// Checked against [`WATCHED_MINIMUM`] and [`WATCHED_FALLBACK_MAX`].
    fallback: Duration,
}

impl Default for WatchedPolicy {
    /// [`WATCHED_PERCENT`] and [`WATCHED_FALLBACK`], which are in range by
    /// construction.
    fn default() -> Self {
        Self {
            percent: WATCHED_PERCENT,
            fallback: WATCHED_FALLBACK,
        }
    }
}

impl WatchedPolicy {
    /// Build a policy, refusing either number if it is out of range.
    ///
    /// # Errors
    ///
    /// [`WatchedPolicyError::Percent`] or [`WatchedPolicyError::Fallback`],
    /// naming the value that was refused. The percentage is checked first, so a
    /// configuration with both wrong reports the percentage.
    pub fn new(percent: u8, fallback: Duration) -> Result<Self, WatchedPolicyError> {
        if !(WATCHED_PERCENT_MIN..=WATCHED_PERCENT_MAX).contains(&percent) {
            return Err(WatchedPolicyError::Percent { percent });
        }
        if !(WATCHED_MINIMUM..=WATCHED_FALLBACK_MAX).contains(&fallback) {
            return Err(WatchedPolicyError::Fallback { fallback });
        }
        Ok(Self { percent, fallback })
    }

    /// The percentage of the length that has to have been watched.
    #[must_use]
    pub const fn percent(&self) -> u8 {
        self.percent
    }

    /// The watched time that answers where there is no usable length.
    #[must_use]
    pub const fn fallback(&self) -> Duration {
        self.fallback
    }

    /// Whether this answer registers the media as watched.
    ///
    /// The percentage where there is a length to take a percentage of, and the
    /// fallback where there is not. **Neither comparison is strict**: the rule
    /// is watched *at* the threshold, and a source whose last reading lands
    /// exactly on it is under no obligation to send another.
    ///
    /// A length of **zero** falls back rather than satisfying the percentage.
    /// The timeline cannot produce one, because a length of zero arrives absent,
    /// but [`Progress`] is a value with public fields and that refusal does not
    /// travel with it. Any percentage of zero is zero, which would register a
    /// viewer who has watched nothing at all.
    ///
    /// The fallback needs no floor of its own: every accepted fallback is at
    /// least [`WATCHED_MINIMUM`] already.
    #[must_use]
    pub fn counts_as_watched(&self, progress: &Progress) -> bool {
        match progress.duration {
            Known::Value(length) if !length.is_zero() => {
                progress.watched >= self.threshold_for(length)
            }
            Known::Value(_) | Known::NotReported | Known::Unsupported => {
                progress.watched >= self.fallback
            }
        }
    }

    /// The watched time this policy asks for from media of `length`.
    ///
    /// The percentage of the length, raised to [`WATCHED_MINIMUM`] where that is
    /// more - and that floor is itself held down to [`WATCHED_PERCENT_MAX`] of
    /// the length, so it can never ask for more of a file than the largest
    /// setting would. Media shorter than about a minute and a quarter is decided
    /// by the cap rather than by either number.
    ///
    /// Divided before it is multiplied, which cannot overflow, where multiplying
    /// first can. The division truncates by under a nanosecond and the
    /// multiplication scales that by at most [`WATCHED_PERCENT_MAX`], so the
    /// threshold is under 80 nanoseconds low against a poll cadence of a second.
    fn threshold_for(&self, length: Duration) -> Duration {
        let hundredth = length / 100;
        let asked = hundredth * u32::from(self.percent);
        let floor = WATCHED_MINIMUM.min(hundredth * u32::from(WATCHED_PERCENT_MAX));
        asked.max(floor)
    }
}

/// How long the media is, with a length its own reading contradicts discarded.
///
/// Players lie about length, and the two values refused here are the ones a
/// consumer below has no way to tell from the truth. A length **below the
/// position it arrives with** describes media playback has already run past,
/// which a genuinely short file never does. A length of **zero** is no media at
/// all, and the contradiction rule reaches it only when a position arrives
/// above it: a position of zero is legal and reported, so a player at the start
/// of a file offers nothing for zero to be below.
///
/// Refused rather than corrected. The only correction available is the position
/// itself, since playback reached it, and a watched decision taken as a fraction
/// of that number would call every such file complete the moment it arrived. An
/// absence is a fact the fallback for a missing length has to answer for.
///
/// A position **equal** to the length is the last instant of the media and is
/// kept. The comparison is strict for that reason, and because the contract
/// every adapter is held to states the rule in those words: a duration is never
/// shorter than its position.
///
/// [`Known::Unsupported`] is carried through rather than folded into
/// [`Known::NotReported`], because a source that cannot report a length and one
/// that reported an impossible one are different facts: the first is settled for
/// the life of the source, the second is about this reading alone.
///
/// **This is the second place the rule lives.** The MPRIS adapter refuses the
/// same two values before a reading is ever built. It is repeated here because a
/// recording is a text file anyone can edit, and because the adapter that has
/// not been written yet is not bound by the one that has.
fn duration_of(reading: &PlayerSnapshot) -> Known<Duration> {
    let Known::Value(duration) = reading.duration else {
        return reading.duration;
    };
    if duration.is_zero() {
        return Known::NotReported;
    }

    match reading.position {
        Known::Value(position) if duration < position => Known::NotReported,
        // Both absences answer alike, though they are different facts: with no
        // position reported there is nothing a length can contradict.
        Known::Value(_) | Known::NotReported | Known::Unsupported => Known::Value(duration),
    }
}

/// The part of a reading the next one is measured against.
///
/// Only the three fields the fold needs, rather than the whole reading, so
/// that what the timeline remembers is visible at a glance.
#[derive(Debug, Clone, Copy)]
struct Previous {
    /// When it was taken, on the clock its source was read with.
    observed_at: Timestamp,
    /// What it reported, which is what the interval after it counts as.
    state: PlayState,
    /// Where it said playback was, which the next position is predicted from.
    position: Known<Duration>,
}

impl Previous {
    /// How far the position should have moved by the time of the next reading.
    ///
    /// The interval when this reading was playing, and nothing when it was not.
    /// Which of the two readings the state comes from cannot be told apart from
    /// here, and no test can pin it: the one caller has already returned when
    /// the two disagree, so at the point this is reached they are the same.
    fn expects(self, reading: &PlayerSnapshot) -> Duration {
        match self.state {
            PlayState::Playing => reading.observed_at.since(self.observed_at),
            PlayState::Paused | PlayState::Stopped => Duration::ZERO,
        }
    }

    /// Whether the interval up to `reading` is short enough to have been seen.
    ///
    /// Compared against [`OBSERVED_LIMIT`], and not strictly: an interval of
    /// exactly the limit is a daemon under load rather than one that was not
    /// running.
    fn observed(self, reading: &PlayerSnapshot) -> bool {
        reading.observed_at.since(self.observed_at) <= OBSERVED_LIMIT
    }
}

/// The state a sequence of readings is folded through.
///
/// One continuous playback of one thing, and nothing here notices otherwise.
/// No rule reads [`PlayerSnapshot::media`], so a source that finishes one file
/// and opens another goes on adding to the watched time of the first.
#[derive(Debug, Default)]
pub struct Timeline {
    /// Watched time accumulated over every reading so far.
    watched: Duration,
    /// The reading this one will be measured against, absent until the first.
    previous: Option<Previous>,
    /// Playback the position has not caught up with yet.
    ///
    /// Only ever what the run of readings ending at the last one fell behind
    /// by: one interval of ordinary reporting settles it, whether or not the
    /// position ever made it up.
    stalled: Duration,
}

impl Timeline {
    /// A timeline that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            watched: Duration::ZERO,
            previous: None,
            stalled: Duration::ZERO,
        }
    }

    /// Whether the step from one reading to the next is a break in playback,
    /// carrying over what the position still owes.
    ///
    /// The position is predicted from the earlier reading: it advances by the
    /// interval when that reading was playing and stays put when it was paused.
    /// Nothing here mentions where playback started, and that is the whole of
    /// it. A position drifts from its first anchor over a long file for reasons
    /// that are not seeks, so a rule anchored there reports one on every
    /// reading past the threshold and never stops.
    ///
    /// Three things can have happened to the position, and only one of them is
    /// ever a break.
    ///
    /// It went **backwards**, which no amount of reporting lag does, so its
    /// distance from the prediction is judged against [`SEEK_THRESHOLD`]
    /// directly.
    ///
    /// It advanced **less than the interval implies**, which is what a player
    /// that has not published a new position yet looks like. Never a break: a
    /// position that stands still has skipped nothing. What it did not report
    /// is remembered, because the reading that catches up has to be allowed to.
    ///
    /// It advanced **at least as far as the interval implies**, which is
    /// ordinary playback until the overshoot passes what the stall owed plus
    /// the threshold. Either way the stall is settled: a player owes its
    /// backlog at once, and one interval of ordinary reporting says the
    /// position is current again.
    fn broke_between(&mut self, previous: Previous, reading: &PlayerSnapshot) -> bool {
        // Every step settles what the position owed, so it is taken here and
        // only the one branch that extends it puts anything back. An interval
        // nothing can be read from is no exception: what was owed before it
        // cannot be made good across it.
        let owed = mem::take(&mut self.stalled);

        let (Known::Value(before), Known::Value(now)) = (previous.position, reading.position)
        else {
            return false;
        };
        // An interval the state changed inside says nothing about seeking: the
        // change happened at a moment this cannot name, and everything from
        // that moment to one end of the interval is unaccounted for. Measured
        // on this crate's recording, both ends of its one pause diverge by a
        // whole interval and neither is a seek.
        if previous.state != reading.state {
            return false;
        }

        let expected = previous.expects(reading);
        // Every subtraction below is guarded by the test just above it, so none
        // of them can saturate. They are written saturating anyway because a
        // plain one panics rather than going negative, and a lint denies it:
        // the guard is what makes the answer exact, not the method name.
        if now < before {
            return before.saturating_sub(now).saturating_add(expected) > SEEK_THRESHOLD;
        }

        let advanced = now.saturating_sub(before);
        if advanced < expected {
            self.stalled = owed + expected.saturating_sub(advanced);
            return false;
        }

        advanced.saturating_sub(expected) > owed.saturating_add(SEEK_THRESHOLD)
    }

    /// Fold one reading in, and answer with what it leaves behind.
    ///
    /// The position is the source's own answer, carried through exactly as it
    /// arrived. The length is the source's answer too, except where the reading
    /// it came with contradicts it: a length below its own position, or a
    /// length of zero, is published absent. The watched time and the seek are
    /// the timeline's, and no reading carries either: a seek is a break between
    /// two consecutive readings, and watched time is what a sequence of them
    /// adds up to.
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
        // An interval longer than `OBSERVED_LIMIT` is a gap, and the reading
        // before it is dropped here rather than in each rule below: nothing
        // inside a gap was seen, so there is nothing to count, nothing to
        // judge, and nothing the position can still be owed across it.
        let previous = self.previous.filter(|earlier| earlier.observed(reading));
        if previous.is_none() {
            self.stalled = Duration::ZERO;
        }

        if let Some(earlier) = previous
            && earlier.state == PlayState::Playing
        {
            self.watched += reading.observed_at.since(earlier.observed_at);
        }
        let seeked = previous.is_some_and(|earlier| self.broke_between(earlier, reading));
        self.previous = Some(Previous {
            observed_at: reading.observed_at,
            state: reading.state,
            position: reading.position,
        });

        Progress {
            position: reading.position,
            watched: self.watched,
            duration: duration_of(reading),
            seeked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OBSERVED_LIMIT, Progress, SEEK_THRESHOLD, Timeline, WATCHED_FALLBACK, WATCHED_FALLBACK_MAX,
        WATCHED_MINIMUM, WATCHED_PERCENT, WATCHED_PERCENT_MAX, WATCHED_PERCENT_MIN, WatchedPolicy,
        WatchedPolicyError,
    };
    use crate::clock::{Clock, TestClock, Timestamp};
    use crate::path::RawPath;
    use crate::trace::Trace;
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::time::Duration;

    /// One mpv playing one episode: 60 readings a second apart, of which 40 are
    /// playing and 20 are a single pause, and one backward seek near the end.
    const RECORDING: &str = include_str!("../tests/fixtures/mpv-one-episode.jsonl");

    // What the recording holds, measured from it on 2026-09-18. One note for
    // every constant below, so not a doc comment on the first of them. It said
    // "all three" until there were five, which is what a count in prose does.
    //
    // The readings are a second apart but not exactly: the shortest interval
    // is 998.109148 ms and the longest 1002.104074 ms, so a test that assumes
    // a round second asserts something the recording does not contain. Its 60
    // readings leave 59 intervals, of which 39 begin while playback is running
    // and span 39.001189719 s together, while 20 begin paused and span
    // 19.998711057 s; the two add up to the 58.999900776 s it covers.
    //
    // Nanoseconds and not microseconds, because the first measurement of this
    // divided them away and the total below was wrong by 719 of them.
    const PLAYING_INTERVALS: usize = 39;
    const PAUSED_INTERVALS: usize = 20;
    const WATCHED_IN_FULL: Duration = Duration::from_nanos(39_001_189_719);
    const SHORTEST_INTERVAL: Duration = Duration::from_nanos(998_109_148);
    const LONGEST_INTERVAL: Duration = Duration::from_nanos(1_002_104_074);
    const READINGS: usize = 60;

    // Every test that replays the recording assumes the whole of it was
    // observed, and a limit under its cadence makes every interval in the file
    // a gap. Checked rather than described, because the replay tests that then
    // fail say nothing about the limit, and one of them does not fail at all:
    // `an_interval_that_began_paused_adds_no_watched_time` asserts that nothing
    // is counted, and a fold of gaps counts nothing.
    const _: () = assert!(
        LONGEST_INTERVAL.as_nanos() <= OBSERVED_LIMIT.as_nanos(),
        "an observed limit below the recording's cadence makes the recording a gap"
    );

    /// The one break in the recording, also measured from it.
    ///
    /// Reading 50 reports 11.0 s where the reading before it reported
    /// 31.799999 s, which is 21.798419620 s less than the interval between them
    /// implies. Nanoseconds again, for the reason given above.
    const SEEK_AT_READING: usize = 50;

    /// The interval a run of readings uses unless it is about a longer one.
    const SECOND: Duration = Duration::from_secs(1);

    /// An interval long enough that nobody watched it.
    ///
    /// Named rather than written as a multiple of the limit, because the
    /// positions either side of a gap have to be consistent with playing
    /// through it and the arithmetic that makes them so needs the number.
    const A_GAP: Duration = Duration::from_secs(60);

    const _: () = assert!(
        A_GAP.as_nanos() > OBSERVED_LIMIT.as_nanos(),
        "an interval a test calls a gap has to be longer than the limit"
    );

    /// A position no source reported, for runs that are not about positions.
    const NOWHERE: Known<Duration> = Known::NotReported;

    /// The answer for a run holding no seek, named because an empty array
    /// otherwise needs its element type spelled out at every use.
    const NO_SEEKS: [usize; 0] = [];

    /// One reading to build: the interval since the reading before it, the
    /// state that reading reports, and the position it reports.
    type Step = (Duration, PlayState, Known<Duration>);

    /// A position `seconds` into the media, as a source would report it.
    const fn a_position(seconds: u64) -> Known<Duration> {
        Known::Value(Duration::from_secs(seconds))
    }

    /// A media `seconds` long, as a source would report its length.
    ///
    /// The same construction as [`a_position`] under the name of the argument
    /// it fills, because both arguments of `reading` are the same type and
    /// `reading(a_position(10), a_position(5))` reads as two positions.
    const fn a_length(seconds: u64) -> Known<Duration> {
        Known::Value(Duration::from_secs(seconds))
    }

    /// A run of readings, each taken the given interval after the one before.
    ///
    /// Built through a [`TestClock`] because a [`Timestamp`] comes only from a
    /// clock, which is the same reason the timeline needs none of its own. The
    /// clock moves before each reading is stamped, so the first sits one
    /// interval after the epoch and the others follow it. One clock for the
    /// whole run, so a test builds its steps and stamps them once.
    fn readings(steps: &[Step]) -> Vec<PlayerSnapshot> {
        let clock = TestClock::new();
        steps
            .iter()
            .map(|(interval, state, position)| {
                clock.advance(*interval);
                PlayerSnapshot {
                    state: *state,
                    observed_at: clock.now(),
                    ..reading(*position, Known::NotReported)
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
    fn a_length_below_its_position_is_absent_rather_than_believed() {
        // Players lie about length, and this is the lie nothing downstream can
        // tell from the truth: five seconds of media arriving with a position
        // ten seconds into it describes a file playback has already run past,
        // which a genuinely short file never does.
        //
        // Absent and not corrected. A corrected length is a number nobody
        // measured, and the one honest correction - the position, since
        // playback reached it - is the number the watched decision would then
        // be a fraction of, making every such file complete on arrival.
        let mut timeline = Timeline::new();

        let progress = timeline.advance(&reading(a_position(10), a_length(5)));

        assert_eq!(progress.duration, Known::NotReported);
        // The position survives. It is the source's own answer and it was not
        // what the reading contradicted, so a rule that threw away the pair
        // would discard the one half of it never in question.
        assert_eq!(progress.position, a_position(10));
    }

    #[test]
    fn a_position_at_the_very_end_of_its_length_is_not_a_contradiction() {
        // The boundary, and the only thing holding the comparison to a strict
        // one. A position equal to the length is the last instant of the media
        // rather than an impossible one, the contract every adapter is held to
        // permits it in those words, and the MPRIS adapter keeps it. Refusing
        // it here would take the length away exactly where a watched decision
        // needs one, and would put this crate at odds with the two places that
        // already answer.
        let mut timeline = Timeline::new();

        let progress = timeline.advance(&reading(a_position(600), a_length(600)));

        assert_eq!(progress.duration, a_length(600));
    }

    #[test]
    fn a_length_of_zero_is_absent_even_where_no_position_contradicts_it() {
        // Zero is a length no file has, and it is the one impossible value the
        // contradiction rule cannot reach: a position of zero is legal and the
        // MPRIS adapter deliberately keeps it, so a player sitting at the start
        // of a file offers nothing for zero to be below. Published as a value
        // it would make every consumer special-case a sentinel, which is what
        // `Known` exists to stop.
        let mut timeline = Timeline::new();

        let at_the_start = timeline.advance(&reading(a_position(0), a_length(0)));
        let unplaced = timeline.advance(&reading(NOWHERE, a_length(0)));

        assert_eq!(at_the_start.duration, Known::NotReported);
        assert_eq!(unplaced.duration, Known::NotReported);
    }

    #[test]
    fn a_length_stands_when_there_is_no_position_to_contradict_it() {
        // Both absences, and they answer alike here although they are different
        // facts. `Capabilities` declares a position and a length separately, so
        // a source reporting one and not the other is a source the type allows,
        // and a rule discarding a length whenever the position is missing would
        // leave that source with no length it can ever publish.
        let mut timeline = Timeline::new();

        let silent = timeline.advance(&reading(Known::NotReported, a_length(600)));
        let incapable = timeline.advance(&reading(Known::Unsupported, a_length(600)));

        assert_eq!(silent.duration, a_length(600));
        assert_eq!(incapable.duration, a_length(600));
    }

    #[test]
    fn the_recording_keeps_every_length_it_reports() {
        // The rule discards nothing a well-behaved player reports, which is
        // what separates it from one that fires on ordinary data: an inverted
        // comparison takes the length off all sixty readings here. The
        // constructed tests above catch that inversion too, and what they
        // cannot show is this: the rule stays quiet over a real player's own
        // numbers. The count at the end is what stops this from passing over a
        // recording that reports no length at all, where carrying nothing
        // through unchanged is trivially true.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");
        let mut timeline = Timeline::new();
        let mut reported = 0;

        for (index, reading) in trace.snapshots.iter().enumerate() {
            let published = timeline.advance(reading).duration;
            assert_eq!(
                published, reading.duration,
                "reading {index} reports a length this rule discarded"
            );
            if matches!(reading.duration, Known::Value(_)) {
                reported += 1;
            }
        }

        assert_eq!(reported, READINGS, "every reading reports a length");
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

        assert_eq!(watched_over(&trace.snapshots), WATCHED_IN_FULL);
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
        let watched = watched_over(&trace.snapshots);
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
        let readings = readings(&[
            (SECOND, PlayState::Stopped, NOWHERE),
            (SECOND, PlayState::Stopped, NOWHERE),
            (SECOND, PlayState::Playing, NOWHERE),
            (SECOND, PlayState::Playing, NOWHERE),
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

    /// Which readings of a run the timeline calls a seek, by their index.
    fn seeks_in(run: &[PlayerSnapshot]) -> Vec<usize> {
        let mut timeline = Timeline::new();
        let mut seeked = Vec::new();
        for (index, reading) in run.iter().enumerate() {
            if timeline.advance(reading).seeked {
                seeked.push(index);
            }
        }
        seeked
    }

    #[test]
    fn the_one_break_in_the_recording_is_the_one_seek_reported() {
        // Which reading and not how many: a rule that reported a different
        // reading, or that reported this one and two more, would satisfy a
        // count of one exactly as well for one of those answers.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");

        assert_eq!(seeks_in(&trace.snapshots), [SEEK_AT_READING]);
    }

    #[test]
    fn a_playback_drifting_from_where_it_started_reports_no_seek() {
        // Twenty minutes, each reading consistent with the one before it to
        // ten milliseconds and every one further from where playback started.
        // By the end the position is 11.99 s from what the first reading
        // implies, just under six times the threshold, and nothing was seeked.
        // This is the test a rule anchored on the start of playback fails.
        let drifting: Vec<_> = (0..20 * 60)
            .map(|step| {
                let position = Known::Value(Duration::from_millis(1010 * step));
                (SECOND, PlayState::Playing, position)
            })
            .collect();

        let run = readings(&drifting);

        // What a rule anchored on the start of playback would be looking at by
        // the end, computed from the readings themselves rather than from the
        // arithmetic that built them.
        let (first, last) = (&run[0], &run[run.len() - 1]);
        let (Known::Value(from), Known::Value(to)) = (first.position, last.position) else {
            panic!("the run reports its positions")
        };
        let anchored = to.abs_diff(from + last.observed_at.since(first.observed_at));
        assert!(
            anchored > SEEK_THRESHOLD * 5,
            "the run drifts only {anchored:?} from its start, too little to matter"
        );

        assert_eq!(seeks_in(&run), NO_SEEKS);
    }

    #[test]
    fn a_break_wider_than_the_threshold_is_a_seek_in_either_direction() {
        // The presence half, and both directions: a rule written with an
        // unsigned subtraction sees one of them and not the other.
        let forward = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (SECOND, PlayState::Playing, a_position(11)),
            (SECOND, PlayState::Playing, a_position(90)),
        ]);
        let backward = readings(&[
            (SECOND, PlayState::Playing, a_position(90)),
            (SECOND, PlayState::Playing, a_position(91)),
            (SECOND, PlayState::Playing, a_position(10)),
        ]);

        assert_eq!(seeks_in(&forward), [2]);
        assert_eq!(seeks_in(&backward), [2]);
    }

    #[test]
    fn a_step_back_is_judged_from_where_playback_should_have_reached() {
        // Two seconds back over a second of playback is three seconds from the
        // prediction, and a seek. The step itself is exactly the threshold and
        // no more, so a rule comparing it against the earlier position instead
        // of against the prediction reports nothing. The forward branches carry
        // the same term as an expected advance, where a run exactly the
        // threshold ahead pins it; this branch is the only place it can go
        // missing on its own.
        let stepped_back = readings(&[
            (SECOND, PlayState::Playing, a_position(30)),
            (SECOND, PlayState::Playing, a_position(28)),
        ]);

        assert_eq!(seeks_in(&stepped_back), [1]);
    }

    #[test]
    fn a_position_exactly_the_threshold_ahead_is_not_yet_a_seek() {
        // The boundary the constant's own reasoning rests on, and the only
        // thing holding the comparison to a strict one. A file at three times
        // speed read once a second runs exactly the threshold ahead of the
        // clock every interval; a rule that seeked on equality would report one
        // on every reading of it, for as long as it played.
        let fast = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (SECOND, PlayState::Playing, a_position(13)),
            (SECOND, PlayState::Playing, a_position(16)),
        ]);

        assert_eq!(seeks_in(&fast), NO_SEEKS);
    }

    #[test]
    fn a_position_that_moves_while_paused_is_a_seek() {
        // Nothing should advance while a player is paused, so a position that
        // does is the user dragging the bar. The expected advance is zero here
        // and the elapsed time is not, which is why the two are separate.
        //
        // Five readings at rest before the drag, and a drag of only four
        // seconds, both deliberately. A rule that expected a paused source to
        // advance like a playing one would read the four intervals between
        // those readings as a stall of four seconds, and would then forgive a
        // drag of up to seven; four seconds is well inside that.
        let dragged = readings(&[
            (SECOND, PlayState::Paused, a_position(30)),
            (SECOND, PlayState::Paused, a_position(30)),
            (SECOND, PlayState::Paused, a_position(30)),
            (SECOND, PlayState::Paused, a_position(30)),
            (SECOND, PlayState::Paused, a_position(30)),
            (SECOND, PlayState::Paused, a_position(34)),
        ]);

        assert_eq!(seeks_in(&dragged), [5]);
    }

    #[test]
    fn an_interval_the_state_changed_inside_is_never_a_seek() {
        // A player that changed state somewhere inside the interval, and the
        // recording shows what that costs: a whole interval of apparent
        // divergence with nothing seeked. Here the interval is long enough that
        // the divergence clears the threshold, which is the case a round
        // arriving late produces and a rule without this exception reports as
        // a seek.
        //
        // Both changes and not one. The recording holds both ends of its pause
        // and neither diverges past the threshold, so an exception written for
        // the pausing end alone satisfies the recording and everything else
        // here. The two ends also diverge in opposite directions: the position
        // stands still through an interval counted as playing, and moves
        // through one counted as paused.
        let paused_late = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (SEEK_THRESHOLD * 2, PlayState::Paused, a_position(10)),
        ]);
        let resumed_early = readings(&[
            (SECOND, PlayState::Paused, a_position(10)),
            (SEEK_THRESHOLD * 2, PlayState::Playing, a_position(14)),
        ]);

        assert_eq!(seeks_in(&paused_late), NO_SEEKS);
        assert_eq!(seeks_in(&resumed_early), NO_SEEKS);
    }

    #[test]
    fn a_reading_without_a_position_is_never_a_seek() {
        // There is nothing to compare, and both absences answer the same way
        // here although they are different facts: a source that cannot report
        // a position and one that did not this time are both unjudgeable.
        let silent = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (SECOND, PlayState::Playing, Known::NotReported),
            (SECOND, PlayState::Playing, Known::Unsupported),
            (SECOND, PlayState::Playing, a_position(300)),
        ]);

        assert_eq!(seeks_in(&silent), NO_SEEKS);
    }

    #[test]
    fn a_pause_longer_than_the_threshold_is_never_a_seek() {
        // While a source is paused the position is expected to stay where it
        // is, and the expectation has to be nothing rather than the elapsed
        // time. A rule predicting the same advance in both states reports a
        // seek on every reading of a pause whose rounds are further apart than
        // the threshold, and the recording cannot catch that: its rounds are a
        // second apart and its pause never diverges by more than one of them.
        let waiting = readings(&[
            (SECOND, PlayState::Paused, a_position(30)),
            (SEEK_THRESHOLD * 2, PlayState::Paused, a_position(30)),
            (SEEK_THRESHOLD * 2, PlayState::Paused, a_position(30)),
        ]);

        assert_eq!(seeks_in(&waiting), NO_SEEKS);
    }

    /// A player whose reported position stood still across `stalled` intervals
    /// while it went on playing, as the steps to build it from.
    ///
    /// Steps rather than readings so that a test can put more after them and
    /// have the whole run stamped by one clock. The recording holds no case of
    /// this, because mpv's position tracks the elapsed time to under two
    /// milliseconds; every stall in this file is built.
    fn a_stall_of(stalled: usize) -> Vec<Step> {
        vec![(SECOND, PlayState::Playing, a_position(12)); stalled + 1]
    }

    #[test]
    fn a_player_that_catches_up_after_stalling_has_not_seeked() {
        // Six readings at one position while playing, then one that moves by
        // the five seconds that went unreported plus the second that has just
        // passed. The jump is exactly what was owed, so nothing was skipped,
        // although judged on its size alone it clears the threshold three times
        // over.
        //
        // Five intervals of stall and not three, so that the run separates a
        // stall that accumulates from one that remembers only the last
        // reading's shortfall. The second would forgive an overshoot of one
        // second plus the threshold, and the overshoot here is five. At three
        // intervals the two answer alike and this test cannot tell them apart.
        let mut run = a_stall_of(5);
        run.push((SECOND, PlayState::Playing, a_position(18)));

        assert_eq!(seeks_in(&readings(&run)), NO_SEEKS);
    }

    #[test]
    fn a_position_that_stands_still_is_never_a_seek() {
        // A playing source whose position does not move has skipped nothing,
        // whatever the interval: it is a player that has not published a new
        // position yet, and there is no content it jumped over. The intervals
        // here are twice the threshold, which is what a round arriving late
        // during a stall looks like, and what a shortfall judged against the
        // threshold would report as a seek.
        let frozen = readings(&[
            (SECOND, PlayState::Playing, a_position(12)),
            (SEEK_THRESHOLD * 2, PlayState::Playing, a_position(12)),
            (SEEK_THRESHOLD * 2, PlayState::Playing, a_position(12)),
        ]);

        assert_eq!(seeks_in(&frozen), NO_SEEKS);
    }

    #[test]
    fn a_jump_past_what_the_stall_owed_is_a_seek() {
        // The absence half. A rule that forgives any forward jump following a
        // stall forgives a forward seek that happens to follow one, and this is
        // what separates the two: the same stall, and a jump far past it.
        let mut run = a_stall_of(3);
        run.push((SECOND, PlayState::Playing, a_position(300)));

        assert_eq!(seeks_in(&readings(&run)), [4]);
    }

    #[test]
    fn a_stall_is_forgotten_once_the_position_reports_normally() {
        // What a player owes is owed at once and not indefinitely. One interval
        // of ordinary reporting says the position is current again, so a jump
        // after that is judged against the threshold alone. Without this, a
        // stall forgives a seek of its own size at any later moment.
        let mut run = a_stall_of(3);
        run.push((SECOND, PlayState::Playing, a_position(13)));
        run.push((SECOND, PlayState::Playing, a_position(17)));

        assert_eq!(seeks_in(&readings(&run)), [5]);
    }

    #[test]
    fn a_stall_does_not_survive_a_change_of_state() {
        // An interval the state changed inside is unreadable, so what the
        // position owed before it cannot be settled across it. A stall of three
        // seconds, then a pause, then a four second drag: the drag is a seek,
        // and a stall that carried over would forgive it.
        let mut run = a_stall_of(3);
        run.push((SECOND, PlayState::Paused, a_position(12)));
        run.push((SECOND, PlayState::Paused, a_position(16)));

        assert_eq!(seeks_in(&readings(&run)), [5]);
    }

    #[test]
    fn a_stall_does_not_survive_a_position_that_went_backwards() {
        // A stall of three seconds, then a step back of one second, then a four
        // second jump. A second back over a second of playback sits exactly the
        // threshold from the prediction, so the step back is not itself a seek
        // and only the jump is reported. That jump is judged against the
        // threshold alone, and a stall that carried over would forgive it.
        let mut run = a_stall_of(3);
        run.push((SECOND, PlayState::Playing, a_position(11)));
        run.push((SECOND, PlayState::Playing, a_position(15)));

        assert_eq!(seeks_in(&readings(&run)), [5]);
    }

    #[test]
    fn a_stall_does_not_survive_a_reading_without_a_position() {
        // The same rule for the other unreadable interval. Every reading in the
        // recording carries a position, so only a built run puts this one to
        // the test: a stall of three seconds, then a reading that reports no
        // position at all, then a four second jump once positions are back. The
        // jump is a seek, and a stall that carried over would forgive it.
        let mut run = a_stall_of(3);
        run.push((SECOND, PlayState::Playing, NOWHERE));
        run.push((SECOND, PlayState::Playing, a_position(12)));
        run.push((SECOND, PlayState::Playing, a_position(16)));

        assert_eq!(seeks_in(&readings(&run)), [6]);
    }

    /// What the last reading of a whole run leaves behind.
    ///
    /// A plain loop, because folding with a side effect through an iterator
    /// chain is a trap: `.map(..).next_back()` reads as "the last answer" and
    /// advances the timeline exactly once, over the last reading. It was
    /// written that way first and the total came out zero. This helper is
    /// where the chain is most tempting, so the warning belongs here.
    fn progress_over(run: &[PlayerSnapshot]) -> Progress {
        let mut timeline = Timeline::new();
        let mut progress = None;
        for reading in run {
            progress = Some(timeline.advance(reading));
        }
        progress.expect("a run holds at least one reading")
    }

    /// How much the timeline counts as watched over a whole run.
    fn watched_over(run: &[PlayerSnapshot]) -> Duration {
        progress_over(run).watched
    }

    #[test]
    fn a_gap_nobody_watched_counts_as_nothing_watched() {
        // The daemon was restarted, or the machine slept, or the source stopped
        // answering for a minute. The position resumes exactly where a minute
        // of playback would have left it, which is also exactly where a minute
        // forward on the seek bar would have left it, and the two cannot be
        // told apart. Counting it would let one drag of the bar mark an
        // episode, which is the whole reason a position and a watched time are
        // two fields and not one.
        // The position after the gap is derived from the gap and not written
        // as a literal: the point of the run is that playing through the gap
        // and dragging the bar across it end in the same place, and a literal
        // is only consistent with the interval in the author's head.
        let resumed = 11 + A_GAP.as_secs();
        let gap = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (SECOND, PlayState::Playing, a_position(11)),
            (A_GAP, PlayState::Playing, a_position(resumed)),
            (SECOND, PlayState::Playing, a_position(resumed + 1)),
        ]);

        assert_eq!(watched_over(&gap), SECOND * 2);
    }

    #[test]
    fn a_gap_nobody_watched_is_never_a_seek() {
        // The other half, and the reason the gap is judged before the break
        // is: over an unobserved minute the position can be anywhere at all,
        // and a timeline that called that a seek would report one every time a
        // player was left running while the daemon was not.
        let gap = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (A_GAP, PlayState::Playing, a_position(500)),
            (SECOND, PlayState::Playing, a_position(501)),
        ]);

        assert_eq!(seeks_in(&gap), NO_SEEKS);
    }

    #[test]
    fn an_interval_of_exactly_the_limit_was_still_observed() {
        // The boundary, and the only thing keeping the comparison from becoming
        // a strict one. A run of intervals each exactly at the limit is a daemon
        // under load rather than a daemon that was not running, and every second
        // of it is counted.
        let slow = readings(&[
            (SECOND, PlayState::Playing, a_position(10)),
            (OBSERVED_LIMIT, PlayState::Playing, a_position(20)),
            (OBSERVED_LIMIT, PlayState::Playing, a_position(30)),
        ]);

        assert_eq!(watched_over(&slow), OBSERVED_LIMIT * 2);
        assert_eq!(seeks_in(&slow), NO_SEEKS);
    }

    #[test]
    fn a_gap_clears_what_the_position_owed() {
        // A stall of three seconds, then a gap, then a four second jump. The
        // jump is a seek: whatever the position owed before an interval nobody
        // watched cannot be made good across it, for the same reason a change
        // of state settles it.
        let mut run = a_stall_of(3);
        run.push((A_GAP, PlayState::Playing, a_position(12)));
        run.push((SECOND, PlayState::Playing, a_position(16)));

        assert_eq!(seeks_in(&readings(&run)), [5]);
    }

    /// The answer a caller holds, with everything the decision ignores fixed.
    ///
    /// The position among them, and deliberately: a rule reading the position
    /// where it should read the watched time finds nothing here to read.
    fn progress(watched: Duration, duration: Known<Duration>) -> Progress {
        Progress {
            position: NOWHERE,
            watched,
            duration,
            seeked: false,
        }
    }

    /// A run of `rounds` readings a second apart, playing media `length` long.
    ///
    /// [`readings`] reports no length at all, which sends every decision below
    /// to the fallback; this is the same run with a length on each reading.
    fn playing_for(length: Duration, rounds: u64) -> Vec<PlayerSnapshot> {
        let steps: Vec<Step> = (0..rounds)
            .map(|round| (SECOND, PlayState::Playing, a_position(round)))
            .collect();
        readings(&steps)
            .into_iter()
            .map(|snapshot| PlayerSnapshot {
                duration: Known::Value(length),
                ..snapshot
            })
            .collect()
    }

    #[test]
    fn an_episode_is_watched_at_the_configured_percentage_of_its_length() {
        // Both halves. A ten minute file and 361 readings a second apart, which
        // is 360 intervals and six minutes of watched time: past the default
        // half of it, short of the largest setting's four fifths. Built rather
        // than replayed, for the reason the test below this one states.
        let run = playing_for(Duration::from_mins(10), 361);
        let progress = progress_over(&run);
        assert_eq!(progress.watched, Duration::from_mins(6));

        let strictest = WatchedPolicy::new(WATCHED_PERCENT_MAX, WATCHED_MINIMUM)
            .expect("the largest percentage is in range");

        assert!(WatchedPolicy::default().counts_as_watched(&progress));
        assert!(!strictest.counts_as_watched(&progress));
    }

    #[test]
    fn the_recording_cannot_reach_the_smallest_percentage_that_may_be_set() {
        // The trap in this rule, stated as a test rather than left as a comment.
        // The recording holds 39 seconds of playback against a ten minute file,
        // which is under a fifteenth of it, so no policy that can be built marks
        // it watched. An implementation that registers nothing ever passes this
        // exactly as well, which is why the test above it is built by hand.
        let trace = Trace::from_jsonl(RECORDING).expect("the recording parses");
        let progress = progress_over(&trace.snapshots);

        let Known::Value(length) = progress.duration else {
            panic!("the recording reports a length")
        };
        let reached = 100.0 * progress.watched.as_secs_f64() / length.as_secs_f64();
        assert!(
            reached < f64::from(WATCHED_PERCENT_MIN),
            "the recording reaches {reached}% of its length, inside the range"
        );

        let loosest = WatchedPolicy::new(WATCHED_PERCENT_MIN, WATCHED_MINIMUM)
            .expect("the smallest percentage is in range");

        assert!(!loosest.counts_as_watched(&progress));
        assert!(!WatchedPolicy::default().counts_as_watched(&progress));
    }

    #[test]
    fn a_twenty_minute_episode_and_a_ninety_minute_film_differ_in_seconds() {
        // Why the threshold is a percentage and not a count of seconds. The same
        // twelve minutes is past half the episode and under a quarter of the
        // film, so no single number of seconds answers for both.
        let policy = WatchedPolicy::default();
        let twelve_minutes = Duration::from_mins(12);

        let episode = progress(twelve_minutes, a_length(20 * 60));
        let film = progress(twelve_minutes, a_length(90 * 60));

        assert!(policy.counts_as_watched(&episode));
        assert!(!policy.counts_as_watched(&film));
    }

    #[test]
    fn a_duration_that_cannot_be_reported_falls_back_to_a_timer() {
        // Both absences, separately. They answer alike here and they are still
        // different facts: a source that cannot report a length has settled the
        // question for its lifetime, while one that did not report a length this
        // time may report one in the next reading.
        let policy = WatchedPolicy::default();
        let plenty = policy.fallback() * 2;
        let short = policy.fallback().saturating_sub(SECOND);

        assert!(policy.counts_as_watched(&progress(plenty, Known::NotReported)));
        assert!(policy.counts_as_watched(&progress(plenty, Known::Unsupported)));
        assert!(!policy.counts_as_watched(&progress(short, Known::NotReported)));
        assert!(!policy.counts_as_watched(&progress(short, Known::Unsupported)));
    }

    #[test]
    fn exactly_the_threshold_is_already_enough_in_either_branch() {
        // Watched *at* the threshold, on the percentage and on the fallback:
        // neither comparison is strict. A source whose last reading lands
        // exactly on the threshold is under no obligation to send another, and a
        // strict rule would be waiting for one that never comes.
        //
        // Ten minutes is half of twenty exactly, so the percentage branch is
        // tested on its boundary rather than near it.
        let policy = WatchedPolicy::default();
        let half = progress(Duration::from_mins(10), a_length(20 * 60));

        assert!(policy.counts_as_watched(&half));
        assert!(policy.counts_as_watched(&progress(policy.fallback(), Known::NotReported)));
    }

    #[test]
    fn a_length_of_zero_falls_back_rather_than_registering_everything() {
        // `Timeline` cannot produce this, because a length of zero arrives
        // absent, but `Progress` is a value with public fields and that refusal
        // does not travel with it. Any percentage of zero is zero, so the rule
        // would register a viewer who has watched nothing at all - the one
        // failure this crate exists to prevent.
        let policy = WatchedPolicy::default();

        assert!(!policy.counts_as_watched(&progress(Duration::ZERO, a_length(0))));
        assert!(policy.counts_as_watched(&progress(policy.fallback(), a_length(0))));
    }

    #[test]
    fn a_short_is_held_to_the_minimum_rather_than_to_its_percentage() {
        // A three minute short at the smallest percentage that can be
        // configured asks for thirty-six seconds, which is the length of a look
        // rather than of a viewing. The minimum is what the percentage cannot
        // express, and it is the whole reason there are two numbers here.
        let loosest = WatchedPolicy::new(WATCHED_PERCENT_MIN, WATCHED_MINIMUM)
            .expect("the smallest percentage is in range");
        let short = a_length(3 * 60);

        assert!(!loosest.counts_as_watched(&progress(Duration::from_secs(40), short)));
        assert!(loosest.counts_as_watched(&progress(WATCHED_MINIMUM, short)));
    }

    #[test]
    fn the_minimum_never_asks_more_of_a_file_than_the_largest_setting_would() {
        // Media shorter than the minimum itself exists and is catalogued: read
        // off MyAnimeList on 2026-09-19, `Jigazou` runs twelve seconds. Without
        // the cap its threshold would be a minute, five times the file, and it
        // could never be registered at all while nothing said why.
        //
        // Twelve seconds, so the cap decides and the percentage does not: four
        // fifths of the file is 9.6 s, and the default half of it is 6 s.
        let policy = WatchedPolicy::default();
        let jigazou = a_length(12);

        assert!(!policy.counts_as_watched(&progress(Duration::from_millis(9_599), jigazou)));
        assert!(policy.counts_as_watched(&progress(Duration::from_millis(9_600), jigazou)));
    }

    #[test]
    fn a_percentage_outside_its_range_is_refused_and_each_bound_is_not() {
        // Refused and not corrected: a number quietly moved into range makes a
        // configuration file disagree with the program reading it, and the error
        // carries what was asked for so the disagreement can be named.
        let below = WATCHED_PERCENT_MIN - 1;
        let above = WATCHED_PERCENT_MAX + 1;

        assert_eq!(
            WatchedPolicy::new(below, WATCHED_FALLBACK),
            Err(WatchedPolicyError::Percent { percent: below })
        );
        assert_eq!(
            WatchedPolicy::new(above, WATCHED_FALLBACK),
            Err(WatchedPolicyError::Percent { percent: above })
        );
        assert!(WatchedPolicy::new(WATCHED_PERCENT_MIN, WATCHED_FALLBACK).is_ok());
        assert!(WatchedPolicy::new(WATCHED_PERCENT_MAX, WATCHED_FALLBACK).is_ok());

        // Both wrong reports the percentage, which is the only thing the order
        // of the two checks promises and the only thing that pins it.
        assert_eq!(
            WatchedPolicy::new(below, Duration::ZERO),
            Err(WatchedPolicyError::Percent { percent: below })
        );
    }

    #[test]
    fn a_fallback_outside_its_range_is_refused_and_each_bound_is_not() {
        // The other field, and both of its bounds. The lower one is the same
        // number as the minimum under the percentage rule, because both answer
        // the least watched time worth registering.
        let below = WATCHED_MINIMUM.saturating_sub(SECOND);
        let above = WATCHED_FALLBACK_MAX + SECOND;

        assert_eq!(
            WatchedPolicy::new(WATCHED_PERCENT, below),
            Err(WatchedPolicyError::Fallback { fallback: below })
        );
        assert_eq!(
            WatchedPolicy::new(WATCHED_PERCENT, above),
            Err(WatchedPolicyError::Fallback { fallback: above })
        );
        assert!(WatchedPolicy::new(WATCHED_PERCENT, WATCHED_MINIMUM).is_ok());
        assert!(WatchedPolicy::new(WATCHED_PERCENT, WATCHED_FALLBACK_MAX).is_ok());
    }

    #[test]
    fn the_default_policy_is_one_that_could_have_been_configured() {
        // The `Default` impl skips the checks, so nothing but this stops a
        // default that no caller would be allowed to ask for.
        let policy = WatchedPolicy::default();

        assert_eq!(
            WatchedPolicy::new(policy.percent(), policy.fallback()),
            Ok(policy)
        );
    }
}
