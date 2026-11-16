//! Sending what was seen to the lists.
//!
//! Each backend takes the operations past its own cursor in the log, lowest
//! first. For each it finds the episode of an entry the operation goes to, as
//! the dry run of [`delivery`](crate::delivery) does from what the store holds
//! of the list, reads that entry from the service, and decides what seeing the
//! episode writes to it. The write is kept in the store before it is sent, and
//! the cursor moves past the operation once the service has taken it. Writes
//! whose answer was lost are sent again as they were kept: deciding again
//! would read a list the lost write has changed already.
//!
//! Operations that go to no entry, whose entry left the list, whose entry is
//! a number the service has no work under, or whose row holds none are set
//! aside with why, and the cursor moves past them. So are those whose entry
//! the store holds and the service no longer does: a write would add the
//! entry to the list again, and an entry that left the list is not added to
//! it again. What was set aside is tried again where what it goes to may
//! have changed.
//!
//! Failures of the service end a pass where it is, and the operations left
//! wait for the next one. Each backend has a cursor of its own, so one that
//! cannot be reached holds back no other.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use benshi_core::list::{Dated, Write};
use benshi_core::token::Token;
use benshi_store::Store;
use benshi_store::delivery::Pending;
use benshi_sync::ListBackend;
#[cfg(target_os = "linux")]
use benshi_sync::anilist::transport::Http;
#[cfg(target_os = "linux")]
use benshi_sync::anilist::{AniList, Turns};
use benshi_sync::plan::{Plan, Standing, plan};
use benshi_sync::relations::Relations;
use benshi_sync::resolve::{By, Target};
use humantime::format_duration;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::bus::{BusEvent, EventBus};
use crate::config::{Sending, When};
use crate::delivery::{Destination, Kept, Lookup, Seen, Unsent, Watched, seen};
use crate::detection::POISONED_STORE;
use crate::listing::{FIRST_RETRY, LAST_RETRY, Say, Wakes, until};
use crate::secrets;
use crate::supervisor::TaskError;

/// How many operations a pass dealt with, by what it did with each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    /// Operations written to the list.
    pub sent: usize,
    /// Operations that wrote nothing.
    pub nothing: usize,
    /// Operations set aside.
    pub aside: usize,
}

impl std::ops::Add for Tally {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            sent: self.sent + other.sent,
            nothing: self.nothing + other.nothing,
            aside: self.aside + other.aside,
        }
    }
}

/// Why a pass stopped before it dealt with every operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The service refused the token, in its words.
    Refused(String),
    /// The service or the store failed, as a person reads it, and how long
    /// the service asked to be left alone where it asked.
    Failed {
        /// What failed.
        said: String,
        /// The wait the service asked for.
        asked: Option<Duration>,
    },
}

/// What a pass came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    /// What it did with the operations it dealt with.
    pub tally: Tally,
    /// Why it stopped before the last, and how many operations wait, where
    /// it stopped.
    pub stopped: Option<(Stop, usize)>,
}

/// Sending to one list: the store it keeps its place in, the zone that names
/// the day of a viewing, and the rules it resolves by.
pub struct Sender {
    list: String,
    store: Arc<Mutex<Store>>,
    zone: TimeZone,
    rules: &'static Relations,
}

/// What an operation came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counted {
    Sent,
    Nothing,
    Aside,
}

impl Sender {
    /// Sending what the store keeps under `list`, with the day of a viewing
    /// named in `zone` and an episode sent on by `rules`.
    #[must_use]
    pub fn new(
        list: &str,
        store: Arc<Mutex<Store>>,
        zone: TimeZone,
        rules: &'static Relations,
    ) -> Self {
        Self {
            list: list.to_owned(),
            store,
            zone,
            rules,
        }
    }

    /// Send every operation past the cursor through `backend`, lowest first,
    /// and tell each event of what was done through `tell`.
    ///
    /// # Panics
    ///
    /// If a panic elsewhere poisoned the store, and with the panic of a call
    /// into the store, handed on as it was raised.
    pub async fn pass<B: ListBackend + Sync>(
        &self,
        backend: &B,
        tell: &(dyn Fn(BusEvent) + Sync),
    ) -> Passed {
        let list = self.list.clone();
        let read = self
            .ask(move |store| Ok((store.pending(&list)?, Kept::of(store, &list)?)))
            .await;

        self.deal_with(backend, read, tell, true).await
    }

    /// Try again every operation set aside, lowest first, as [`Sender::pass`]
    /// sends one past the cursor. Those that still cannot be sent stay aside
    /// and are told of no more, and the tally leaves them out.
    ///
    /// # Panics
    ///
    /// As [`Sender::pass`].
    pub async fn retry<B: ListBackend + Sync>(
        &self,
        backend: &B,
        tell: &(dyn Fn(BusEvent) + Sync),
    ) -> Passed {
        let list = self.list.clone();
        let read = self
            .ask(move |store| {
                let aside: Vec<Pending> = store
                    .aside(&list)?
                    .into_iter()
                    .map(|aside| Pending {
                        id: aside.id,
                        operation: aside.operation,
                    })
                    .collect();
                Ok((aside, Kept::of(store, &list)?))
            })
            .await;

        self.deal_with(backend, read, tell, false).await
    }

    /// Send the operations `read` from the store, in the order read, by what
    /// it holds of the list. Operations set aside are told and counted only
    /// where `tell_aside`.
    async fn deal_with<B: ListBackend + Sync>(
        &self,
        backend: &B,
        read: Result<(Vec<Pending>, Kept), Stop>,
        tell: &(dyn Fn(BusEvent) + Sync),
        tell_aside: bool,
    ) -> Passed {
        let (operations, kept) = match read {
            Ok(read) => read,
            Err(stop) => {
                return Passed {
                    tally: Tally::default(),
                    stopped: Some((stop, 0)),
                };
            }
        };

        let lookup = Lookup::new(&kept, self.rules);
        let told = |event: BusEvent| {
            if tell_aside || !matches!(event, BusEvent::SetAside { .. }) {
                tell(event);
            }
        };
        let mut tally = Tally::default();
        let waiting = operations.len();
        for (done, pending) in operations.into_iter().enumerate() {
            match self.one(backend, &lookup, pending, &told).await {
                Ok(Counted::Sent) => tally.sent += 1,
                Ok(Counted::Nothing) => tally.nothing += 1,
                Ok(Counted::Aside) if tell_aside => tally.aside += 1,
                Ok(Counted::Aside) => {}
                Err(stop) => {
                    return Passed {
                        tally,
                        stopped: Some((stop, waiting - done)),
                    };
                }
            }
        }
        Passed {
            tally,
            stopped: None,
        }
    }

    /// Send one operation.
    async fn one<B: ListBackend + Sync>(
        &self,
        backend: &B,
        lookup: &Lookup<'_>,
        pending: Pending,
        tell: &(dyn Fn(BusEvent) + Sync),
    ) -> Result<Counted, Stop> {
        let id = pending.id;
        let seen = match seen(pending, &self.zone) {
            Ok(seen) => seen,
            Err(why) => {
                return self
                    .aside(backend, id, None, None, Unsent::Unread(why), tell)
                    .await;
            }
        };
        let list = self.list.clone();
        if let Some(kept) = self.ask(move |store| store.decided(&list, id)).await? {
            return self.again(backend, lookup, id, seen, kept, tell).await;
        }

        let to = match lookup.destination(seen.show, seen.watched.episode) {
            Ok(to) => to,
            Err((to, why)) => {
                return self
                    .aside(backend, id, Some(seen.watched), to, why, tell)
                    .await;
            }
        };
        let held = backend
            .entry(to.entry)
            .await
            .map_err(|failed| stopped(backend.name(), failed))?;
        let (before, to) = match held {
            Some(dated) => (
                Some(standing(&dated)),
                Destination {
                    episodes: dated.entry.episodes,
                    ..to
                },
            ),
            None if to.listed => {
                return self
                    .aside(
                        backend,
                        id,
                        Some(seen.watched),
                        Some(to),
                        Unsent::Departed,
                        tell,
                    )
                    .await;
            }
            None => (None, to),
        };

        match plan(before.as_ref(), to.episodes, to.episode, seen.watched.day) {
            Plan::Nothing(why) => {
                self.delivered(id).await?;
                tell(BusEvent::NothingWritten {
                    list: backend.name().to_owned(),
                    id,
                    watched: seen.watched,
                    to,
                    why,
                });
                Ok(Counted::Nothing)
            }
            Plan::Write(after) => {
                let write = Write {
                    id: to.entry,
                    status: after.status,
                    progress: after.progress,
                    rewatches: after.rewatches,
                    started: after.started,
                    completed: after.completed,
                };
                let list = self.list.clone();
                self.ask(move |store| store.decide(&list, id, &write))
                    .await?;
                let sending = Outgoing {
                    id,
                    watched: seen.watched,
                    to,
                    before,
                    after,
                    again: false,
                };
                self.send(backend, &write, sending, tell).await
            }
        }
    }

