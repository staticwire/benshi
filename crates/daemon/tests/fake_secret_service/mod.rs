//! A Secret Service of the test's own, served at one end of a socket pair.
//!
//! It keeps items in one collection, which the alias `default` names unless
//! a test takes the alias away, and answers as the specification says: an
//! object that is locked is unlocked through a prompt, and a prompt is
//! answered as the test says a person would. Every call is written down, in
//! the order it came.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use benshi_daemon::secrets::{Asking, SecretService};
use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Type, Value};
use zbus::{Connection, DBusError, Guid, ObjectServer, connection, interface};

const SERVICE: &str = "/org/freedesktop/secrets";

/// The one collection.
pub const COLLECTION: &str = "/org/freedesktop/secrets/collection/fake";

/// What the person the store asks does with its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Person {
    /// Types the password.
    Unlocks,
    /// Closes the window.
    Dismisses,
    /// Is not there.
    Away,
}

/// How the store answers a call, where it does not answer as it should.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `org.freedesktop.Secret.Error.IsLocked`.
    IsLocked,
    /// `org.freedesktop.Secret.Error.NoSession`.
    NoSession,
    /// `org.freedesktop.DBus.Error.ServiceUnknown`, which the bus answers
    /// where no program owns the name and none can be started for it.
    ServiceUnknown,
    /// `org.freedesktop.DBus.Error.Spawn.ChildExited`, which the bus answers
    /// where the program it started for the name ended before it took it.
    SpawnFailed,
    /// No answer at all.
    Silent,
}

/// One item, as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub attributes: HashMap<String, String>,
    pub parameters: Vec<u8>,
    pub value: Vec<u8>,
    pub content_type: String,
    pub locked: bool,
}

impl Item {
    /// An item of benshi's for `backend`, holding `value`.
    pub fn of(backend: &str, value: &[u8]) -> Self {
        Self {
            label: format!("{backend} token for benshi"),
            attributes: HashMap::from([
                ("application".to_owned(), "benshi".to_owned()),
                ("backend".to_owned(), backend.to_owned()),
            ]),
            parameters: Vec::new(),
            value: value.to_vec(),
            content_type: "text/plain; charset=utf8".to_owned(),
            locked: false,
        }
    }
}

/// What the store holds and how it behaves.
#[derive(Debug)]
pub struct State {
    pub items: BTreeMap<u32, Item>,
    pub collection_locked: bool,
    /// Whether `default` names the collection.
    pub aliased: bool,
    pub person: Person,
    /// The methods that ask the person before they do what they were asked.
    pub asking: BTreeSet<&'static str>,
    /// The calls that fail, by the name of their method.
    pub faults: HashMap<&'static str, Fault>,
    /// Every call, as `Method` and what it was handed where that matters.
    pub calls: Vec<String>,
    /// The sessions open now.
    pub sessions: BTreeSet<String>,
    /// What `replace` was, at each `CreateItem`.
    pub replaced: Vec<bool>,
    next: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            collection_locked: false,
            aliased: true,
            person: Person::Unlocks,
            asking: BTreeSet::new(),
            faults: HashMap::new(),
            calls: Vec::new(),
            sessions: BTreeSet::new(),
            replaced: Vec::new(),
            next: 1,
        }
    }
}

impl State {
    fn next(&mut self) -> u32 {
        let next = self.next;
        self.next += 1;
        next
    }

    /// The names of the methods called, in order.
    pub fn methods(&self) -> Vec<&str> {
        self.calls
            .iter()
            .map(|call| call.split(' ').next().unwrap_or_default())
            .collect()
    }
}

/// A store, shared between the objects that serve it and the test.
#[derive(Debug, Clone, Default)]
pub struct Fake(Arc<Mutex<State>>);

