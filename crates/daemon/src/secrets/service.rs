//! The Secret Service API, spoken over the session bus.
//!
//! A token is an item of the default collection with two attributes,
//! `application` of `benshi` and `backend` of its key's backend, which is how
//! it is found again. It is sent in the clear, in a session opened with the
//! algorithm `plain`: every process of the user's session may ask the store for
//! an item that is unlocked, so an encrypted transfer on the same bus would
//! keep the token from nobody who could not ask for it.
//!
//! Every call is given up on after the deadline [`SecretService`] is made
//! with. A window a call opens to unlock the store is waited for as long as
//! its [`Asking`] says.

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zbus::export::futures_core::Stream;
use zbus::proxy::{CacheProperties, Defaults};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Type, Value};
use zbus::{MatchRule, MessageStream, message};

use super::{Asking, Error, Key, Secrets};

/// What the store is asked to show a token as, and holds it as.
const CONTENT_TYPE: &str = "text/plain; charset=utf8";

/// The attribute that names the program an item is for.
const APPLICATION: &str = "application";

/// The attribute that names the backend a token is for.
const BACKEND: &str = "backend";

/// What [`APPLICATION`] holds.
const BENSHI: &str = "benshi";

/// The algorithm of a session that passes a secret as it is.
const PLAIN: &str = "plain";

/// The alias of the collection a token is written to.
const DEFAULT: &str = "default";

/// The path [`DEFAULT`] reads as where no collection has the alias.
const NO_COLLECTION: &str = "/";

/// The path a call answers with in place of a prompt where it needs none.
const NO_PROMPT: &str = "/";

/// The window a prompt is shown over: none, since a daemon has none.
const NO_WINDOW: &str = "";

/// The property of an item that is its label.
const LABEL: &str = "org.freedesktop.Secret.Item.Label";

/// The property of an item that holds its attributes.
const ATTRIBUTES: &str = "org.freedesktop.Secret.Item.Attributes";

/// The interface of a prompt, and the signal it ends with.
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const COMPLETED: &str = "Completed";

/// The error of an object that is locked.
const IS_LOCKED: &str = "org.freedesktop.Secret.Error.IsLocked";

/// The error of the bus where no program has the name or can be started for
/// it.
const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";

/// What the errors of the bus begin with where it could not start a program
/// for the name.
const SPAWN: &str = "org.freedesktop.DBus.Error.Spawn.";

/// A secret as the Secret Service API passes it.
#[derive(Debug, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
struct Secret {
    /// The session it was passed in.
    session: OwnedObjectPath,
    /// What the algorithm of the session needs, which for `plain` is nothing.
    parameters: Vec<u8>,
    /// The token.
    value: Vec<u8>,
    /// [`CONTENT_TYPE`].
    content_type: String,
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Service",
    default_service = "org.freedesktop.secrets",
    default_path = "/org/freedesktop/secrets"
)]
trait Service {
    fn open_session(
        &self,
        algorithm: &str,
        input: &Value<'_>,
    ) -> zbus::Result<(OwnedValue, OwnedObjectPath)>;

    fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> zbus::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)>;

    fn unlock(
        &self,
        objects: &[OwnedObjectPath],
    ) -> zbus::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)>;

    fn read_alias(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Collection",
    default_service = "org.freedesktop.secrets"
)]
trait Collection {
    fn create_item(
        &self,
        properties: HashMap<&str, &Value<'_>>,
        secret: &Secret,
        replace: bool,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;

    #[zbus(property)]
    fn locked(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Item",
    default_service = "org.freedesktop.secrets"
)]
trait Item {
    fn get_secret(&self, session: &OwnedObjectPath) -> zbus::Result<Secret>;

    fn delete(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Prompt",
    default_service = "org.freedesktop.secrets"
)]
trait Prompt {
    fn prompt(&self, window_id: &str) -> zbus::Result<()>;

