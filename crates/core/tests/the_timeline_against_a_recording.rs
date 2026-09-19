//! The recording, from the file to the decision.
//!
//! An integration test rather than more unit tests, for two reasons that the
//! tests inside `timeline` cannot cover between them.
//!
//! **It uses nothing but the public API.** Everything here is reachable from
//! outside the crate: parse a trace, fold it, read the answer, ask a policy
//! about it. If any of that stops being possible from outside, this file stops
//! compiling, and nothing in the module's own tests would notice.
//!
//! **Two of its six tests assert over a whole sequence rather than one rule at
//! a time.** The watched time never goes backwards across sixty readings, and a
//! decision that has become true never goes back; neither is a statement any
//! single rule can be checked for. The other four have twins in the module and
//! are here because they arrive through the public API instead.
//!
//! What this file does **not** currently do is catch something alone, and that
//! was measured rather than assumed. Of the fifty-two breaks in the list that
//! proves these tests can fail, sixteen redden a test here and every one of
//! those reddens a test in the module as well. That is a fact about today's
//! breaks and not a reason to drop the file; a break that reaches only here is
//! the thing to write when one is wanted.
//!
//! The recording is sixty readings of one mpv playing one episode: a pause of
//! twenty seconds, one backward seek, and no dishonesty anywhere. It holds
//! thirty-nine seconds of playback against a file of six hundred, which no
//! policy that can be built registers, so the flip in the watched decision is
//! tested on a run that continues past it - see the test that does.

use std::time::Duration;

use benshi_core::clock::{Clock, TestClock, Timestamp};
use benshi_core::timeline::{
    Progress, Timeline, WATCHED_MINIMUM, WATCHED_PERCENT_MIN, WatchedPolicy,
};
use benshi_core::trace::Trace;
use benshi_core::{Known, PlayerSnapshot};

const RECORDING: &str = include_str!("fixtures/mpv-one-episode.jsonl");

// What the recording holds. The watched time was measured from it on
// 2026-09-18 in whole nanoseconds, because an earlier measurement divided them
// away and came out 719 of them wrong; the other two are counts. Restated here
// rather than shared with the module's own tests, which cannot export a
// `#[cfg(test)]` constant across a crate boundary, and two literals that have to
// agree is the cost of reaching the crate from outside.
const WATCHED_IN_FULL: Duration = Duration::from_nanos(39_001_189_719);
const SEEK_AT_READING: usize = 50;
const READINGS: usize = 60;

/// One second, the cadence the continued run is built at.
const SECOND: Duration = Duration::from_secs(1);

/// Fold a whole run and keep every answer it produced.
///
/// `collect` drives the iterator to the end, which is what makes the side
/// effect in `advance` safe to write as a chain here. A chain that stops early
/// would fold part of the run and look like it folded all of it.
fn fold(run: &[PlayerSnapshot]) -> Vec<Progress> {
    let mut timeline = Timeline::new();
    run.iter()
        .map(|reading| timeline.advance(reading))
        .collect()
}

/// The recording, parsed.
fn recording() -> Vec<PlayerSnapshot> {
    Trace::from_jsonl(RECORDING)
        .expect("the recording parses")
        .snapshots
}

#[test]
fn the_whole_recording_folds_to_the_watched_time_it_holds() {
    let answers = fold(&recording());

    assert_eq!(answers.len(), READINGS);
    assert_eq!(
        answers
            .last()
            .expect("the recording holds readings")
            .watched,
        WATCHED_IN_FULL
    );
}

#[test]
fn the_whole_recording_reports_one_seek_and_names_its_reading() {
    // Which reading and not how many: a rule reporting a different one, or this
    // one and two others, satisfies a count of one just as well.
    let seeked: Vec<usize> = fold(&recording())
        .iter()
        .enumerate()
        .filter(|(_, progress)| progress.seeked)
        .map(|(index, _)| index)
        .collect();

    assert_eq!(seeked, [SEEK_AT_READING]);
}

#[test]
fn the_watched_time_never_goes_backwards_across_the_recording() {
    // The sequence property, and the recording holds the obvious way to lose
    // it. A seek moves the position and is not allowed to move the watched
    // time, so the reading the seek is reported at grows like any other: the
    // interval before it began playing, and playing is all the fold asks.
    let answers = fold(&recording());

    for (index, pair) in answers.windows(2).enumerate() {
        assert!(
            pair[1].watched >= pair[0].watched,
            "reading {} lost watched time, {:?} down to {:?}",
            index + 1,
            pair[0].watched,
            pair[1].watched
        );
    }

    assert!(
        answers[SEEK_AT_READING].watched > answers[SEEK_AT_READING - 1].watched,
        "the interval the seek was reported on began playing and has to count"
    );
}