    /// Send again the write kept for operation `id`, as it was decided.
    async fn again<B: ListBackend + Sync>(
        &self,
        backend: &B,
        lookup: &Lookup<'_>,
        id: i64,
        seen: Seen,
        kept: Write,
        tell: &(dyn Fn(BusEvent) + Sync),
    ) -> Result<Counted, Stop> {
        let to = lookup.to(
            lookup.own(seen.show),
            Target {
                entry: kept.id,
                episode: kept.progress,
            },
            By::Entry,
        );
        let sending = Outgoing {
            id,
            watched: seen.watched,
            to,
            before: None,
            after: Standing {
                status: kept.status,
                progress: kept.progress,
                rewatches: kept.rewatches,
                started: kept.started,
                completed: kept.completed,
            },
            again: true,
        };
        self.send(backend, &kept, sending, tell).await
    }

    /// Send `write`, kept already, and move the cursor past its operation once
    /// the service has taken it.
    async fn send<B: ListBackend + Sync>(
        &self,
        backend: &B,
        write: &Write,
        sending: Outgoing,
        tell: &(dyn Fn(BusEvent) + Sync),
    ) -> Result<Counted, Stop> {
        match backend.write(write).await {
            Ok(()) => {
                self.delivered(sending.id).await?;
                tell(BusEvent::Sent {
                    list: backend.name().to_owned(),
                    id: sending.id,
                    watched: sending.watched,
                    to: sending.to,
                    before: sending.before,
                    after: sending.after,
                    again: sending.again,
                });
                Ok(Counted::Sent)
            }
            Err(benshi_sync::Error::NoWork { .. }) => {
                self.aside(
                    backend,
                    sending.id,
                    Some(sending.watched),
                    Some(sending.to),
                    Unsent::NoWork,
                    tell,
                )
                .await
            }
            Err(failed) => Err(stopped(backend.name(), failed)),
        }
    }

    /// Set operation `id` aside for `why`, and tell of it.
    async fn aside<B: ListBackend + Sync>(
        &self,
        backend: &B,
        id: i64,
        watched: Option<Watched>,
        to: Option<Destination>,
        why: Unsent,
        tell: &(dyn Fn(BusEvent) + Sync),
    ) -> Result<Counted, Stop> {
        let reason = why.reason(watched.as_ref().and_then(|watched| watched.episode));
        let list = self.list.clone();
        self.ask(move |store| store.set_aside(&list, id, &reason))
            .await?;
        tell(BusEvent::SetAside {
            list: backend.name().to_owned(),
            id,
            watched,
            to,
            why,
        });
        Ok(Counted::Aside)
    }

    /// Move the cursor past operation `id`, delivered.
    async fn delivered(&self, id: i64) -> Result<(), Stop> {
        let list = self.list.clone();
        self.ask(move |store| store.delivered(&list, id)).await
    }

    /// How many operations are past the cursor, and none where the store
    /// cannot say.
    async fn past_the_cursor(&self) -> usize {
        let list = self.list.clone();
        self.ask(move |store| Ok(store.pending(&list)?.len()))
            .await
            .unwrap_or(0)
    }

    /// Ask the store, on a thread that may wait for it, or stop the pass with
    /// why it could not answer.
    async fn ask<T: Send + 'static>(
        &self,
        asked: impl FnOnce(&mut Store) -> Result<T, benshi_store::Error> + Send + 'static,
    ) -> Result<T, Stop> {
        let store = Arc::clone(&self.store);
        let answer = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().expect(POISONED_STORE);
            asked(&mut store)
        });

        let said = match answer.await {
            Ok(Ok(answered)) => return Ok(answered),
            Ok(Err(refused)) => format!("store refused: {refused}"),
            // Handed on as it was raised.
            Err(panicked) if panicked.is_panic() => {
                std::panic::resume_unwind(panicked.into_panic())
            }
            Err(given_up) => format!("store could not be asked: {given_up}"),
        };
        Err(Stop::Failed { said, asked: None })
    }
}

/// One write on its way, and what is told of it once the service has it.
struct Outgoing {
    id: i64,
    watched: Watched,
    to: Destination,
    before: Option<Standing>,
    after: Standing,
    again: bool,
}

/// Where the account stands on an entry, as the service holds it.
fn standing(dated: &Dated) -> Standing {
    Standing {
        status: dated.entry.status,
        progress: dated.entry.progress,
        rewatches: dated.entry.rewatches,
        started: dated.started,
        completed: dated.completed,
    }
}

/// Why a pass stops where the service of `list` failed.
fn stopped(list: &str, failed: benshi_sync::Error) -> Stop {
    match failed {
        benshi_sync::Error::Refused(why) => Stop::Refused(why),
        benshi_sync::Error::Wait { after } => Stop::Failed {
            said: format!("{list} {failed}"),
            asked: Some(after),
        },
        failed => Stop::Failed {
            said: format!("{list} {failed}"),
            asked: None,
        },
    }
}

/// What is said where no token is found.
const NO_TOKEN: &str = "no AniList token, so nothing is sent: benshi login keeps one";

/// What a pass asked for at once came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ending {
    /// Every operation that waited was dealt with.
    Done(Tally),
    /// The pass stopped before the last operation.
    Stopped {
        /// What it dealt with before it stopped.
        tally: Tally,
        /// Why it stopped, how many operations wait and for what, as a person
        /// reads it.
        line: String,
    },
    /// Nothing was sent, since no token was found or none could be read, and
    /// why, as a person reads it.
    NoToken(String),
}

/// What a pass asked for at once tells as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// One event of the pass, as the bus carries it.
    Event(BusEvent),
    /// What the pass came to, the last thing heard.
    Ended(Ending),
}

/// Where a list is sent to: the token, and the backend that sends with it.
pub trait Account {
    /// The backend a pass sends through.
    type Backend: ListBackend + Send + Sync;

    /// The token to send with, and none where none is kept.
    fn token(&self) -> impl Future<Output = Result<Option<Token>, secrets::Error>> + Send;

    /// The backend that sends with `token`.
    ///
    /// # Errors
    ///
    /// [`benshi_sync::Error::Unreachable`] where no backend can be built.
    fn backend(&self, token: &Token) -> Result<Self::Backend, benshi_sync::Error>;
}

/// The token and the backend of AniList.
///
/// The token is the one [`token_of`](crate::listing::token_of) finds. Its
/// requests take turns shared with the list's task.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct ToAniList {
    address: String,
    given: Option<Token>,
    turns: Turns,
}

