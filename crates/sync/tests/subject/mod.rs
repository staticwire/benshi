//! The AniList backend as the subject of the contract.
//!
//! The works the suite writes to on the test account, and a service that
//! answers as AniList or a recording of it does, or fails in one of the ways
//! the suite tries. The failures are made here, since a recording holds what
//! the service answered once.

use benshi_core::list::EntryId;
use benshi_sync::Error;
use benshi_sync::anilist::transport::{Answer, Request, Transport};
use benshi_sync::anilist::{AniList, DEADLINE};
use serde_json::json;

use crate::contract::{Failure, Subject};

/// The series of 25 episodes filed on the test account's list before the
/// suite runs: set aside after the third, scored, and begun in 2015.
pub const LISTED: EntryId = EntryId(16_498);

/// The film the suite adds to the test account's list.
pub const UNLISTED: EntryId = EntryId(112_151);

/// The number the service has no work under.
pub const MISSING: EntryId = EntryId(999_999_999);

/// The service, answering as `T` does or failing in one way.
pub enum Service<T> {
    /// It answers as `T` does.
    Answering(T),
    /// It fails this way.
    Failing(Failure),
}

impl<T: Transport + Sync> Transport for Service<T> {
    async fn send(&self, request: &Request) -> Result<Answer, Error> {
        match self {
            Self::Answering(service) => service.send(request).await,
            Self::Failing(failure) => failed(*failure).await,
        }
    }
}

/// What the service answers when it fails this way: too many requests and a
/// token that is not one as AniList answers them, and a page that is not
/// JSON.
async fn failed(failure: Failure) -> Result<Answer, Error> {
    let answer = |status, retry_after, body: String| {
        Ok(Answer {
            status,
            retry_after,
            body,
        })
    };
    match failure {
        Failure::Silent => std::future::pending().await,
        Failure::Unreachable => Err(Error::Unreachable("connection refused".into())),
        Failure::Wait(after) => answer(
            429,
            Some(after),
            json!({ "data": null, "errors": [{ "message": "Too Many Requests.", "status": 429 }] })
                .to_string(),
        ),
        Failure::Refused => answer(
            400,
            None,
            json!({ "data": null, "errors": [{ "message": "Invalid token", "status": 400 }] })
                .to_string(),
        ),
        Failure::Unreadable => answer(200, None, "<html></html>".to_owned()),
    }
}

/// The backend over `service` as the subject of the contract, and over a
/// service that fails in each way the suite tries.
pub fn subject<T: Transport + Sync + 'static>(service: T) -> Subject<AniList<Service<T>>> {
    Subject {
        backend: AniList::new(Service::Answering(service)),
        listed: LISTED,
        unlisted: UNLISTED,
        missing: MISSING,
        deadline: DEADLINE,
        failing: Box::new(|failure| AniList::new(Service::Failing(failure))),
    }
}