    fn dismiss(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Session",
    default_service = "org.freedesktop.secrets"
)]
trait Session {
    fn close(&self) -> zbus::Result<()>;
}

/// Tokens kept by the store that answers the Secret Service API.
#[derive(Debug)]
pub struct SecretService {
    connection: zbus::Connection,
    deadline: Duration,
    asking: Asking,
}

impl SecretService {
    /// The store on the session bus of this process.
    ///
    /// # Errors
    ///
    /// [`Error::Unreachable`] where there is no session bus, and
    /// [`Error::Timeout`] where it does not answer within `deadline`.
    pub async fn connect(deadline: Duration, asking: Asking) -> Result<Self, Error> {
        let connection = tokio::time::timeout(deadline, zbus::Connection::session())
            .await
            .map_err(|_elapsed| Error::Timeout { deadline })?
            .map_err(|unconnected| Error::Unreachable(Box::new(unconnected)))?;

        Ok(Self::over(connection, deadline, asking))
    }

    /// The store at the other end of `connection`.
    #[must_use]
    pub fn over(connection: zbus::Connection, deadline: Duration, asking: Asking) -> Self {
        Self {
            connection,
            deadline,
            asking,
        }
    }

    /// `call`, given up on at the deadline.
    async fn call<T>(&self, call: impl Future<Output = zbus::Result<T>>) -> Result<T, Error> {
        tokio::time::timeout(self.deadline, call)
            .await
            .map_err(|_elapsed| Error::Timeout {
                deadline: self.deadline,
            })?
            .map_err(failed)
    }

    /// A proxy of the service. Building one makes no call while it caches
    /// no property.
    async fn service(&self) -> Result<ServiceProxy<'_>, Error> {
        ServiceProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(failed)
    }

    /// A proxy of the object at `path`, built as [`Self::service`] is.
    async fn at<'p, P>(&self, path: &'p OwnedObjectPath) -> Result<P, Error>
    where
        P: Defaults + From<zbus::Proxy<'p>>,
    {
        zbus::proxy::Builder::<P>::new(&self.connection)
            .path(path.as_str())
            .map_err(failed)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(failed)
    }

    /// Runs `during` in a session of its own, which is closed after it.
    ///
    /// A session the store does not close, in time or at all, is left to it:
    /// what was read in the session has been read, and the store closes it
    /// when this connection goes.
    async fn in_session<T>(
        &self,
        during: impl AsyncFnOnce(&OwnedObjectPath) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let service = self.service().await?;
        let (_output, session) = self
            .call(service.open_session(PLAIN, &Value::from("")))
            .await?;
        let outcome = during(&session).await;
        if let Ok(closing) = self.at::<SessionProxy<'_>>(&session).await {
            drop(self.call(closing.close()).await);
        }

        outcome
    }

    /// Asks the store to unlock `objects`, and is [`Error::Locked`] with no
    /// call where no person may be asked, so that nothing the store does to
    /// unlock can open a window.
    async fn unlock(&self, objects: &[OwnedObjectPath]) -> Result<(), Error> {
        if self.asking == Asking::Never {
            return Err(Error::Locked);
        }
        let service = self.service().await?;
        let (_unlocked, prompt) = self.call(service.unlock(objects)).await?;

        self.prompted(&prompt).await
    }

    /// Waits for the person the store asks through `prompt`, where it asks
    /// at all and this store may.
    async fn prompted(&self, prompt: &OwnedObjectPath) -> Result<(), Error> {
        if prompt.as_str() == NO_PROMPT {
            return Ok(());
        }
        let asked = self.at::<PromptProxy<'_>>(prompt).await?;
        let Asking::Within(wait) = self.asking else {
            self.dismiss(&asked).await;
            return Err(Error::Locked);
        };

        // No sender is named, so any peer of the bus can send `Completed` on
        // this path. A forged one ends the wait, as `Dismissed` or as the
        // `IsLocked` of the next call.
        let completion = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .path(prompt.as_str())
            .and_then(|rule| rule.interface(PROMPT))
            .and_then(|rule| rule.member(COMPLETED))
            .map_err(failed)?
            .build();
        let mut completed = self
            .call(MessageStream::for_match_rule(
                completion,
                &self.connection,
                None,
            ))
            .await?;
        self.call(asked.prompt(NO_WINDOW)).await?;

        let answer = tokio::time::timeout(
            wait,
            std::future::poll_fn(|context| Pin::new(&mut completed).poll_next(context)),
        )
        .await;
        let Ok(answer) = answer else {
            self.dismiss(&asked).await;
            return Err(Error::Timeout { deadline: wait });
        };
        let message = answer
            .ok_or_else(|| Error::Unreachable("connection closed".into()))?
            .map_err(failed)?;
        let (dismissed, _result): (bool, OwnedValue) =
            message.body().deserialize().map_err(failed)?;
        if dismissed {
            return Err(Error::Dismissed);
        }

        Ok(())
    }

    /// Dismisses `asked`. The call that made the prompt fails either way, so
    /// what the store answers to the dismissal is not read.
    async fn dismiss(&self, asked: &PromptProxy<'_>) {
        drop(self.call(asked.dismiss()).await);
    }
}

