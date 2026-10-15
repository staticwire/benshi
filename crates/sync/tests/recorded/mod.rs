//! Answers recorded from AniList, and the transports that write and replay
//! them.
//!
//! A fixture is a JSON array of exchanges, each a request and the answer it
//! had. No token is in one: [`Http`] sends it in a header, and neither a
//! request nor an answer carries it.

// Each test binary that includes this uses one half of it: the recorder the
// recording transport, and a test the replay.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use benshi_sync::Error;
use benshi_sync::anilist::transport::{Answer, Http, Request, Transport};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One request and the answer it had.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exchange {
    pub query: String,
    pub variables: Value,
    pub status: u16,
    /// `Retry-After`, in whole seconds.
    pub retry_after: Option<u64>,
    /// The body, read as JSON where it is, and kept as text where it is not.
    pub body: Value,
}

/// Where a fixture of this name is kept.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/anilist")
        .join(name)
}

/// A service that answers what was recorded, in order, and fails the test
/// where a request is not the one that was recorded next.
pub struct Replay {
    exchanges: Mutex<Vec<Exchange>>,
}

impl Replay {
    /// The exchanges of the fixture of this name.
    pub fn of(name: &str) -> Self {
        let text = std::fs::read_to_string(fixture(name)).expect("the fixture is there");
        let mut exchanges: Vec<Exchange> =
            serde_json::from_str(&text).expect("the fixture is JSON");
        exchanges.reverse();
        Self {
            exchanges: Mutex::new(exchanges),
        }
    }
}

// A replay answers from memory, and the trait asks for a future either way.
// The `async` therefore stays where there is nothing to await, rather than
// being spelled out as the future it desugars to.
#[allow(clippy::unused_async_trait_impl)]
impl Transport for Replay {
    async fn send(&self, request: &Request) -> Result<Answer, Error> {
        let exchange = self
            .exchanges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .expect("the fixture has an answer for every request");
        assert_eq!(
            (request.query.as_str(), &request.variables),
            (exchange.query.as_str(), &exchange.variables),
            "the request is the one that was recorded"
        );

        Ok(Answer {
            status: exchange.status,
            retry_after: exchange.retry_after.map(Duration::from_secs),
            body: match exchange.body {
                Value::String(text) => text,
                json => json.to_string(),
            },
        })
    }
}

/// A replay that ends with exchanges nobody asked for fails the test: the
/// backend stopped before it had asked what it asked when it was recorded.
/// Not while the test is failing already, where a second panic would abort
/// the process.
impl Drop for Replay {
    fn drop(&mut self) {
        let left = self
            .exchanges
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        if !std::thread::panicking() {
            assert_eq!(left, 0, "{left} recorded exchanges were never asked for");
        }
    }
}

/// The service over HTTPS, with every exchange kept.
pub struct Recording<'a> {
    pub http: &'a Http,
    pub kept: Arc<Mutex<Vec<Exchange>>>,
}

impl Transport for Recording<'_> {
    async fn send(&self, request: &Request) -> Result<Answer, Error> {
        let answer = self.http.send(request).await?;
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Exchange {
                query: request.query.clone(),
                variables: request.variables.clone(),
                status: answer.status,
                retry_after: answer.retry_after.map(|wait| wait.as_secs()),
                body: serde_json::from_str(&answer.body)
                    .unwrap_or_else(|_| Value::String(answer.body.clone())),
            });

        Ok(answer)
    }
}