impl Fake {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().expect("no test panicked holding the store")
    }

    /// The store served at one end of a socket pair, and the adapter at the
    /// other. The connection that serves it is handed back to be kept for as
    /// long as the adapter is used.
    pub async fn serve(&self, deadline: Duration, asking: Asking) -> (SecretService, Connection) {
        let (theirs, ours) = UnixStream::pair().expect("a socket pair");
        let serving = connection::Builder::unix_stream(theirs)
            .server(Guid::generate())
            .expect("a server of this guid")
            .p2p()
            .serve_at(SERVICE, ServiceObject(self.clone()))
            .expect("the service object")
            .serve_at(COLLECTION, CollectionObject(self.clone()))
            .expect("the collection object")
            .build();
        let using = connection::Builder::unix_stream(ours).p2p().build();
        let (serving, using) = tokio::try_join!(serving, using).expect("the two ends connect");

        let held: Vec<u32> = self.state().items.keys().copied().collect();
        for id in held {
            serving
                .object_server()
                .at(
                    item_path(id),
                    ItemObject {
                        fake: self.clone(),
                        id,
                    },
                )
                .await
                .expect("an item object");
        }

        (SecretService::over(using, deadline, asking), serving)
    }

    /// Writes the call down, and answers with its fault where it has one.
    async fn called(&self, method: &'static str, call: String) -> Result<(), Failure> {
        let fault = {
            let mut state = self.state();
            state.calls.push(call);
            state.faults.get(method).copied()
        };

        match fault {
            None => Ok(()),
            Some(Fault::Silent) => std::future::pending().await,
            Some(Fault::IsLocked) => Err(Failure::IsLocked(format!("{method} on a locked object"))),
            Some(Fault::NoSession) => Err(Failure::NoSession("no such session".to_owned())),
            Some(Fault::SpawnFailed) => Err(Failure::SpawnChildExited(
                "Process org.freedesktop.secrets exited with status 1".to_owned(),
            )),
            Some(Fault::ServiceUnknown) => Err(Failure::ServiceUnknown(
                "The name org.freedesktop.secrets was not provided by any .service files"
                    .to_owned(),
            )),
        }
    }

    /// Files `item` and serves it, in place of one of the same attributes
    /// where `replace` is set.
    async fn file(
        &self,
        item: Item,
        replace: bool,
        server: &ObjectServer,
    ) -> Result<OwnedObjectPath, Failure> {
        let (id, new) = {
            let mut state = self.state();
            let same = state
                .items
                .iter()
                .find(|(_, held)| held.attributes == item.attributes)
                .map(|(id, _)| *id);
            if let Some(id) = same.filter(|_| replace) {
                state.items.insert(id, item);
                (id, false)
            } else {
                let id = state.next();
                state.items.insert(id, item);
                (id, true)
            }
        };
        if new {
            server
                .at(
                    item_path(id),
                    ItemObject {
                        fake: self.clone(),
                        id,
                    },
                )
                .await?;
        }

        path(item_path(id))
    }

    /// A prompt that does `action` once the person unlocks.
    async fn prompt(
        &self,
        action: Action,
        server: &ObjectServer,
    ) -> Result<OwnedObjectPath, Failure> {
        let id = self.state().next();
        let at = format!("/org/freedesktop/secrets/prompt/{id}");
        server
            .at(
                at.as_str(),
                PromptObject {
                    fake: self.clone(),
                    action,
                },
            )
            .await?;

        path(at)
    }

    fn is_locked(&self, object: &str) -> bool {
        let state = self.state();
        if object == COLLECTION {
            return state.collection_locked;
        }
        state
            .items
            .iter()
            .any(|(id, item)| item_path(*id) == object && item.locked)
    }
}

fn item_path(id: u32) -> String {
    format!("{COLLECTION}/{id}")
}

fn path(text: String) -> Result<OwnedObjectPath, Failure> {
    OwnedObjectPath::try_from(text).map_err(|wrong| Failure::ZBus(wrong.into()))
}

fn nothing() -> OwnedObjectPath {
    OwnedObjectPath::try_from("/").expect("`/` is a path")
}

/// The errors the store answers with.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.freedesktop")]
pub enum Failure {
    #[zbus(error)]
    ZBus(zbus::Error),
    #[zbus(name = "Secret.Error.IsLocked")]
    IsLocked(String),
    #[zbus(name = "Secret.Error.NoSession")]
    NoSession(String),
    #[zbus(name = "Secret.Error.NoSuchObject")]
    NoSuchObject(String),
    #[zbus(name = "DBus.Error.ServiceUnknown")]
    ServiceUnknown(String),
    #[zbus(name = "DBus.Error.Spawn.ChildExited")]
    SpawnChildExited(String),
    #[zbus(name = "DBus.Error.NotSupported")]
    NotSupported(String),
}

