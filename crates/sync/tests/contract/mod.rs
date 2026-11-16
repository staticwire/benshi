//! The contract every `ListBackend` must satisfy.
//!
//! Written against the backend in memory and run unchanged against every
//! later one. Nothing here may name a service: a check that cannot be phrased
//! without one is a check on that backend, and belongs in its own tests.
//!
//! Each check states a clause in its assertion message, and each clause has a
//! test in `contract_rejects.rs` that breaks it on purpose. A check no backend
//! can fail is a description, not a check. What a subject must hold before
//! the suite can run is said of the subject, and is not a clause.
//!
//! The backend in memory runs the suite on a paused clock, from
//! `#[tokio::test(start_paused = true)]`, so a deadline of fifteen seconds
//! passes the moment nothing else is left to run, and costs no real time.
//!
//! [`verify_answers`] runs the checks that ask the service and no other,
//! which is what a recording of a service's answers is made from. The
//! failures are made by the subject, and on a clock that runs they cannot be
//! checked: a call to a service that never answers ends a moment past its
//! deadline, and the check of its deadline is exact.

// The recorder of the AniList fixtures runs the checks that ask the service
// alone, and leaves the rest of this unused.
#![allow(dead_code)]

use std::time::Duration;

use benshi_core::list::{Dated, Day, Entry, EntryId, Status, Write};
use benshi_sync::{Error, ListBackend};
use tokio::time::{Instant, timeout};

/// The day the writes to the listed work say it was begun.
const BEGUN: Day = Day::new(2026, 9, 2);

/// The day the first writes to the listed work say it was completed.
const DONE: Day = Day::new(2026, 9, 30);

/// The day the write that adds a work says it was begun.
const ADDED: Day = Day::new(2026, 11, 16);

/// How the service behind a backend fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// It accepts every call and never answers.
    Silent,
    /// It cannot be reached.
    Unreachable,
    /// It asks to be asked again after this long.
    Wait(Duration),
    /// It refuses every call for good.
    Refused,
    /// It answers with what cannot be read.
    Unreadable,
}

/// Every way a service fails, in the order the suite tries them.
pub const EVERY_FAILURE: [Failure; 5] = [
    Failure::Silent,
    Failure::Unreachable,
    Failure::Wait(Duration::from_secs(30)),
    Failure::Refused,
    Failure::Unreadable,
];

/// What a run needs from the caller.
pub struct Subject<B> {
    /// A backend over a service that answers.
    pub backend: B,
    /// A work on the list, with a score, not being watched, and with an
    /// episode past the one after its progress or no count at all, so that
    /// a write of one more episode is not its last and the service takes no
    /// step of its own.
    pub listed: EntryId,
    /// A work the service has and the list does not hold.
    pub unlisted: EntryId,
    /// A number the service has no work under.
    pub missing: EntryId,
    /// How long the backend promises one call may take.
    pub deadline: Duration,
    /// The same backend over a service that fails in the given way.
    pub failing: Box<dyn Fn(Failure) -> B>,
}

/// Run every check against one backend.
///
/// Panics on the first failure, naming the clause that was broken.
pub async fn verify<B: ListBackend>(subject: Subject<B>) {
    let write = verify_answers(&subject).await;

    for failure in EVERY_FAILURE {
        each_failure_is_answered_as_its_class(&subject, failure, &write).await;
    }
}

/// Run every check that asks the service behind the backend, and answer with
/// the first write it made, which the checks of the failures send again.
///
/// Panics on the first failure, naming the clause that was broken.
pub async fn verify_answers<B: ListBackend>(subject: &Subject<B>) -> Write {
    let backend = &subject.backend;
    let deadline = subject.deadline;

    let before = answered(bounded(deadline, backend.entry(subject.listed)).await)
        .expect("subject: the listed work is on its list")
        .entry;
    assert!(
        before.score.is_some(),
        "subject: the listed work has a score"
    );
    assert_ne!(
        before.status,
        Status::Watching,
        "subject: the listed work is not being watched"
    );
    assert!(
        before
            .episodes
            .is_none_or(|episodes| before.progress + 1 < episodes),
        "subject: the listed work has an episode past the one after its progress"
    );

    a_work_off_the_list_is_answered_as_none(subject).await;

    let write = Write {
        id: subject.listed,
        status: Status::Watching,
        progress: before.progress + 1,
        rewatches: before.rewatches + 1,
        started: Some(BEGUN),
        completed: Some(DONE),
    };
    answered(bounded(deadline, backend.write(&write)).await);
    let once = answered(bounded(deadline, backend.entry(subject.listed)).await)
        .expect("contract: a write is read back as it was written");
    a_write_is_read_back(&write, &once);
    assert_eq!(
        once.entry.score, before.score,
        "contract: a write leaves the score as it was"
    );
    assert_eq!(
        once.entry,
        Entry {
            status: write.status,
            progress: write.progress,
            rewatches: write.rewatches,
            ..before
        },
        "contract: a write changes nothing else of the entry"
    );

    answered(bounded(deadline, backend.write(&write)).await);
    let twice = answered(bounded(deadline, backend.entry(subject.listed)).await);
    assert_eq!(
        twice.as_ref(),
        Some(&once),
        "contract: a write made twice leaves what one left"
    );

    let undated = Write {
        completed: None,
        ..write
    };
    answered(bounded(deadline, backend.write(&undated)).await);
    let cleared = answered(bounded(deadline, backend.entry(subject.listed)).await)
        .expect("contract: a write is read back as it was written");
    a_write_is_read_back(&undated, &cleared);

    let adding = Write {
        id: subject.unlisted,
        status: Status::Watching,
        progress: 1,
        rewatches: 0,
        started: Some(ADDED),
        completed: None,
    };
    answered(bounded(deadline, backend.write(&adding)).await);
    let added = answered(bounded(deadline, backend.entry(subject.unlisted)).await)
        .expect("contract: a write adds a work that is off the list");
    a_write_is_read_back(&adding, &added);
    assert_eq!(
        added.entry.score, None,
        "contract: a work a write adds has no score"
    );

    let list = answered(bounded(deadline, backend.list()).await);
    for written in [&cleared.entry, &added.entry] {
        assert!(
            list.contains(written),
            "contract: the list holds what was written, and {written:?} is not in {list:?}"
        );
    }
    let mut works: Vec<EntryId> = list.iter().map(|entry| entry.id).collect();
    works.sort_unstable();
    works.dedup();
    assert_eq!(
        works.len(),
        list.len(),
        "contract: a work is on the list once"
    );

    a_write_to_a_work_the_service_does_not_have_is_refused(subject, &adding).await;

    write
}