#[cfg(target_os = "linux")]
impl ToAniList {
    /// Writes sent to the service at `address`, with `given` as the token
    /// where there is one, its requests taking `turns`.
    #[must_use]
    pub fn new(address: &str, given: Option<Token>, turns: &Turns) -> Self {
        Self {
            address: address.to_owned(),
            given,
            turns: turns.clone(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Account for ToAniList {
    type Backend = AniList<Http>;

    async fn token(&self) -> Result<Option<Token>, secrets::Error> {
        crate::listing::token_of(self.given.as_ref()).await
    }

    fn backend(&self, token: &Token) -> Result<AniList<Http>, benshi_sync::Error> {
        crate::listing::anilist_at(&self.address, token, &self.turns)
    }
}

/// What the sending task sends through, when, and how it says what fails.
pub struct Sends<A> {
    /// Where the token and the backend come from.
    pub account: A,
    /// When what was seen is sent.
    pub sending: Sending,
    /// How the task says a line.
    pub say: Say,
}

/// What asks a sending task for a pass at once, as `benshi sync now` does.
/// Cloning shares it.
#[derive(Debug, Clone)]
pub struct SendNow(mpsc::UnboundedSender<mpsc::UnboundedSender<Heard>>);

impl SendNow {
    /// Ask for a pass at once, and hear what it does as it goes, the last
    /// thing heard being what it came to. None where no task takes asks.
    #[must_use]
    pub fn ask(&self) -> Option<mpsc::UnboundedReceiver<Heard>> {
        let (heard, hearing) = mpsc::unbounded_channel();
        self.0.send(heard).ok()?;
        Some(hearing)
    }
}

/// The asks a sending task takes from its [`SendNow`].
#[derive(Debug)]
pub struct Asked(tokio::sync::Mutex<mpsc::UnboundedReceiver<mpsc::UnboundedSender<Heard>>>);

impl Asked {
    /// The next ask, where what it does is told, and none once no
    /// [`SendNow`] is left to ask.
    pub async fn next(&self) -> Option<mpsc::UnboundedSender<Heard>> {
        self.0.lock().await.recv().await
    }
}

/// What asks a sending task for a pass at once, and the asks it takes.
#[must_use]
pub fn send_now() -> (SendNow, Asked) {
    let (asking, asked) = mpsc::unbounded_channel();
    (SendNow(asking), Asked(tokio::sync::Mutex::new(asked)))
}

/// What came of an attempt, for when the next is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Every operation that waited was dealt with.
    Idle,
    /// Nothing is sent until a login.
    Login,
    /// The attempt is made again after this long, the wait between attempts
    /// growing where the service asked for none.
    Retry { after: Duration, grows: bool },
}

/// The sending task of one list: where it sends, when `[send]` says, and how
/// it says what it does.
pub struct SendTask<A> {
    account: A,
    sender: Sender,
    sending: Sending,
    bus: Arc<EventBus>,
    wakes: Wakes,
    asked: Asked,
    say: Say,
}

impl<A: Account + Sync> SendTask<A> {
    /// The task that sends through `account` with `sender` at the times
    /// `sending` names, tells each event on `bus`, is woken by `wakes` and
    /// asked by the [`SendNow`] of `asked`, and says what fails through `say`.
    #[must_use]
    pub fn new(
        account: A,
        sender: Sender,
        sending: Sending,
        bus: Arc<EventBus>,
        wakes: Wakes,
        asked: Asked,
        say: Say,
    ) -> Self {
        Self {
            account,
            sender,
            sending,
            bus,
            wakes,
            asked,
            say,
        }
    }

    /// Send at the times the settings name, for ever.
    ///
    /// Attempts that fail are made again after [`FIRST_RETRY`], and after
    /// twice as long each time they fail again, up to [`LAST_RETRY`], or after
    /// the wait the service asked for. Tokens the service refuses, and tokens
    /// not found or that cannot be read, wait for a login. Viewings recorded
    /// meanwhile wait with the rest, and `benshi sync now` is attempted at
    /// once whatever came before. What was set aside is tried again at the
    /// start, at `benshi sync now`, and when the list is read or a name or a
    /// pin is given or taken back.
    ///
    /// # Errors
    ///
    /// None: every failure of an attempt is said, and the task goes on. The
    /// result is the supervisor's.
    pub async fn run(&self) -> Result<(), TaskError> {
        let each = match self.sending.when {
            When::Each => true,
            When::Manual => false,
            When::Close | When::Interval => {
                (self.say)(
                    "send when \"close\" and \"interval\" are not built yet: each viewing is sent \
                     as it is recorded",
                );
                true
            }
        };
        let mut asks = self.asked.0.lock().await;
        let mut wait = FIRST_RETRY;
        let mut said = None;
        let mut held = false;
        let mut due = self.sending.at_start.then(Instant::now);
        let mut retrying = self.sending.at_start;

        loop {
            let asker = tokio::select! {
                () = until(due) => None,
                () = self.wakes.recorded.notified(), if each && !held => None,
                () = self.wakes.resolvable.notified(), if !held => {
                    retrying = true;
                    None
                }
                () = self.wakes.sending_logged_in.notified() => {
                    said = None;
                    None
                }
                Some(asker) = asks.recv() => {
                    retrying = true;
                    Some(asker)
                }
            };

            let (ending, next) = self
                .attempt(&mut said, wait, asker.as_ref(), retrying)
                .await;
            if let Some(asker) = &asker {
                // Clients that stopped listening are no failure of sending.
                drop(asker.send(Heard::Ended(ending)));
            }
            (held, due) = match next {
                Next::Idle => {
                    wait = FIRST_RETRY;
                    retrying = false;
                    (false, None)
                }
                Next::Login => (true, None),
                Next::Retry { after, grows } => {
                    if grows {
                        wait = (wait * 2).min(LAST_RETRY);
                    }
                    (true, Some(Instant::now() + after))
                }
            };
        }
    }

    /// Read the token, send what waits, what was set aside first where
    /// `retrying`, and say what failed, telling each event to `asker` as well
    /// where one asked. `wait` is how long a failure waits before the attempt
    /// is made again, where the service asked for no wait of its own.
    async fn attempt(
        &self,
        said: &mut Option<String>,
        wait: Duration,
        asker: Option<&mpsc::UnboundedSender<Heard>>,
        retrying: bool,
    ) -> (Ending, Next) {
        let token = match self.account.token().await {
            Ok(Some(token)) => token,
            Ok(None) => {
                self.tell(said, NO_TOKEN, NO_TOKEN);
                return (Ending::NoToken(NO_TOKEN.to_owned()), Next::Login);
            }
            Err(unread) => {
                let line = format!("AniList token could not be read, so nothing is sent: {unread}");
                self.tell(said, &line, &line);
                return (Ending::NoToken(line), Next::Login);
            }
        };
        let backend = match self.account.backend(&token) {
            Ok(backend) => backend,
            Err(unbuilt) => {
                let failed = format!("AniList {unbuilt}");
                return self.failed(said, Tally::default(), &failed, None, 0, wait);
            }
        };

        let tell = |event: BusEvent| {
            if let Some(asker) = asker {
                // Clients that stopped listening are no failure of sending.
                drop(asker.send(Heard::Event(event.clone())));
            }
            self.bus.publish(event);
        };
        let retried = if retrying {
            self.sender.retry(&backend, &tell).await
        } else {
            Passed {
                tally: Tally::default(),
                stopped: None,
            }
        };
        let passed = if let Some((stop, _aside)) = retried.stopped {
            // What was set aside waits for the user, and what is past the
            // cursor for the service.
            Passed {
                tally: retried.tally,
                stopped: Some((stop, self.sender.past_the_cursor().await)),
            }
        } else {
            let passed = self.sender.pass(&backend, &tell).await;
            Passed {
                tally: retried.tally + passed.tally,
                ..passed
            }
        };
        match passed.stopped {
            None => {
                *said = None;
                (Ending::Done(passed.tally), Next::Idle)
            }
            Some((Stop::Refused(why), waiting)) => {
                let reason = format!("{} refused the token: {why}", backend.name());
                let line = if waiting == 0 {
                    format!("{reason}; nothing is sent before benshi login")
                } else {
                    format!("{reason}; {} for benshi login", waits(waiting))
                };
                self.tell(said, &reason, &line);
                (
                    Ending::Stopped {
                        tally: passed.tally,
                        line,
                    },
                    Next::Login,
                )
            }
            Some((
                Stop::Failed {
                    said: failed,
                    asked,
                },
                waiting,
            )) => self.failed(said, passed.tally, &failed, asked, waiting, wait),
        }
    }

    /// What a failure comes to, said where its reason is not what was said
    /// last.
    fn failed(
        &self,
        said: &mut Option<String>,
        tally: Tally,
        failed: &str,
        asked: Option<Duration>,
        waiting: usize,
        wait: Duration,
    ) -> (Ending, Next) {
        let after = asked.unwrap_or(wait);
        let line = if waiting == 0 {
            format!("{failed}; tried again in {}", format_duration(after))
        } else {
            format!(
                "{failed}; {}, tried again in {}",
                waits(waiting),
                format_duration(after)
            )
        };
        self.tell(said, failed, &line);
        (
            Ending::Stopped { tally, line },
            Next::Retry {
                after,
                grows: asked.is_none(),
            },
        )
    }

    /// Say `line` where `reason` is not what was said last.
    fn tell(&self, said: &mut Option<String>, reason: &str, line: &str) {
        if said.as_deref() != Some(reason) {
            (self.say)(line);
            *said = Some(reason.to_owned());
        }
    }
}

/// How many operations wait, in words.
fn waits(operations: usize) -> String {
    if operations == 1 {
        "1 operation waits".to_owned()
    } else {
        format!("{operations} operations wait")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, SystemTime};

    use benshi_core::ShowId;
    use benshi_core::list::{Dated, Day, Entry, EntryId, Format, Status, Write};
    use benshi_core::path::RawPath;
    use benshi_core::pin::{Episodes, Pin};
    use benshi_store::{Store, Viewing};
    use benshi_sync::memory::{Fault, Memory};
    use benshi_sync::plan::{Standing, Why};
    use benshi_sync::relations::Relations;
    use benshi_sync::resolve::{By, Unresolved};
    use benshi_sync::{Error, ListBackend};
    use jiff::tz::TimeZone;
    use tempfile::TempDir;

    use benshi_core::token::Token;
    use tokio::sync::broadcast;
    use tokio::time::Instant;

    use super::{
        Account, Ending, Heard, NO_TOKEN, Passed, SendNow, SendTask, Sender, Stop, Tally, send_now,
    };
    use crate::bus::{BusEvent, EventBus};
    use crate::config::{Sending, When};
    use crate::delivery::{Destination, Unsent, Watched};
    use crate::listing::Wakes;
    use crate::secrets;
    use crate::supervisor::TaskError;

    /// The list every operation here is sent to, as the store files it.
    const LIST: &str = "anilist";

    /// The entry the list holds of the show every viewing here is of.
    const ENTRY: EntryId = EntryId(12345);

    /// Another entry, which a rule sends an episode to.
    const SECOND: EntryId = EntryId(900_001);

    /// How long the backends in memory here give a call.
    const DEADLINE: Duration = Duration::from_secs(15);

    /// Noon, UTC, on this day of November 2026.
    fn noon(day: u8) -> SystemTime {
        SystemTime::from(
            format!("2026-11-{day:02}T12:00:00Z")
                .parse::<jiff::Timestamp>()
                .expect("an instant"),
        )
    }

    const fn november(day: u8) -> Day {
        Day::new(2026, 11, day)
    }

    /// One entry of `episodes` episodes, where the account stands at `status`
    /// and `progress`.
    fn an_entry(id: EntryId, episodes: Option<u32>, status: Status, progress: u32) -> Entry {
        Entry {
            id,
            title: format!("Show {}", id.0),
            spellings: Vec::new(),
            format: Some(Format::Tv),
            episodes,
            episode_length: None,
            year: None,
            released: true,
            status,
            progress,
            rewatches: 0,
            score: None,
        }
    }

    /// One film, seen and seen again `rewatches` times.
    fn a_film(id: EntryId, rewatches: u32) -> Entry {
        Entry {
            format: Some(Format::Film),
            rewatches,
            ..an_entry(id, Some(1), Status::Completed, 1)
        }
    }

    /// One store of our own, `listed` filed under `list`.
    fn a_store(list: &str, listed: &[Entry]) -> (TempDir, Arc<Mutex<Store>>) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let mut store = Store::open(&home.path().join("benshi.db")).expect("the database opens");
        store.file(list, listed).expect("the list is filed");
        (home, Arc::new(Mutex::new(store)))
    }

    /// The show the store files `entry` of `list` under.
    fn show_of(store: &Arc<Mutex<Store>>, list: &str, entry: EntryId) -> ShowId {
        store
            .lock()
            .expect("the store")
            .listed(list)
            .expect("the list reads")
            .into_iter()
            .find(|(_, listed)| listed.id == entry)
            .map(|(show, _)| show)
            .expect("the entry is filed")
    }

    /// Record a viewing of this episode of `show` at noon on November 1.
    fn record(store: &Arc<Mutex<Store>>, show: ShowId, episode: u32) {
        store
            .lock()
            .expect("the store")
            .record(&Viewing {
                show,
                episode: Some(episode),
                media: RawPath::from_bytes(format!("Show Title - {episode:02}.mkv").into_bytes()),
                at: noon(1),
            })
            .expect("the viewing is recorded");
    }

    /// The numbers of the operations past the cursor of `list`.
    fn pending(store: &Arc<Mutex<Store>>, list: &str) -> Vec<i64> {
        store
            .lock()
            .expect("the store")
            .pending(list)
            .expect("the log reads")
            .into_iter()
            .map(|pending| pending.id)
            .collect()
    }

    /// The write the store keeps for operation `id` of `list`.
    fn decided(store: &Arc<Mutex<Store>>, list: &str, id: i64) -> Option<Write> {
        store
            .lock()
            .expect("the store")
            .decided(list, id)
            .expect("the kept writes read")
    }

    /// These rule lines, read as a file and kept for the run of the tests.
    fn rules(lines: &str) -> &'static Relations {
        let text = format!("::meta\n- version: 1.3.0\n::rules\n{lines}\n");
        Box::leak(Box::new(
            Relations::read(&text).unwrap_or_else(|refused| panic!("{refused}: {text}")),
        ))
    }

    /// Sending to `list` from `store`, a viewing on the day UTC names.
    fn a_sender(list: &str, store: &Arc<Mutex<Store>>, rules: &'static Relations) -> Sender {
        Sender::new(list, Arc::clone(store), TimeZone::UTC, rules)
    }

    /// Every event told, in the order told.
    #[derive(Default)]
    struct Told(Mutex<Vec<BusEvent>>);

    impl Told {
        fn tell(&self, event: BusEvent) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(event);
        }

        fn events(&self) -> Vec<BusEvent> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    /// AniList as a list in memory, holding `listed` and having `unlisted`.
    fn anilist(listed: Vec<Entry>, unlisted: Vec<Entry>) -> Memory {
        Memory::new("AniList", DEADLINE, listed, unlisted)
    }

    /// Where the account stands on `entry` in `backend`.
    async fn held(backend: &impl ListBackend, entry: EntryId) -> Option<Dated> {
        backend.entry(entry).await.expect("the entry is read")
    }

    const fn standing(
        status: Status,
        progress: u32,
        rewatches: u32,
        started: Option<Day>,
        completed: Option<Day>,
    ) -> Standing {
        Standing {
            status,
            progress,
            rewatches,
            started,
            completed,
        }
    }

    /// What the viewings recorded here say was watched.
    fn watched(episode: u32) -> Watched {
        Watched {
            day: november(1),
            title: format!("Show {}", ENTRY.0),
            episode: Some(episode),
        }
    }

    /// This episode of [`ENTRY`], twelve episodes, as its own show reaches it.
    fn of_the_entry(episode: u32) -> Destination {
        Destination {
            entry: ENTRY,
            episode,
            by: By::Entry,
            title: None,
            episodes: Some(12),
            listed: true,
        }
    }

    #[tokio::test]
    async fn an_episode_past_the_progress_is_written_and_the_cursor_moves_past_it() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 3);
        let backend = anilist(vec![entry], Vec::new());
        let told = Told::default();

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(
            passed,
            Passed {
                tally: Tally {
                    sent: 1,
                    ..Tally::default()
                },
                stopped: None,
            }
        );
        let after = standing(Status::Watching, 3, 0, Some(november(1)), None);
        assert_eq!(
            told.events(),
            [BusEvent::Sent {
                list: "AniList".to_owned(),
                id: 1,
                watched: watched(3),
                to: of_the_entry(3),
                before: Some(standing(Status::Watching, 2, 0, None, None)),
                after,
                again: false,
            }]
        );
        let written = held(&backend, ENTRY)
            .await
            .expect("the list holds the entry");
        assert_eq!(super::standing(&written), after);
        assert_eq!(pending(&store, LIST), Vec::<i64>::new());
        assert_eq!(decided(&store, LIST, 1), None);
    }

    #[tokio::test]
    async fn an_episode_at_or_below_the_progress_writes_nothing_and_the_conflict_is_told() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 5);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 3);
        let backend = anilist(vec![entry], Vec::new());
        let told = Told::default();

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(passed.tally.nothing, 1);
        assert_eq!(
            told.events(),
            [BusEvent::NothingWritten {
                list: "AniList".to_owned(),
                id: 1,
                watched: watched(3),
                to: of_the_entry(3),
                why: Why::Conflict { progress: 5 },
            }]
        );
        let kept = held(&backend, ENTRY)
            .await
            .expect("the list holds the entry");
        assert_eq!(kept.entry.progress, 5);
        assert_eq!(pending(&store, LIST), Vec::<i64>::new());
    }

    #[tokio::test]
    async fn a_list_that_cannot_be_reached_holds_back_no_other() {
        let first = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let second = an_entry(SECOND, Some(12), Status::Watching, 2);
        let (_home, store) = a_store("first", std::slice::from_ref(&first));
        store
            .lock()
            .expect("the store")
            .file("second", std::slice::from_ref(&second))
            .expect("the second list is filed");
        // The first list sets aside the viewing of the second's show without
        // asking its service, and stops at its own.
        record(&store, show_of(&store, "second", SECOND), 3);
        record(&store, show_of(&store, "first", ENTRY), 3);
        let unreachable =
            Memory::new("First", DEADLINE, vec![first], Vec::new()).failing(Fault::Unreachable);
        let reachable = Memory::new("Second", DEADLINE, vec![second], Vec::new());

        let (to_first, to_second) = (
            a_sender("first", &store, rules("")),
            a_sender("second", &store, rules("")),
        );

        let (stuck, through) = tokio::join!(
            to_first.pass(&unreachable, &|_| {}),
            to_second.pass(&reachable, &|_| {}),
        );

        assert!(
            matches!(&stuck.stopped, Some((Stop::Failed { .. }, 1))),
            "{stuck:?}"
        );
        assert_eq!(stuck.tally.aside, 1);
        assert_eq!(pending(&store, "first"), [2]);
        assert_eq!(
            through,
            Passed {
                tally: Tally {
                    sent: 1,
                    aside: 1,
                    ..Tally::default()
                },
                stopped: None,
            }
        );
        assert_eq!(pending(&store, "second"), Vec::<i64>::new());
        let written = held(&reachable, SECOND).await.expect("the list holds it");
        assert_eq!(written.entry.progress, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_that_never_answers_holds_back_no_other() {
        let first = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let second = an_entry(SECOND, Some(12), Status::Watching, 2);
        let (_home, store) = a_store("first", std::slice::from_ref(&first));
        store
            .lock()
            .expect("the store")
            .file("second", std::slice::from_ref(&second))
            .expect("the second list is filed");
        record(&store, show_of(&store, "first", ENTRY), 3);
        record(&store, show_of(&store, "second", SECOND), 3);
        let silent = Memory::new("First", DEADLINE, vec![first], Vec::new()).failing(Fault::Silent);
        let answering = Memory::new("Second", DEADLINE, vec![second], Vec::new());
        let (to_first, to_second) = (
            a_sender("first", &store, rules("")),
            a_sender("second", &store, rules("")),
        );
        let started = tokio::time::Instant::now();

        let (stuck, through) = tokio::join!(to_first.pass(&silent, &|_| {}), async {
            let through = to_second.pass(&answering, &|_| {}).await;
            (through, started.elapsed())
        },);

        assert!(
            matches!(&stuck.stopped, Some((Stop::Failed { .. }, 2))),
            "{stuck:?}"
        );
        let (through, took) = through;
        assert_eq!(through.tally.sent, 1);
        assert!(took < DEADLINE, "the second list waited {took:?}");
    }

    /// One list in memory that loses the answer to its first write: the write
    /// is taken, and the call fails as one the service never answered.
    struct LosesTheFirstAnswer {
        inner: Memory,
        lost: AtomicBool,
    }

    impl ListBackend for LosesTheFirstAnswer {
        fn name(&self) -> &str {
            self.inner.name()
        }

        async fn list(&self) -> Result<Vec<Entry>, Error> {
            self.inner.list().await
        }

        async fn entry(&self, id: EntryId) -> Result<Option<Dated>, Error> {
            self.inner.entry(id).await
        }

        async fn write(&self, write: &Write) -> Result<(), Error> {
            self.inner.write(write).await?;
            if self.lost.swap(false, Ordering::SeqCst) {
                return Err(Error::Timeout { deadline: DEADLINE });
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_write_whose_answer_was_lost_is_sent_again_as_it_was_kept() {
        // Films seen again count one more rewatch: deciding again from the
        // list the lost write changed would count two.
        let film = a_film(ENTRY, 3);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&film));
        record(&store, show_of(&store, LIST, ENTRY), 1);
        let backend = LosesTheFirstAnswer {
            inner: anilist(vec![film], Vec::new()),
            lost: AtomicBool::new(true),
        };
        let sender = a_sender(LIST, &store, rules(""));
        let told = Told::default();

        let lost = sender.pass(&backend, &|event| told.tell(event)).await;
        let kept = decided(&store, LIST, 1).expect("the write is kept");
        let again = sender.pass(&backend, &|event| told.tell(event)).await;

        assert!(
            matches!(lost.stopped, Some((Stop::Failed { .. }, 1))),
            "{lost:?}"
        );
        assert_eq!(kept.rewatches, 4);
        assert_eq!(again.tally.sent, 1);
        let written = held(&backend.inner, ENTRY)
            .await
            .expect("the list holds it");
        assert_eq!(written.entry.rewatches, 4);
        assert!(
            matches!(
                told.events().as_slice(),
                [BusEvent::Sent { again: true, before: None, after, .. }] if after.rewatches == 4
            ),
            "{:?}",
            told.events()
        );
        assert_eq!(decided(&store, LIST, 1), None);
        assert_eq!(pending(&store, LIST), Vec::<i64>::new());
    }

    #[tokio::test]
    async fn an_entry_the_service_no_longer_holds_is_set_aside_and_not_added_again() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 3);
        let backend = anilist(Vec::new(), vec![entry]);
        let told = Told::default();

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(passed.tally.aside, 1);
        assert_eq!(
            told.events(),
            [BusEvent::SetAside {
                list: "AniList".to_owned(),
                id: 1,
                watched: Some(watched(3)),
                to: Some(of_the_entry(3)),
                why: Unsent::Departed,
            }]
        );
        assert_eq!(held(&backend, ENTRY).await, None);
        let aside = store
            .lock()
            .expect("the store")
            .aside(LIST)
            .expect("the operations set aside read");
        assert_eq!(aside.len(), 1);
        assert_eq!(aside[0].reason, "no longer on the list, though it was");
    }

    #[tokio::test]
    async fn an_episode_that_goes_to_no_entry_is_set_aside_with_why() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 14);
        let backend = anilist(vec![entry], Vec::new());
        let told = Told::default();

        a_sender(LIST, &store, rules(""))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(
            told.events(),
            [BusEvent::SetAside {
                list: "AniList".to_owned(),
                id: 1,
                watched: Some(watched(14)),
                to: None,
                why: Unsent::Unresolved(Unresolved::Outside { episodes: Some(12) }),
            }]
        );
        let aside = store
            .lock()
            .expect("the store")
            .aside(LIST)
            .expect("the operations set aside read");
        assert_eq!(
            aside[0].reason,
            "entry has 12 episodes, no rule takes episode 14"
        );
    }

    #[tokio::test]
    async fn a_rule_that_sends_an_episode_to_an_entry_no_list_holds_adds_it() {
        let entry = an_entry(ENTRY, Some(12), Status::Completed, 12);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 14);
        let backend = anilist(
            vec![entry],
            vec![an_entry(SECOND, Some(12), Status::Planned, 0)],
        );
        let told = Told::default();

        let passed = a_sender(LIST, &store, rules("- ?|?|12345:13-24 -> ?|?|900001:1-12"))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(passed.tally.sent, 1);
        let after = standing(Status::Watching, 2, 0, Some(november(1)), None);
        assert_eq!(
            told.events(),
            [BusEvent::Sent {
                list: "AniList".to_owned(),
                id: 1,
                watched: watched(14),
                to: Destination {
                    entry: SECOND,
                    episode: 2,
                    by: By::Rule,
                    title: None,
                    episodes: None,
                    listed: false,
                },
                before: None,
                after,
                again: false,
            }]
        );
        let added = held(&backend, SECOND).await.expect("the list holds it now");
        assert_eq!(super::standing(&added), after);
    }

    #[tokio::test]
    async fn a_write_to_a_number_with_no_work_is_set_aside() {
        let entry = an_entry(ENTRY, Some(12), Status::Completed, 12);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 14);
        let backend = anilist(vec![entry], Vec::new());
        let told = Told::default();

        let passed = a_sender(LIST, &store, rules("- ?|?|12345:13-24 -> ?|?|900001:1-12"))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(passed.tally.aside, 1);
        assert!(
            matches!(
                told.events().as_slice(),
                [BusEvent::SetAside { why: Unsent::NoWork, to: Some(to), .. }] if to.entry == SECOND
            ),
            "{:?}",
            told.events()
        );
        assert_eq!(decided(&store, LIST, 1), None);
        assert_eq!(pending(&store, LIST), Vec::<i64>::new());
    }

    #[tokio::test]
    async fn operations_of_one_entry_are_sent_in_order_each_from_what_the_service_holds() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        let show = show_of(&store, LIST, ENTRY);
        record(&store, show, 3);
        record(&store, show, 4);
        let backend = anilist(vec![entry], Vec::new());
        let told = Told::default();

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|event| told.tell(event))
            .await;

        assert_eq!(passed.tally.sent, 2);
        let befores: Vec<Option<u32>> = told
            .events()
            .iter()
            .map(|event| match event {
                BusEvent::Sent { before, .. } => before.map(|before| before.progress),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(befores, [Some(2), Some(3)]);
        let written = held(&backend, ENTRY).await.expect("the list holds it");
        assert_eq!(written.entry.progress, 4);
        assert_eq!(written.started, Some(november(1)));
    }

    #[tokio::test]
    async fn the_count_of_episodes_the_service_holds_decides_the_last() {
        // The store's copy is from the last reading, and the service has
        // given the work a thirteenth episode since.
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 11);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 12);
        let backend = anilist(
            vec![Entry {
                episodes: Some(13),
                ..entry
            }],
            Vec::new(),
        );

        a_sender(LIST, &store, rules(""))
            .pass(&backend, &|_| {})
            .await;

        let written = held(&backend, ENTRY).await.expect("the list holds it");
        assert_eq!(
            super::standing(&written),
            standing(Status::Watching, 12, 0, Some(november(1)), None)
        );
    }

    #[tokio::test]
    async fn a_start_day_the_service_holds_is_kept() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 3);
        let backend = anilist(vec![entry], Vec::new());
        let begun = Day::new(2026, 9, 2);
        backend
            .write(&Write {
                id: ENTRY,
                status: Status::Watching,
                progress: 2,
                rewatches: 0,
                started: Some(begun),
                completed: None,
            })
            .await
            .expect("the start day is written");

        a_sender(LIST, &store, rules(""))
            .pass(&backend, &|_| {})
            .await;

        let written = held(&backend, ENTRY).await.expect("the list holds it");
        assert_eq!(written.started, Some(begun));
        assert_eq!(written.entry.progress, 3);
    }

    #[tokio::test]
    async fn a_token_the_service_refuses_stops_the_pass_and_moves_nothing() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 3);
        let backend = anilist(vec![entry], Vec::new()).failing(Fault::Refused);

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|_| {})
            .await;

        assert_eq!(
            passed.stopped,
            Some((Stop::Refused("invalid token".to_owned()), 1))
        );
        assert_eq!(pending(&store, LIST), [1]);
        assert_eq!(decided(&store, LIST, 1), None);
    }

    #[tokio::test]
    async fn a_wait_the_service_asks_for_stops_the_pass_with_the_wait() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        record(&store, show_of(&store, LIST, ENTRY), 3);
        let backend =
            anilist(vec![entry], Vec::new()).failing(Fault::Wait(Duration::from_secs(30)));

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|_| {})
            .await;

        assert_eq!(
            passed.stopped,
            Some((
                Stop::Failed {
                    said: "AniList asked to wait 30s".to_owned(),
                    asked: Some(Duration::from_secs(30)),
                },
                1
            ))
        );
    }

    /// Pin episodes 13 to 24 of the show of [`ENTRY`] to episodes 1 to 12 of
    /// [`SECOND`].
    fn pin_the_second_season(store: &Arc<Mutex<Store>>) {
        let show = show_of(store, LIST, ENTRY);
        let run = |first, last| Episodes::new(first, last).expect("a run");
        store
            .lock()
            .expect("the store")
            .pin(
                LIST,
                &Pin::new(show, run(13, 24), SECOND, run(1, 12)).expect("a pin"),
            )
            .expect("the pin is kept");
    }

    /// The list holding [`ENTRY`] completed, and [`SECOND`] planned.
    fn two_seasons() -> Vec<Entry> {
        vec![
            an_entry(ENTRY, Some(12), Status::Completed, 12),
            an_entry(SECOND, Some(12), Status::Planned, 0),
        ]
    }

    /// The reasons of the operations set aside, lowest first.
    fn reasons(store: &Arc<Mutex<Store>>) -> Vec<String> {
        store
            .lock()
            .expect("the store")
            .aside(LIST)
            .expect("the operations set aside read")
            .into_iter()
            .map(|aside| aside.reason)
            .collect()
    }

    #[tokio::test]
    async fn what_was_set_aside_is_sent_once_a_pin_takes_it() {
        let (_home, store) = a_store(LIST, &two_seasons());
        record(&store, show_of(&store, LIST, ENTRY), 14);
        let backend = anilist(two_seasons(), Vec::new());
        let sender = a_sender(LIST, &store, rules(""));
        sender.pass(&backend, &|_| {}).await;
        pin_the_second_season(&store);
        let told = Told::default();

        let retried = sender.retry(&backend, &|event| told.tell(event)).await;

        assert_eq!(
            retried,
            Passed {
                tally: Tally {
                    sent: 1,
                    ..Tally::default()
                },
                stopped: None,
            }
        );
        assert!(
            matches!(
                told.events().as_slice(),
                [BusEvent::Sent { id: 1, to, .. }] if to.entry == SECOND && to.by == By::Pin
            ),
            "{:?}",
            told.events()
        );
        assert_eq!(reasons(&store), Vec::<String>::new());
        let written = held(&backend, SECOND).await.expect("the list holds it");
        assert_eq!(written.entry.progress, 2);
    }

    #[tokio::test]
    async fn what_was_set_aside_and_goes_to_no_entry_still_is_told_of_no_more() {
        let (_home, store) = a_store(LIST, &two_seasons());
        record(&store, show_of(&store, LIST, ENTRY), 14);
        let backend = anilist(two_seasons(), Vec::new());
        let sender = a_sender(LIST, &store, rules(""));
        sender.pass(&backend, &|_| {}).await;
        let told = Told::default();

        let retried = sender.retry(&backend, &|event| told.tell(event)).await;

        assert_eq!(
            retried,
            Passed {
                tally: Tally::default(),
                stopped: None,
            }
        );
        assert_eq!(told.events(), Vec::<BusEvent>::new());
        assert_eq!(
            reasons(&store),
            ["entry has 12 episodes, no rule takes episode 14"]
        );
    }

    #[tokio::test]
    async fn a_retry_the_service_fails_for_stops_and_what_was_set_aside_stays_aside() {
        let (_home, store) = a_store(LIST, &two_seasons());
        let show = show_of(&store, LIST, ENTRY);
        record(&store, show, 14);
        record(&store, show, 15);
        let backend = anilist(two_seasons(), Vec::new());
        a_sender(LIST, &store, rules(""))
            .pass(&backend, &|_| {})
            .await;
        pin_the_second_season(&store);
        let down = anilist(two_seasons(), Vec::new()).failing(Fault::Unreachable);

        let retried = a_sender(LIST, &store, rules(""))
            .retry(&down, &|_| {})
            .await;

        assert!(
            matches!(&retried.stopped, Some((Stop::Failed { .. }, 2))),
            "{retried:?}"
        );
        assert_eq!(reasons(&store).len(), 2);
    }

    #[tokio::test]
    async fn nothing_past_the_cursor_is_a_pass_that_does_nothing() {
        let entry = an_entry(ENTRY, Some(12), Status::Watching, 2);
        let (_home, store) = a_store(LIST, std::slice::from_ref(&entry));
        let backend = anilist(vec![entry], Vec::new()).failing(Fault::Unreachable);

        let passed = a_sender(LIST, &store, rules(""))
            .pass(&backend, &|_| {})
            .await;

        assert_eq!(
            passed,
            Passed {
                tally: Tally::default(),
                stopped: None,
            }
        );
    }

    /// AniList in memory as an account: the token it finds, and whether its
    /// service refuses the token or cannot be reached.
    struct InMemory {
        token: Option<Token>,
        reached: Reached,
    }

    /// The most entries a test here asks for. Past it the backend answers no
    /// more, so that a task trying again at once lets the paused clock move.
    const FUSE: usize = 50;

    /// The backend of [`InMemory`], keeping when each entry was asked for.
    #[derive(Clone)]
    struct Reached {
        list: Arc<Memory>,
        down: Arc<AtomicBool>,
        refusing: Arc<AtomicBool>,
        waits: Arc<Mutex<Option<Duration>>>,
        asked: Arc<Mutex<Vec<Instant>>>,
    }

    impl Reached {
        fn failing(&self) -> Option<Error> {
            let wait = self
                .waits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if self.refusing.load(Ordering::SeqCst) {
                Some(Error::Refused("invalid token".to_owned()))
            } else if let Some(after) = wait {
                Some(Error::Wait { after })
            } else if self.down.load(Ordering::SeqCst) {
                Some(Error::Unreachable("connection refused".into()))
            } else {
                None
            }
        }

        /// Ask for a wait of `after` at the next call, once.
        fn wait_once(&self, after: Duration) {
            *self.waits.lock().unwrap_or_else(PoisonError::into_inner) = Some(after);
        }

        /// When each entry was asked for, in order.
        fn asked(&self) -> Vec<Instant> {
            self.asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl ListBackend for Reached {
        fn name(&self) -> &str {
            self.list.name()
        }

        async fn list(&self) -> Result<Vec<Entry>, Error> {
            self.list.list().await
        }

        async fn entry(&self, id: EntryId) -> Result<Option<Dated>, Error> {
            let asked = {
                let mut asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
                asked.push(Instant::now());
                asked.len()
            };
            if asked > FUSE {
                std::future::pending::<()>().await;
            }
            match self.failing() {
                Some(failed) => Err(failed),
                None => self.list.entry(id).await,
            }
        }

        async fn write(&self, write: &Write) -> Result<(), Error> {
            match self.failing() {
                Some(failed) => Err(failed),
                None => self.list.write(write).await,
            }
        }
    }

    impl Account for InMemory {
        type Backend = Reached;

        fn token(&self) -> impl Future<Output = Result<Option<Token>, secrets::Error>> + Send {
            std::future::ready(Ok(self.token.clone()))
        }

        fn backend(&self, _token: &Token) -> Result<Reached, Error> {
            Ok(self.reached.clone())
        }
    }

    /// One sending task running over a store of its own, the list holding
    /// [`ENTRY`] at 2 of 12 episodes.
    struct Running {
        home: TempDir,
        store: Arc<Mutex<Store>>,
        reached: Reached,
        list: Arc<Memory>,
        wakes: Wakes,
        now: SendNow,
        said: Arc<Mutex<Vec<String>>>,
        events: broadcast::Receiver<BusEvent>,
        task: tokio::task::JoinHandle<Result<(), TaskError>>,
    }

    impl Running {
        /// Record a viewing of this episode of the show of [`ENTRY`].
        fn record(&self, episode: u32) {
            record(&self.store, show_of(&self.store, LIST, ENTRY), episode);
        }

        /// The next event told on the bus, within an hour.
        async fn next_event(&mut self) -> BusEvent {
            tokio::time::timeout(Duration::from_hours(1), self.events.recv())
                .await
                .expect("an event is told within an hour")
                .expect("the bus is open")
        }

        /// Wait until the service was asked for an entry `times` times, for
        /// two hours at most.
        async fn asked_for(&self, times: usize) {
            tokio::time::timeout(Duration::from_hours(2), async {
                while self.reached.asked().len() < times {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
            .await
            .expect("the service is asked often enough within two hours");
        }

        /// Stop the task, and start another over the same store and list at the
        /// times `sending` names.
        fn restart(self, sending: Sending) -> Self {
            self.task.abort();
            start(
                self.home,
                self.store,
                self.list,
                self.reached,
                sending,
                Some(a_token()),
            )
        }

        /// Every line said so far.
        fn said(&self) -> Vec<String> {
            self.said
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    /// Run a sending task at the times `sending` names, with `token` found,
    /// its viewings of episode `recorded` in the log before it starts.
    fn running(sending: Sending, token: Option<Token>, recorded: &[u32]) -> Running {
        let listed = vec![
            an_entry(ENTRY, Some(12), Status::Watching, 2),
            an_entry(SECOND, Some(12), Status::Planned, 0),
        ];
        let (home, store) = a_store(LIST, &listed);
        for episode in recorded {
            record(&store, show_of(&store, LIST, ENTRY), *episode);
        }
        let list = Arc::new(anilist(listed, Vec::new()));
        let reached = Reached {
            list: Arc::clone(&list),
            down: Arc::default(),
            refusing: Arc::default(),
            waits: Arc::default(),
            asked: Arc::default(),
        };
        start(home, store, list, reached, sending, token)
    }

    /// Start a sending task over `store`, sending to `list` through `reached`.
    fn start(
        home: TempDir,
        store: Arc<Mutex<Store>>,
        list: Arc<Memory>,
        reached: Reached,
        sending: Sending,
        token: Option<Token>,
    ) -> Running {
        let bus = Arc::new(EventBus::new());
        let events = bus.subscribe();
        let wakes = Wakes::new();
        let (now, asked) = send_now();
        let said = Arc::new(Mutex::new(Vec::new()));
        let saying = Arc::clone(&said);
        let task = Arc::new(SendTask::new(
            InMemory {
                token,
                reached: reached.clone(),
            },
            a_sender(LIST, &store, rules("")),
            sending,
            bus,
            wakes.clone(),
            asked,
            Arc::new(move |line: &str| {
                saying
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(line.to_owned());
            }),
        ));
        let running = tokio::spawn(async move { task.run().await });

        Running {
            home,
            store,
            reached,
            list,
            wakes,
            now,
            said,
            events,
            task: running,
        }
    }

    fn a_token() -> Token {
        Token::new("a token")
    }

    const EACH: Sending = Sending {
        when: When::Each,
        interval: Duration::from_mins(15),
        at_start: false,
        at_stop: false,
    };

    /// What a pass asked for at once tells, up to what it came to.
    async fn heard(now: &SendNow) -> Vec<Heard> {
        let mut hearing = now.ask().expect("the task takes asks");
        let mut heard = Vec::new();
        while let Some(told) = tokio::time::timeout(Duration::from_hours(1), hearing.recv())
            .await
            .expect("the pass ends within an hour")
        {
            let ended = matches!(told, Heard::Ended(_));
            heard.push(told);
            if ended {
                break;
            }
        }
        heard
    }

    #[tokio::test(start_paused = true)]
    async fn with_each_a_viewing_is_sent_as_it_is_recorded() {
        let mut running = running(EACH, Some(a_token()), &[]);

        running.record(3);
        running.wakes.recorded();

        assert!(
            matches!(running.next_event().await, BusEvent::Sent { id: 1, .. }),
            "the viewing is sent"
        );
        let written = held(running.list.as_ref(), ENTRY)
            .await
            .expect("it is held");
        assert_eq!(written.entry.progress, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn with_manual_a_viewing_waits_for_benshi_sync_now() {
        let running = running(
            Sending {
                when: When::Manual,
                ..EACH
            },
            Some(a_token()),
            &[],
        );
        running.record(3);
        running.wakes.recorded();

        tokio::time::sleep(Duration::from_hours(2)).await;
        let waited = running.reached.asked();
        let heard = heard(&running.now).await;

        assert_eq!(waited, Vec::<Instant>::new());
        assert!(
            matches!(
                heard.as_slice(),
                [
                    Heard::Event(BusEvent::Sent { id: 1, .. }),
                    Heard::Ended(Ending::Done(Tally { sent: 1, .. }))
                ]
            ),
            "{heard:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn at_start_what_waits_is_sent() {
        let mut running = running(
            Sending {
                at_start: true,
                ..EACH
            },
            Some(a_token()),
            &[3],
        );

        assert!(
            matches!(running.next_event().await, BusEvent::Sent { id: 1, .. }),
            "what waited is sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_is_tried_again_after_a_minute_and_twice_as_long_each_time() {
        let mut running = running(EACH, Some(a_token()), &[]);
        running.reached.down.store(true, Ordering::SeqCst);
        running.record(3);
        running.wakes.recorded();

        running.asked_for(3).await;
        running.reached.down.store(false, Ordering::SeqCst);
        let sent = running.next_event().await;

        let asked = running.reached.asked();
        let between: Vec<Duration> = asked.windows(2).map(|two| two[1] - two[0]).collect();
        assert_eq!(
            between,
            [
                Duration::from_mins(1),
                Duration::from_mins(2),
                Duration::from_mins(4)
            ]
        );
        assert!(matches!(sent, BusEvent::Sent { .. }), "{sent:?}");
        assert_eq!(
            running.said(),
            [
                "AniList could not be reached: connection refused; 1 operation waits, tried again \
              in 1m"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_the_service_asks_for_is_waited_and_grows_no_wait_after_it() {
        let mut running = running(EACH, Some(a_token()), &[]);
        running.reached.wait_once(Duration::from_secs(90));
        running.reached.down.store(true, Ordering::SeqCst);
        running.record(3);
        running.wakes.recorded();

        running.asked_for(3).await;
        running.reached.down.store(false, Ordering::SeqCst);
        running.next_event().await;

        let asked = running.reached.asked();
        let between: Vec<Duration> = asked.windows(2).map(|two| two[1] - two[0]).collect();
        assert_eq!(
            between,
            [
                Duration::from_secs(90),
                Duration::from_mins(1),
                Duration::from_mins(2)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_token_waits_for_a_login() {
        let mut running = running(EACH, Some(a_token()), &[]);
        running.reached.refusing.store(true, Ordering::SeqCst);
        running.record(3);
        running.wakes.recorded();

        tokio::time::sleep(Duration::from_hours(3)).await;
        running.record(4);
        running.wakes.recorded();
        tokio::time::sleep(Duration::from_hours(1)).await;
        let before_the_login = running.reached.asked().len();
        running.reached.refusing.store(false, Ordering::SeqCst);
        running.wakes.logged_in();
        let sent = running.next_event().await;

        assert_eq!(before_the_login, 1);
        assert!(matches!(sent, BusEvent::Sent { id: 1, .. }), "{sent:?}");
        assert_eq!(
            running.said(),
            ["AniList refused the token: invalid token; 1 operation waits for benshi login"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_token_says_so_and_benshi_sync_now_hears_why() {
        let running = running(
            Sending {
                at_start: true,
                ..EACH
            },
            None,
            &[3],
        );

        let heard = heard(&running.now).await;

        assert_eq!(heard, [Heard::Ended(Ending::NoToken(NO_TOKEN.to_owned()))]);
        assert_eq!(running.said(), [NO_TOKEN]);
        assert_eq!(running.reached.asked(), Vec::<Instant>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn benshi_sync_now_is_attempted_at_once_while_a_failure_waits() {
        let running = running(EACH, Some(a_token()), &[]);
        running.reached.down.store(true, Ordering::SeqCst);
        running.record(3);
        running.wakes.recorded();
        running.asked_for(1).await;
        let failed = running.reached.asked()[0];
        tokio::time::sleep(Duration::from_secs(10)).await;

        let heard = heard(&running.now).await;

        let asked = running.reached.asked();
        assert_eq!(asked.len(), 2);
        assert!(asked[1] - failed < Duration::from_mins(1), "{asked:?}");
        assert_eq!(
            heard,
            [Heard::Ended(Ending::Stopped {
                tally: Tally::default(),
                line: "AniList could not be reached: connection refused; 1 operation waits, \
                       tried again in 2m"
                    .to_owned(),
            })]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_wake_that_what_was_set_aside_may_go_somewhere_tries_it_again() {
        let mut running = running(EACH, Some(a_token()), &[]);
        running.record(14);
        running.wakes.recorded();
        let set_aside = running.next_event().await;
        pin_the_second_season(&running.store);

        running.wakes.resolvable();

        assert!(
            matches!(set_aside, BusEvent::SetAside { id: 1, .. }),
            "{set_aside:?}"
        );
        assert!(
            matches!(running.next_event().await, BusEvent::Sent { id: 1, .. }),
            "what was set aside is sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn benshi_sync_now_tries_again_what_was_set_aside() {
        let mut running = running(EACH, Some(a_token()), &[]);
        running.record(14);
        running.wakes.recorded();
        running.next_event().await;
        pin_the_second_season(&running.store);

        let heard = heard(&running.now).await;

        assert!(
            matches!(
                heard.as_slice(),
                [
                    Heard::Event(BusEvent::Sent { id: 1, .. }),
                    Heard::Ended(Ending::Done(Tally { sent: 1, .. }))
                ]
            ),
            "{heard:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn at_start_what_was_set_aside_is_tried_again() {
        let mut running = running(EACH, Some(a_token()), &[]);
        running.record(14);
        running.wakes.recorded();
        running.next_event().await;
        pin_the_second_season(&running.store);

        let mut again = running.restart(Sending {
            at_start: true,
            ..EACH
        });

        assert!(
            matches!(again.next_event().await, BusEvent::Sent { id: 1, .. }),
            "what was set aside is sent at start"
        );
    }

    /// Record a viewing that is set aside, and then pin it to [`SECOND`].
    async fn set_aside_and_pinned(running: &mut Running) {
        running.record(14);
        running.wakes.recorded();
        let set_aside = running.next_event().await;
        assert!(
            matches!(set_aside, BusEvent::SetAside { id: 1, .. }),
            "{set_aside:?}"
        );
        pin_the_second_season(&running.store);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_that_fails_counts_what_waits_past_the_cursor_alone() {
        let mut running = running(EACH, Some(a_token()), &[]);
        set_aside_and_pinned(&mut running).await;
        running.reached.down.store(true, Ordering::SeqCst);

        let heard = heard(&running.now).await;

        assert_eq!(
            heard,
            [Heard::Ended(Ending::Stopped {
                tally: Tally::default(),
                line: "AniList could not be reached: connection refused; tried again in 1m"
                    .to_owned(),
            })]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_refused_with_nothing_past_the_cursor_says_nothing_is_sent() {
        let mut running = running(EACH, Some(a_token()), &[]);
        set_aside_and_pinned(&mut running).await;
        running.reached.refusing.store(true, Ordering::SeqCst);

        let heard = heard(&running.now).await;

        assert_eq!(
            heard,
            [Heard::Ended(Ending::Stopped {
                tally: Tally::default(),
                line: "AniList refused the token: invalid token; nothing is sent before benshi \
                       login"
                    .to_owned(),
            })]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn close_and_interval_send_each_viewing_until_they_are_built() {
        for when in [When::Close, When::Interval] {
            let mut running = running(Sending { when, ..EACH }, Some(a_token()), &[]);

            running.record(3);
            running.wakes.recorded();

            assert!(
                matches!(running.next_event().await, BusEvent::Sent { .. }),
                "{when:?}"
            );
            assert_eq!(
                running.said(),
                [
                    "send when \"close\" and \"interval\" are not built yet: each viewing is sent \
                  as it is recorded"
                ],
                "{when:?}"
            );
        }
    }
}