/// The attributes the token of `key` is found by.
fn attributes(key: Key) -> HashMap<&'static str, &'static str> {
    HashMap::from([(APPLICATION, BENSHI), (BACKEND, key.backend)])
}

/// Which of [`Error::Locked`], [`Error::Unreachable`] and [`Error::Refused`]
/// a failed call is.
fn failed(error: zbus::Error) -> Error {
    match &error {
        zbus::Error::MethodError(name, _, _) if name.as_str() == IS_LOCKED => Error::Locked,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == SERVICE_UNKNOWN || name.starts_with(SPAWN) =>
        {
            Error::Unreachable(Box::new(error))
        }
        // The connection itself failed, which the store answered nothing to.
        zbus::Error::InputOutput(_) => Error::Unreachable(Box::new(error)),
        _ => Error::Refused(Box::new(error)),
    }
}

impl Secrets for SecretService {
    async fn read(&self, key: Key) -> Result<Option<String>, Error> {
        self.in_session(async |session| {
            let service = self.service().await?;
            let (unlocked, locked) = self.call(service.search_items(attributes(key))).await?;
            let item = match (unlocked.into_iter().next(), locked.into_iter().next()) {
                (Some(item), _) => item,
                (None, Some(item)) => {
                    self.unlock(std::slice::from_ref(&item)).await?;
                    item
                }
                (None, None) => return Ok(None),
            };
            let secret = self
                .call(self.at::<ItemProxy<'_>>(&item).await?.get_secret(session))
                .await?;

            String::from_utf8(secret.value)
                .map(Some)
                .map_err(|_not_text| Error::NotText)
        })
        .await
    }

    async fn write(&self, key: Key, token: &str) -> Result<(), Error> {
        self.in_session(async |session| {
            let service = self.service().await?;
            let collection = self.call(service.read_alias(DEFAULT)).await?;
            if collection.as_str() == NO_COLLECTION {
                return Err(Error::Refused("no default collection".into()));
            }
            let held = self.at::<CollectionProxy<'_>>(&collection).await?;
            if self.call(held.locked()).await? {
                self.unlock(std::slice::from_ref(&collection)).await?;
            }
            let label = Value::from(key.label);
            let attributes = Value::from(attributes(key));
            let properties = HashMap::from([(LABEL, &label), (ATTRIBUTES, &attributes)]);
            let secret = Secret {
                session: session.clone(),
                parameters: Vec::new(),
                value: token.as_bytes().to_vec(),
                content_type: CONTENT_TYPE.to_owned(),
            };
            let (_item, prompt) = self
                .call(held.create_item(properties, &secret, true))
                .await?;

            self.prompted(&prompt).await
        })
        .await
    }

    async fn remove(&self, key: Key) -> Result<(), Error> {
        let service = self.service().await?;
        let (unlocked, locked) = self.call(service.search_items(attributes(key))).await?;
        if !locked.is_empty() {
            self.unlock(&locked).await?;
        }
        for item in unlocked.iter().chain(&locked) {
            let prompt = self
                .call(self.at::<ItemProxy<'_>>(item).await?.delete())
                .await?;
            self.prompted(&prompt).await?;
        }

        Ok(())
    }
}
