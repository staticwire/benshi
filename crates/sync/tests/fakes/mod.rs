//! Building blocks shared by the backends that exercise the contract.
//!
//! Entries, and the backend in memory they are listed in. The two test
//! binaries that use these wrap it differently: one runs it as it is, and the
//! other breaks one clause at a time.

use std::time::Duration;

use benshi_core::list::{Entry, EntryId, Format, Rating, Status};
use benshi_sync::memory::{Fault, Memory};

use crate::contract::{Failure, Subject};

/// The deadline the backends here promise, and which the suite measures
/// against.
pub const DEADLINE: Duration = Duration::from_secs(15);

/// A work on the list.
pub const LISTED: EntryId = EntryId(21);

/// A work the service has and the list does not hold.
pub const UNLISTED: EntryId = EntryId(437);

/// A number the service has no work under.
pub const MISSING: EntryId = EntryId(9);

/// A series of twelve episodes, set aside after the third, and scored.
pub fn a_series(id: EntryId) -> Entry {
    Entry {
        id,
        title: "Show Title".to_owned(),
        spellings: vec!["Show Title Spelt Otherwise".to_owned()],
        format: Some(Format::Tv),
        episodes: Some(12),
        episode_length: Some(Duration::from_mins(24)),
        year: Some(2024),
        released: true,
        status: Status::OnHold,
        progress: 3,
        rewatches: 0,
        score: Rating::new(80),
    }
}

/// A film, as the service has it before it is on the list.
///
/// It carries a score, which a backend in memory does not read: a work a
/// write adds has none, and a backend that kept this one would show it.
pub fn a_film(id: EntryId) -> Entry {
    Entry {
        id,
        title: "A Film".to_owned(),
        spellings: Vec::new(),
        format: Some(Format::Film),
        episodes: Some(1),
        episode_length: Some(Duration::from_mins(95)),
        year: Some(1988),
        released: true,
        status: Status::Planned,
        progress: 0,
        rewatches: 0,
        score: Rating::new(50),
    }
}

/// A backend in memory whose list holds the series, and whose service also
/// has the film.
pub fn memory() -> Memory {
    Memory::new("memory", DEADLINE, [a_series(LISTED)], [a_film(UNLISTED)])
}

/// The fault a backend in memory stands in for a failure with.
pub fn fault(failure: Failure) -> Fault {
    match failure {
        Failure::Silent => Fault::Silent,
        Failure::Unreachable => Fault::Unreachable,
        Failure::Wait(after) => Fault::Wait(after),
        Failure::Refused => Fault::Refused,
        Failure::Unreadable => Fault::Unreadable,
    }
}

/// A subject over a backend in memory, made into the backend under test by
/// `wrap`, which is told the failure the backend stands in for, if any.
pub fn subject<B>(wrap: impl Fn(Memory, Option<Failure>) -> B + 'static) -> Subject<B> {
    Subject {
        backend: wrap(memory(), None),
        listed: LISTED,
        unlisted: UNLISTED,
        missing: MISSING,
        deadline: DEADLINE,
        failing: Box::new(move |failure| wrap(memory().failing(fault(failure)), Some(failure))),
    }
}