/// A work the service has and the list does not hold has no entry.
async fn a_work_off_the_list_is_answered_as_none<B: ListBackend>(subject: &Subject<B>) {
    let answer = bounded(subject.deadline, subject.backend.entry(subject.unlisted)).await;

    assert_eq!(
        answered(answer),
        None,
        "contract: a work off the list is answered as none"
    );
    let list = answered(bounded(subject.deadline, subject.backend.list()).await);
    assert!(
        list.iter().all(|entry| entry.id != subject.unlisted),
        "contract: a work off the list is not listed"
    );
}

/// A write to a number the service has no work under is refused, and says
/// which number.
async fn a_write_to_a_work_the_service_does_not_have_is_refused<B: ListBackend>(
    subject: &Subject<B>,
    write: &Write,
) {
    let nowhere = Write {
        id: subject.missing,
        ..*write
    };

    let answer = bounded(subject.deadline, subject.backend.write(&nowhere)).await;

    assert!(
        matches!(answer, Err(Error::NoWork { id }) if id == subject.missing),
        "contract: a write to a work the service does not have is answered as no work, \
         and got {answer:?}"
    );
}

/// What an entry holds of the account's standing, both days included, is
/// what the write said.
fn a_write_is_read_back(write: &Write, read: &Dated) {
    let entry = &read.entry;
    assert_eq!(
        (
            entry.id,
            entry.status,
            entry.progress,
            entry.rewatches,
            read.started,
            read.completed
        ),
        (
            write.id,
            write.status,
            write.progress,
            write.rewatches,
            write.started,
            write.completed
        ),
        "contract: a write is read back as it was written"
    );
}

/// Every call to a backend over a failing service is answered as the class
/// of that failure.
async fn each_failure_is_answered_as_its_class<B: ListBackend>(
    subject: &Subject<B>,
    failure: Failure,
    write: &Write,
) {
    let deadline = subject.deadline;
    // Each call gets a backend of its own: a backend asked to wait holds its
    // next request back as long as it was asked, which no deadline bounds.
    let (list, entry, writing) = (
        (subject.failing)(failure),
        (subject.failing)(failure),
        (subject.failing)(failure),
    );

    let answers = [
        bounded(deadline, list.list()).await.err(),
        bounded(deadline, entry.entry(subject.listed)).await.err(),
        bounded(deadline, writing.write(write)).await.err(),
    ];

    for answer in answers {
        let of_its_class = match (failure, &answer) {
            (Failure::Silent, Some(Error::Timeout { deadline: missed })) => *missed == deadline,
            (Failure::Wait(after), Some(Error::Wait { after: told })) => *told == after,
            (Failure::Unreachable, Some(Error::Unreachable(_)))
            | (Failure::Refused, Some(Error::Refused(_)))
            | (Failure::Unreadable, Some(Error::Unreadable(_))) => true,
            _ => false,
        };
        assert!(
            of_its_class,
            "contract: each failure is answered as its class, and {failure:?} got {answer:?}"
        );
    }
}

/// The answer of a call to a service that answers.
fn answered<T>(answer: Result<T, Error>) -> T {
    answer.unwrap_or_else(|error| {
        panic!("contract: a call to a service that answers succeeds, and one failed: {error}")
    })
}

/// Make one call, and fail the run where it does not end within the
/// deadline.
///
/// A call that never ends is cut off at twice the deadline, so that a
/// backend with no deadline fails the run rather than hangs it.
async fn bounded<T>(deadline: Duration, call: impl Future<Output = T>) -> T {
    let started = Instant::now();
    let Ok(answer) = timeout(deadline * 2, call).await else {
        panic!(
            "contract: every call ends within its deadline, and one ran past twice {deadline:?}"
        );
    };
    let took = started.elapsed();
    assert!(
        took <= deadline,
        "contract: every call ends within its deadline, and one took {took:?} of {deadline:?}"
    );

    answer
}
