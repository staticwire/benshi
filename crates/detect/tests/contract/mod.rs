//! The contract every `PlayerWatcher` implementation must satisfy.
//!
//! Written against the first implementation and run unchanged against every
//! later one. Nothing here may name a platform: a check that cannot be phrased
//! without one is a check on an adapter, not on the contract, and belongs in
//! that adapter's own tests.
//!
//! Each check states a clause in its assertion message, and each clause has a
//! test elsewhere in this directory that deliberately breaks it. A check no
//! implementation can fail is a description, not a check.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use benshi_core::clock::Timestamp;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayerId};
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo};

/// What a suite run needs from the caller.
pub struct Subject<W: PlayerWatcher> {
    /// The watcher under test.
    pub watcher: W,
    /// How long the implementation promises a single source may take.
    pub deadline: Duration,
}

/// Run every check against one implementation.
///
/// Panics on the first failure, naming the clause that was broken.
pub async fn verify<W: PlayerWatcher>(mut subject: Subject<W>) {
    let first = list(&mut subject.watcher).await;
    let second = list(&mut subject.watcher).await;

    listing_is_repeatable(&first, &second);
    identities_are_unique(&first);
    capabilities_are_stable(&first, &second);
    an_identity_keeps_its_application(&first, &second);

    let earlier = poll_within_deadline(&mut subject).await;
    let later = poll_within_deadline(&mut subject).await;

    for outcome in [&earlier, &later] {
        snapshots_come_only_from_listed_sources(&first, outcome);
        a_declared_path_arrives_as_a_path(&first, outcome);
        an_absent_position_is_absent_not_zero(&first, outcome);
        a_duration_is_never_shorter_than_its_position(outcome);
    }

    timestamps_never_go_backwards(&earlier, &later);
}

/// Take a listing, failing the run if the platform itself is unusable.
async fn list<W: PlayerWatcher>(watcher: &mut W) -> Vec<SourceInfo> {
    match watcher.sources().await {
        Ok(sources) => sources,
        Err(error) => panic!("contract: sources() must succeed on a usable platform: {error}"),
    }
}

/// Take a reading and check the clause that a poll respects its deadline.
///
/// Measured against real elapsed time, because a deadline is a promise about
/// real time. Twice the deadline is the allowance: it leaves room for a slow
/// machine while still failing an implementation that polls its sources one
/// after another instead of together.
async fn poll_within_deadline<W: PlayerWatcher>(subject: &mut Subject<W>) -> PollOutcome {
    let started = Instant::now();
    let outcome = match subject.watcher.poll().await {
        Ok(outcome) => outcome,
        Err(error) => panic!("contract: poll() must succeed on a usable platform: {error}"),
    };
    let elapsed = started.elapsed();

    assert!(
        elapsed < subject.deadline * 2,
        "contract: a poll respects its deadline. \
         The deadline is {:?} and the poll took {elapsed:?}. \
         A source that never answers must cost its own deadline and no more, \
         which means sources are polled together rather than in turn.",
        subject.deadline
    );

    outcome
}

/// The identities a listing contains.
fn identities(sources: &[SourceInfo]) -> BTreeSet<&PlayerId> {
    sources.iter().map(|source| &source.player).collect()
}

/// What each source in a listing declared it can report, keyed by identity.
///
/// Several checks compare a reading against the declaration of the source that
/// produced it, and each one starts from this.
fn declarations(sources: &[SourceInfo]) -> BTreeMap<&PlayerId, Capabilities> {
    sources
        .iter()
        .map(|source| (&source.player, source.capabilities))
        .collect()
}

/// A listing repeated with nothing changed yields the same sources.
///
/// An implementation that consumes its listing, or rebuilds it from whatever
/// answered fastest, fails here.
fn listing_is_repeatable(first: &[SourceInfo], second: &[SourceInfo]) {
    assert_eq!(
        identities(first),
        identities(second),
        "contract: a listing is repeatable. Two calls with nothing changed \
         returned different sources."
    );
}

/// No identity appears twice in one listing.
///
/// Two windows of one player are two sources and must be told apart, so the
/// identity carries whatever the platform uses to distinguish them.
fn identities_are_unique(sources: &[SourceInfo]) {
    let mut seen = BTreeSet::new();

    for source in sources {
        assert!(
            seen.insert(&source.player),
            "contract: identities are unique within a listing. {:?} appeared twice.",
            source.player
        );
    }
}

/// A source's capabilities do not change between listings.
///
/// Capabilities describe what a source *can* report, never what it *did* report
/// this round. An implementation that infers `position: false` from a missing
/// position fails here, and that inference is the one this rule exists to stop.
fn capabilities_are_stable(first: &[SourceInfo], second: &[SourceInfo]) {
    let before = declarations(first);

    for source in second {
        let Some(earlier) = before.get(&source.player) else {
            continue;
        };

        assert_eq!(
            *earlier, source.capabilities,
            "contract: capabilities are stable per source. {:?} declared \
             different capabilities in two consecutive listings.",
            source.player
        );
    }
}