/// A secret as the Secret Service API passes it.
#[derive(Debug, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
pub struct Secret {
    session: OwnedObjectPath,
    parameters: Vec<u8>,
    value: Vec<u8>,
    content_type: String,
}

struct ServiceObject(Fake);

#[interface(name = "org.freedesktop.Secret.Service")]
impl ServiceObject {
    async fn open_session(
        &self,
        algorithm: String,
        input: Value<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<(Value<'static>, OwnedObjectPath), Failure> {
        let input = String::try_from(input).unwrap_or_else(|_| "<not a string>".to_owned());
        self.0
            .called("OpenSession", format!("OpenSession {algorithm} {input:?}"))
            .await?;
        if algorithm != "plain" {
            return Err(Failure::NotSupported(algorithm));
        }
        let id = self.0.state().next();
        let at = format!("/org/freedesktop/secrets/session/{id}");
        server
            .at(
                at.as_str(),
                SessionObject {
                    fake: self.0.clone(),
                    at: at.clone(),
                },
            )
            .await?;
        self.0.state().sessions.insert(at.clone());

        Ok((Value::from(""), path(at)?))
    }

    async fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>), Failure> {
        self.0
            .called("SearchItems", "SearchItems".to_owned())
            .await?;
        let (mut unlocked, mut locked) = (Vec::new(), Vec::new());
        let found: Vec<(u32, bool)> = self
            .0
            .state()
            .items
            .iter()
            .filter(|(_, item)| {
                attributes
                    .iter()
                    .all(|(name, value)| item.attributes.get(name) == Some(value))
            })
            .map(|(id, item)| (*id, item.locked))
            .collect();
        for (id, is_locked) in found {
            if is_locked {
                locked.push(path(item_path(id))?);
            } else {
                unlocked.push(path(item_path(id))?);
            }
        }

        Ok((unlocked, locked))
    }

    async fn unlock(
        &self,
        objects: Vec<OwnedObjectPath>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<(Vec<OwnedObjectPath>, OwnedObjectPath), Failure> {
        self.0.called("Unlock", "Unlock".to_owned()).await?;
        if objects
            .iter()
            .any(|object| self.0.is_locked(object.as_str()))
        {
            let prompt = self.0.prompt(Action::Unlock(objects), server).await?;
            return Ok((Vec::new(), prompt));
        }

        Ok((objects, nothing()))
    }

    async fn read_alias(&self, name: String) -> Result<OwnedObjectPath, Failure> {
        self.0
            .called("ReadAlias", format!("ReadAlias {name}"))
            .await?;
        if name == "default" && self.0.state().aliased {
            return path(COLLECTION.to_owned());
        }

        Ok(nothing())
    }
}

struct CollectionObject(Fake);

#[interface(name = "org.freedesktop.Secret.Collection")]
impl CollectionObject {
    async fn create_item(
        &self,
        properties: HashMap<String, OwnedValue>,
        secret: Secret,
        replace: bool,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<(OwnedObjectPath, OwnedObjectPath), Failure> {
        self.0.called("CreateItem", "CreateItem".to_owned()).await?;
        if self.0.state().collection_locked {
            return Err(Failure::IsLocked("the collection is locked".to_owned()));
        }
        let property = |name: &str| {
            properties
                .get(name)
                .and_then(|value| value.try_clone().ok())
                .ok_or_else(|| Failure::NoSuchObject(format!("no {name}")))
        };
        let label = String::try_from(property("org.freedesktop.Secret.Item.Label")?)
            .map_err(|wrong| Failure::ZBus(wrong.into()))?;
        let attributes = HashMap::<String, String>::try_from(property(
            "org.freedesktop.Secret.Item.Attributes",
        )?)
        .map_err(|wrong| Failure::ZBus(wrong.into()))?;
        if !self.0.state().sessions.contains(secret.session.as_str()) {
            return Err(Failure::NoSession("no such session".to_owned()));
        }
        self.0.state().replaced.push(replace);
        let item = Item {
            label,
            attributes,
            parameters: secret.parameters,
            value: secret.value,
            content_type: secret.content_type,
            locked: false,
        };

        if self.0.state().asking.contains("CreateItem") {
            let prompt = self.0.prompt(Action::Create(item, replace), server).await?;
            return Ok((nothing(), prompt));
        }

        Ok((self.0.file(item, replace, server).await?, nothing()))
    }

    #[zbus(property)]
    async fn locked(&self) -> zbus::fdo::Result<bool> {
        self.0
            .called("Locked", "Locked".to_owned())
            .await
            .map_err(|failure| zbus::fdo::Error::Failed(failure.to_string()))?;

        Ok(self.0.state().collection_locked)
    }
}

struct ItemObject {
    fake: Fake,
    id: u32,
}

#[interface(name = "org.freedesktop.Secret.Item")]
impl ItemObject {
    async fn get_secret(&self, session: OwnedObjectPath) -> Result<Secret, Failure> {
        self.fake
            .called("GetSecret", "GetSecret".to_owned())
            .await?;
        let state = self.fake.state();
        let item = state
            .items
            .get(&self.id)
            .ok_or_else(|| Failure::NoSuchObject("the item is gone".to_owned()))?;
        if item.locked {
            return Err(Failure::IsLocked("the item is locked".to_owned()));
        }
        if !state.sessions.contains(session.as_str()) {
            return Err(Failure::NoSession("no such session".to_owned()));
        }

        Ok(Secret {
            session,
            parameters: item.parameters.clone(),
            value: item.value.clone(),
            content_type: item.content_type.clone(),
        })
    }

