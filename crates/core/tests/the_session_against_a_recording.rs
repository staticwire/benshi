//! The recording, from the file to what a store is asked to do.
//!
//! Through the public API alone: parse a trace, decide each reading's file
//! against a list, hand both to a session, and read what it answers.
//!
//! The recording is sixty readings of one mpv playing one episode, with a
//! pause of twenty seconds in it. It holds thirty-nine seconds of playback
//! against a file of six hundred, which no policy that can be built
//! registers, so what it shows of a session is the keeping: a total for each
//! interval that was played, and no episode recorded.

use std::time::Duration;

use benshi_core::path::RawPath;
use benshi_core::recognise::altname::Altnames;
use benshi_core::recognise::corpus::Corpus;
use benshi_core::recognise::index::Index;
use benshi_core::recognise::parse::{Episode, parse};
use benshi_core::recognise::{Match, Recognition, Stage, decide};
use benshi_core::session::{Effect, Session};
use benshi_core::timeline::{WATCHED_MINIMUM, WATCHED_PERCENT_MIN, WatchedPolicy};
use benshi_core::trace::Trace;
use benshi_core::{MediaRef, PlayerSnapshot};

const RECORDING: &str = include_str!("fixtures/mpv-one-episode.jsonl");

// What the recording holds, in whole nanoseconds, and how many of its
// fifty-nine intervals began playing. Restated here because a constant of the
// crate's own tests does not cross the crate boundary.
const WATCHED_IN_FULL: Duration = Duration::from_nanos(39_001_189_719);
const PLAYING_INTERVALS: usize = 39;

/// The title the list holds for what the recording plays.
const TITLE: &str = "Test Show";

fn recording() -> Vec<PlayerSnapshot> {
    Trace::from_jsonl(RECORDING)
        .expect("the recording parses")
        .snapshots
}

/// What recognition answers for the file a reading names, against a list
/// holding the title.
fn decided(reading: &PlayerSnapshot) -> Recognition {
    let MediaRef::LocalFile(path) = &reading.media else {
        panic!("the recording plays a file")
    };
    let list: Corpus = [TITLE].into_iter().collect();
    let name = RawPath::from_bytes(path.file_name().to_vec());

    decide(&parse(&name), &Altnames::new(), &list, &Index::of(&list))
}

#[test]
fn the_recording_is_kept_interval_by_interval_and_never_recorded() {
    // The policy is the loosest that can be built, and the recording stays
    // under it: a fifth of the six hundred seconds the file runs is two
    // minutes, and the recording holds thirty-nine seconds of playback.
    //
    // Nothing recorded is also what a session that answers nothing produces,
    // so the totals are asserted first: one for each interval that began
    // playing, the last of them everything the recording holds.
    let readings = recording();
    assert_eq!(
        decided(&readings[0]),
        Recognition::Recognised(Match {
            title: TITLE.to_owned(),
            episode: Episode::Only(3),
            stage: Stage::Key,
        })
    );
    let loosest = WatchedPolicy::new(WATCHED_PERCENT_MIN, WATCHED_MINIMUM)
        .expect("the smallest percentage and the smallest fallback are in range");
    let mut session = Session::new(loosest);

    let mut kept = Vec::new();
    let mut recorded = 0;
    for reading in &readings {
        for effect in session.advance(reading, Some(&decided(reading))) {
            match effect {
                Effect::Resume {
                    player,
                    title,
                    episode,
                } => session.resumed(&player, &title, episode, None),
                Effect::Keep { watched, .. } => kept.push(watched),
                Effect::Record { .. } => recorded += 1,
                Effect::Close { .. } => panic!("the recording plays one file to its end"),
            }
        }
    }

    assert_eq!(kept.len(), PLAYING_INTERVALS);
    assert_eq!(kept.last(), Some(&WATCHED_IN_FULL));
    assert!(
        kept.windows(2).all(|pair| pair[0] < pair[1]),
        "a total was kept that is no larger than the one before it"
    );
    assert_eq!(recorded, 0);
}
