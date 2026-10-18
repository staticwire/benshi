//! List backends: the trait, a list kept in memory and AniList's, and the
//! rules that send an episode of one work to another.
//!
//! A backend reads an account's list and writes where the account stands on
//! one work, named by the number the service gives it. Recognition yields a
//! show and an episode in the release's own numbering, which is all a filename
//! honestly carries. **The rule of this crate is that the work an episode is
//! written to is resolved per account, against that account's own list**,
//! because services model the same work differently and no identifier mapping
//! can bridge a one-to-many split. Silently writing progress to the wrong
//! title is the worst failure a tracker has, and guessing is how you get
//! there.
//!
//! A new backend is finished when it passes the shared contract suite, in
//! `tests/contract`, not when it seems to work.

pub mod anilist;
pub mod memory;
pub mod plan;
pub mod relations;
pub mod resolve;

use std::time::Duration;

use benshi_core::BoxError;
use benshi_core::list::{Entry, EntryId, Write};

/// Why a call to a list was not answered.
///
/// [`Error::Timeout`], [`Error::Unreachable`] and [`Error::Wait`] are
/// transient: the same call can be answered later. [`Error::Refused`] and
/// [`Error::Unreadable`] are permanent for the backend. [`Error::NoWork`]
/// says nothing of the service: it is about the one write that named the
/// work.
///
/// **A message here is a reason and never a subject**, as with the
/// detection crate's errors: the name of the backend is left to whoever
/// reports one.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The service accepted the call and did not answer within its deadline.
    ///
    /// The deadline is written with `{:?}`, which for a `Duration` is the
    /// only form there is and happens to be the readable one: `15s`.
    #[error("did not answer within {deadline:?}")]
    Timeout {
        /// The deadline it exceeded.
        deadline: Duration,
    },
    /// The service could not be reached, or failed on its side.
    #[error("could not be reached: {0}")]
    Unreachable(#[source] BoxError),
    /// The service asked to be asked again after this long.
    #[error("asked to wait {after:?}")]
    Wait {
        /// How long it asked for.
        after: Duration,
    },
    /// The service refused the call, as it refuses a token it does not take.
    #[error("refused the call: {0}")]
    Refused(String),
    /// The service has no work under the number a write named.
    #[error("has no work numbered {}", .id.0)]
    NoWork {
        /// The number the write named.
        id: EntryId,
    },
    /// The service answered with what cannot be read as an answer.
    #[error("answered with what cannot be read: {0}")]
    Unreadable(#[source] BoxError),
}

/// A remote list a local record can be synchronized to.
///
/// Every call carries the deadline of the backend that makes it, and a
/// service that never answers is an [`Error::Timeout`] once it has passed.
// Desugared rather than written `async fn`, because an `async fn` in a trait
// leaves the auto traits of the future it returns undeclared, and a future that
// is not `Send` cannot be spawned on a runtime of several threads. An
// implementation still writes `async fn` in its `impl`, and the compiler checks
// that the future it returns satisfies the bound.
pub trait ListBackend {
    /// Name of this backend, as shown to the user.
    fn name(&self) -> &str;

    /// Every entry of the account's list.
    ///
    /// # Errors
    ///
    /// An [`Error`] of the class the failure of the service is.
    fn list(&self) -> impl Future<Output = Result<Vec<Entry>, Error>> + Send;

    /// The account's entry for one work, and none where the work is not on
    /// its list.
    ///
    /// # Errors
    ///
    /// An [`Error`] of the class the failure of the service is.
    fn entry(&self, id: EntryId) -> impl Future<Output = Result<Option<Entry>, Error>> + Send;

    /// Write where the account stands on one work, adding the work to the
    /// list where it is not on it.
    ///
    /// # Errors
    ///
    /// [`Error::NoWork`] where the service has no work under the number, and
    /// otherwise an [`Error`] of the class the failure of the service is.
    fn write(&self, write: &Write) -> impl Future<Output = Result<(), Error>> + Send;
}

/// The answer of a call, or [`Error::Timeout`] where it has not come within
/// the deadline.
pub(crate) async fn within<T>(
    deadline: Duration,
    call: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::time::timeout(deadline, call)
        .await
        .unwrap_or(Err(Error::Timeout { deadline }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use benshi_core::list::EntryId;

    use super::Error;

    #[test]
    fn a_failure_reads_as_a_reason_and_leaves_the_subject_to_its_reporter() {
        let reasons = [
            Error::Timeout {
                deadline: Duration::from_secs(15),
            },
            Error::Unreachable("connection refused".into()),
            Error::Wait {
                after: Duration::from_secs(30),
            },
            Error::Refused("invalid token".to_owned()),
            Error::NoWork { id: EntryId(9) },
            Error::Unreadable("expected value at line 1 column 1".into()),
        ];

        for failure in reasons {
            // Exhaustive and empty on purpose: a variant added later stops
            // the crate compiling here, in front of the list it has to join.
            match &failure {
                Error::Timeout { .. }
                | Error::Unreachable(_)
                | Error::Wait { .. }
                | Error::Refused(_)
                | Error::NoWork { .. }
                | Error::Unreadable(_) => {}
            }

            let said = failure.to_string();
            for subject in ["memory", "anilist", "AniList", "backend", "service"] {
                assert!(
                    !said.contains(subject),
                    "a reason names its subject: {said}"
                );
            }
            assert!(
                !said.contains("EntryId"),
                "a Rust type name reached a message: {said}"
            );
        }
    }

    #[test]
    fn every_failure_says_what_it_carries() {
        assert_eq!(
            Error::Timeout {
                deadline: Duration::from_secs(15)
            }
            .to_string(),
            "did not answer within 15s"
        );
        assert_eq!(
            Error::Unreachable("connection refused".into()).to_string(),
            "could not be reached: connection refused"
        );
        assert_eq!(
            Error::Wait {
                after: Duration::from_secs(30)
            }
            .to_string(),
            "asked to wait 30s"
        );
        assert_eq!(
            Error::Refused("invalid token".to_owned()).to_string(),
            "refused the call: invalid token"
        );
        assert_eq!(
            Error::NoWork { id: EntryId(9) }.to_string(),
            "has no work numbered 9"
        );
        assert_eq!(
            Error::Unreadable("expected value at line 1 column 1".into()).to_string(),
            "answered with what cannot be read: expected value at line 1 column 1"
        );
    }
}