    async fn delete(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<OwnedObjectPath, Failure> {
        self.fake.called("Delete", "Delete".to_owned()).await?;
        if self.fake.is_locked(&item_path(self.id)) {
            return Err(Failure::IsLocked("the item is locked".to_owned()));
        }
        if self.fake.state().asking.contains("Delete") {
            return self.fake.prompt(Action::Delete(self.id), server).await;
        }
        self.fake.state().items.remove(&self.id);

        Ok(nothing())
    }
}

struct SessionObject {
    fake: Fake,
    at: String,
}

#[interface(name = "org.freedesktop.Secret.Session")]
impl SessionObject {
    async fn close(&self) -> Result<(), Failure> {
        self.fake.called("Close", "Close".to_owned()).await?;
        self.fake.state().sessions.remove(&self.at);

        Ok(())
    }
}

/// What a prompt does once the person unlocks.
#[derive(Debug)]
enum Action {
    Unlock(Vec<OwnedObjectPath>),
    Create(Item, bool),
    Delete(u32),
}

struct PromptObject {
    fake: Fake,
    action: Action,
}

#[interface(name = "org.freedesktop.Secret.Prompt")]
impl PromptObject {
    async fn prompt(
        &self,
        window_id: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<(), Failure> {
        self.fake
            .called("Prompt", format!("Prompt {window_id:?}"))
            .await?;
        let person = self.fake.state().person;
        match person {
            Person::Unlocks => {
                self.carry_out(server).await?;
                Self::completed(&emitter, false, Value::from("")).await?;
            }
            Person::Dismisses => Self::completed(&emitter, true, Value::from("")).await?,
            Person::Away => {}
        }

        Ok(())
    }

    async fn dismiss(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), Failure> {
        self.fake.called("Dismiss", "Dismiss".to_owned()).await?;
        Self::completed(&emitter, true, Value::from("")).await?;

        Ok(())
    }

    #[zbus(signal)]
    async fn completed(
        emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: Value<'_>,
    ) -> zbus::Result<()>;
}

impl PromptObject {
    async fn carry_out(&self, server: &ObjectServer) -> Result<(), Failure> {
        match &self.action {
            Action::Unlock(objects) => {
                let mut state = self.fake.state();
                for object in objects {
                    if object.as_str() == COLLECTION {
                        state.collection_locked = false;
                    }
                    for (id, item) in &mut state.items {
                        if item_path(*id) == object.as_str() {
                            item.locked = false;
                        }
                    }
                }
            }
            Action::Create(item, replace) => {
                self.fake.file(item.clone(), *replace, server).await?;
            }
            Action::Delete(id) => {
                self.fake.state().items.remove(id);
            }
        }

        Ok(())
    }
}