/// An identity belongs to one application, and that application has a name.
///
/// The two are different questions: the identity distinguishes two windows of
/// one player, the application is what policy is keyed on. Which rule relates
/// them is platform knowledge, so the contract checks only that the adapter
/// supplies both and keeps them consistent.
fn an_identity_keeps_its_application(first: &[SourceInfo], second: &[SourceInfo]) {
    for source in first.iter().chain(second) {
        assert!(
            !source.app.0.is_empty(),
            "contract: every source names its application. {:?} named none.",
            source.player
        );
    }

    let before: BTreeMap<&PlayerId, &AppName> = first
        .iter()
        .map(|source| (&source.player, &source.app))
        .collect();

    for source in second {
        let Some(earlier) = before.get(&source.player) else {
            continue;
        };

        assert_eq!(
            *earlier, &source.app,
            "contract: an identity keeps its application. {:?} changed \
             application between two consecutive listings.",
            source.player
        );
    }
}

/// Every snapshot carries an identity the listing returned.
fn snapshots_come_only_from_listed_sources(sources: &[SourceInfo], outcome: &PollOutcome) {
    let listed = identities(sources);

    for snapshot in &outcome.snapshots {
        assert!(
            listed.contains(&snapshot.player),
            "contract: snapshots come only from listed sources. {:?} produced a \
             reading but was not in the listing.",
            snapshot.player
        );
    }
}

/// A source that declares it can report a path reports one.
///
/// Declaring the capability and then emitting a window title makes the
/// declaration worthless, and a consumer has no way to notice.
fn a_declared_path_arrives_as_a_path(sources: &[SourceInfo], outcome: &PollOutcome) {
    let declared = declarations(sources);

    for snapshot in &outcome.snapshots {
        let Some(capabilities) = declared.get(&snapshot.player) else {
            continue;
        };
        if !capabilities.file_path {
            continue;
        }

        match &snapshot.media {
            MediaRef::LocalFile(path) => assert!(
                !path.as_bytes().is_empty(),
                "contract: a reported path is not empty. {:?} reported a path \
                 of no bytes, which names no file.",
                snapshot.player
            ),
            MediaRef::Title(title) => panic!(
                "contract: a declared path arrives as a path. {:?} declared it \
                 can report a path and reported the title {title:?}.",
                snapshot.player
            ),
        }
    }
}

/// A source that cannot report a position says so rather than reporting zero.
///
/// Zero is a legal position, so publishing it as a stand-in for "unknown" makes
/// every consumer special-case a sentinel that is indistinguishable from the
/// start of a file.
fn an_absent_position_is_absent_not_zero(sources: &[SourceInfo], outcome: &PollOutcome) {
    let declared = declarations(sources);

    for snapshot in &outcome.snapshots {
        let Some(capabilities) = declared.get(&snapshot.player) else {
            continue;
        };
        if capabilities.position {
            continue;
        }

        assert_eq!(
            snapshot.position,
            Known::Unsupported,
            "contract: an absent position is absent, not zero. {:?} cannot \
             report a position and reported {:?}.",
            snapshot.player,
            snapshot.position
        );
    }
}

/// A reported duration is never shorter than the position it accompanies.
///
/// Players lie about length. A duration below the position it arrives with is a
/// lie the adapter has to discard before emitting, because nothing downstream
/// can tell it from a genuine short file.
fn a_duration_is_never_shorter_than_its_position(outcome: &PollOutcome) {
    for snapshot in &outcome.snapshots {
        let (Known::Value(position), Known::Value(duration)) =
            (snapshot.position, snapshot.duration)
        else {
            continue;
        };

        assert!(
            duration >= position,
            "contract: a duration is never shorter than its position. {:?} \
             reported a position of {position:?} inside a duration of \
             {duration:?}.",
            snapshot.player
        );
    }
}

/// A source's reading time never moves backwards between polls.
fn timestamps_never_go_backwards(earlier: &PollOutcome, later: &PollOutcome) {
    let before: BTreeMap<&PlayerId, Timestamp> = earlier
        .snapshots
        .iter()
        .map(|snapshot| (&snapshot.player, snapshot.observed_at))
        .collect();

    for snapshot in &later.snapshots {
        let Some(previous) = before.get(&snapshot.player) else {
            continue;
        };

        assert!(
            snapshot.observed_at >= *previous,
            "contract: timestamps never go backwards. {:?} reported {:?} after \
             {previous:?}.",
            snapshot.player,
            snapshot.observed_at
        );
    }
}