#[test]
fn every_position_and_length_the_recording_reports_comes_through() {
    // The two fields the timeline carries rather than computes. A length is
    // discarded where it is zero or below its own position, and a well-behaved
    // player sends neither, so the rule fires on nothing here; the position is
    // never the timeline's to change at all.
    let readings = recording();
    let answers = fold(&readings);
    let mut lengths = 0;

    for (index, (reading, progress)) in readings.iter().zip(&answers).enumerate() {
        assert_eq!(progress.position, reading.position, "reading {index}");
        assert_eq!(progress.duration, reading.duration, "reading {index}");
        if matches!(reading.duration, Known::Value(_)) {
            lengths += 1;
        }
    }

    assert_eq!(lengths, READINGS, "every reading reports a length");
}

#[test]
fn no_policy_that_can_be_built_registers_the_recording() {
    // A fact about the recording rather than a shortcoming of it, and the
    // reason the flip below is tested on a run that continues past this one.
    // Thirty-nine seconds of playback against a file of six hundred is under a
    // fifteenth of it, and the smallest percentage that can be configured is a
    // fifth. Both bounds are asserted from the recording's own numbers, because
    // "nothing registered" is also what an implementation that registers
    // nothing ever would produce.
    let answers = fold(&recording());
    let ending = answers.last().expect("the recording holds readings");
    let Known::Value(length) = ending.duration else {
        panic!("the recording reports a length")
    };

    assert!(
        ending.watched < WATCHED_MINIMUM,
        "the recording holds {:?}, past the minimum on its own",
        ending.watched
    );
    assert!(
        ending.watched * 100 < length * u32::from(WATCHED_PERCENT_MIN),
        "the recording reaches the smallest percentage that may be set"
    );

    let loosest = WatchedPolicy::new(WATCHED_PERCENT_MIN, WATCHED_MINIMUM)
        .expect("the smallest percentage and the smallest fallback are in range");
    for (index, progress) in answers.iter().enumerate() {
        assert!(
            !loosest.counts_as_watched(progress),
            "reading {index} registered the recording"
        );
    }
}

/// The recording, and after it a run of readings a second apart carrying the
/// same media, the same length, and a position advancing by the interval.
///
/// Built through a [`TestClock`] wound forward to the recording's last reading,
/// so the first built reading sits one ordinary interval after the real one and
/// the join is not a seek, a stall or a gap.
fn the_recording_and_then(rounds: u64) -> Vec<PlayerSnapshot> {
    let mut readings = recording();
    let last = readings
        .last()
        .expect("the recording holds readings")
        .clone();
    let Known::Value(from) = last.position else {
        panic!("the recording reports a position")
    };

    let clock = TestClock::new();
    clock.advance(last.observed_at.since(Timestamp::epoch()));
    for round in 1..=rounds {
        clock.advance(SECOND);
        readings.push(PlayerSnapshot {
            position: Known::Value(from + SECOND * u32::try_from(round).expect("a small count")),
            observed_at: clock.now(),
            ..last.clone()
        });
    }
    readings
}

#[test]
fn a_playback_continuing_past_the_recording_registers_where_the_watched_time_says() {
    // The flip, on a sequence whose first sixty readings are a real player's.
    //
    // The expectation is a fifth of the length rather than the rule itself,
    // because re-deriving the rule here would only repeat a mistake the rule is
    // free to make. Two things make a fifth the right expectation and each is
    // true for its own reason. The minimum does not bind for media this long,
    // which is asserted below before it is relied on, so the rule comes down to
    // the percentage alone. And `length / 5` equals the rule's own
    // `length / 100 * 20` for this fixture because its length divides into
    // hundredths exactly; the rule truncates and a fifth does not, so the two
    // agree here rather than everywhere.
    let readings = the_recording_and_then(200);
    let answers = fold(&readings);
    let Known::Value(length) = answers[0].duration else {
        panic!("the recording reports a length")
    };

    let fifth = length / 5;
    assert!(
        fifth > WATCHED_MINIMUM,
        "the minimum binds for media this short and a fifth is not what is asked"
    );

    let policy = WatchedPolicy::new(WATCHED_PERCENT_MIN, WATCHED_MINIMUM)
        .expect("the smallest percentage and the smallest fallback are in range");
    let flipped = answers
        .iter()
        .position(|progress| policy.counts_as_watched(progress))
        .expect("the continued run reaches the threshold");

    assert!(
        flipped >= READINGS,
        "reading {flipped} of the recording itself registered"
    );
    assert!(
        answers[flipped].watched >= fifth,
        "the flip came before a fifth of the media had been watched"
    );
    assert!(
        answers[flipped - 1].watched < fifth,
        "the reading before the flip had already watched a fifth"
    );
    assert!(
        answers[flipped..]
            .iter()
            .all(|progress| policy.counts_as_watched(progress)),
        "the decision went back to unwatched after it was true"
    );
}
