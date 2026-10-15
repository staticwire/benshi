//! Where a token is kept: the secret store of the system.
//!
//! [`Secrets`] is three calls, read, write and remove, each for the token of
//! one backend, named by a [`Key`]. `SecretService`, on Linux, keeps tokens in
//! the store that answers the Secret Service API on the session bus, which is
//! `ksecretd` under KDE and `gnome-keyring` under GNOME. [`Memory`] keeps them
//! in this process, for tests.
//!
//! **A call asks the store to unlock only where it may ask a person.**
//! Unlocking takes a password, which the store asks for in a window of its
//! own: [`Asking`] says whether a call may open one, and how long it waits for
//! an answer. The daemon never asks on its own. A call made for a command a
//! person runs may ask.

use std::collections::HashMap;
use std::future::{self, Future};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

#[cfg(target_os = "linux")]
mod service;

#[cfg(target_os = "linux")]
pub use service::SecretService;

/// How long one call to the store may take before it is given up on.
pub const DEADLINE: Duration = Duration::from_secs(5);

/// How long a person is given to answer the store's window.
pub const PROMPT_WAIT: Duration = Duration::from_mins(2);

/// What a token is kept under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The backend the token is for, which the store finds it by.
    pub backend: &'static str,
    /// What the store shows a person the token as.
    pub label: &'static str,
}

/// The token of AniList.
pub const ANILIST: Key = Key {
    backend: "anilist",
    label: "AniList token for benshi",
};

/// Whether a call may ask a person to unlock the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asking {
    /// Never: a store that would have to ask is [`Error::Locked`].
    Never,
    /// Yes, and this long is waited for the answer.
    Within(Duration),
}

/// A failure is boxed where its type belongs to the transport.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why a call to the store did not do what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No store answers, or there is no session bus to find one on.
    #[error("secret store could not be reached: {0}")]
    Unreachable(#[source] BoxError),
    /// The store, or the person it asked, did not answer in time.
    #[error("secret store did not answer within {deadline:?}")]
    Timeout {
        /// How long was waited.
        deadline: Duration,
    },
    /// The store is locked, or wanted a person where none may be asked.
    #[error("secret store is locked")]
    Locked,
    /// The person the store asked dismissed its window.
    #[error("unlocking the secret store was dismissed")]
    Dismissed,
    /// The store answered with an error of its own, or has no default
    /// collection to write to.
    #[error("secret store refused the call: {0}")]
    Refused(#[source] BoxError),
    /// What the store holds under the key is not UTF-8.
    #[error("secret store holds a token that is not text")]
    NotText,
}

/// A place a token is kept, one for each [`Key`].
pub trait Secrets {
    /// The token kept under `key`, and nothing where none is.
    fn read(&self, key: Key) -> impl Future<Output = Result<Option<String>, Error>> + Send;

    /// Keeps `token` under `key`, in place of one kept there before.
    fn write(&self, key: Key, token: &str) -> impl Future<Output = Result<(), Error>> + Send;

    /// Removes the token kept under `key`. Where none is, nothing happens.
    fn remove(&self, key: Key) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Tokens kept in this process and gone with it, each under the backend of
/// its key, as the store keeps them.
#[derive(Debug, Default)]
pub struct Memory {
    kept: Mutex<HashMap<&'static str, String>>,
}

impl Memory {
    fn kept(&self) -> MutexGuard<'_, HashMap<&'static str, String>> {
        self.kept
            .lock()
            .expect("no call panics while it holds the tokens")
    }
}

impl Secrets for Memory {
    fn read(&self, key: Key) -> impl Future<Output = Result<Option<String>, Error>> + Send {
        future::ready(Ok(self.kept().get(key.backend).cloned()))
    }

    fn write(&self, key: Key, token: &str) -> impl Future<Output = Result<(), Error>> + Send {
        self.kept().insert(key.backend, token.to_owned());

        future::ready(Ok(()))
    }

    fn remove(&self, key: Key) -> impl Future<Output = Result<(), Error>> + Send {
        self.kept().remove(key.backend);

        future::ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ANILIST, DEADLINE, Error, PROMPT_WAIT};

    #[test]
    fn each_wait_is_pinned() {
        assert_eq!(DEADLINE, Duration::from_secs(5));
        assert_eq!(PROMPT_WAIT, Duration::from_mins(2));
    }

    #[test]
    fn the_key_of_anilist_is_pinned() {
        assert_eq!(ANILIST.backend, "anilist");
        assert_eq!(ANILIST.label, "AniList token for benshi");
    }

    #[test]
    fn each_line_is_said_as_decided() {
        let said = [
            Error::Unreachable("no bus".into()),
            Error::Timeout {
                deadline: Duration::from_secs(5),
            },
            Error::Locked,
            Error::Dismissed,
            Error::Refused("no default collection".into()),
            Error::NotText,
        ]
        .map(|error| error.to_string());

        assert_eq!(
            said,
            [
                "secret store could not be reached: no bus",
                "secret store did not answer within 5s",
                "secret store is locked",
                "unlocking the secret store was dismissed",
                "secret store refused the call: no default collection",
                "secret store holds a token that is not text",
            ]
        );
    }
}
