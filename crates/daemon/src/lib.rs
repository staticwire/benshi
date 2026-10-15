//! Wiring, supervision, the event bus, the IPC server and configuration.
//!
//! **The event bus is the log.** `benshi watch` subscribes to exactly the
//! stream the sinks receive, so the ad-hoc debug log that a tracker always ends
//! up needing exists as a product feature and therefore cannot rot.
//!
//! Errors come in three classes and no fourth. *Transient* - a network fault, a
//! D-Bus timeout, a broken pipe - is retried with backoff. *Permanent* - an
//! expired token, an unresolvable mapping, bad configuration - stops that
//! component and says so. *Bugs* panic; a panic in one task does not take the
//! process down.
//!
//! The supervisor **owns each task's handle and observes its exit**. That is
//! what makes it impossible to bypass: a restart limiter that an exception can
//! escape past is not a limiter.

use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use benshi_core::policy::PolicyTable;
use benshi_detect::{PlayerWatcher, WatchError};
use benshi_store::Store;
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::accounts::Accounts;
use crate::bus::EventBus;
use crate::detection::{Detection, Seen, Wiring};
#[cfg(unix)]
use crate::ipc::Server;
use crate::listing::{Listing, Reader, Source, Wakes};
use crate::recognition::{Current, Decided, Recogniser};
#[cfg(unix)]
use crate::supervisor::{Notice, Supervisor, TaskError, TaskRecord};

pub mod accounts;

pub mod bus;

pub mod config;

pub mod detection;

#[cfg(unix)]
pub mod ipc;

pub mod listing;

pub mod panics;

pub mod paths;

pub mod protocol;

pub mod recognition;

pub mod secrets;

pub mod supervisor;

pub mod voice;

/// Run the daemon until every one of its tasks has stopped.
///
/// Built where the socket is, because a daemon with no interface is not one:
/// nothing could ask it what it had seen. The platform that needs a named
/// pipe instead gains its own.
///
/// Three supervised tasks over state they share: detection, which reads the
/// platform and publishes what it saw, the socket, which answers clients
/// from it, and the list's task, which reads the list and puts what it read
/// in place of what detection decides against. They are separate tasks on
/// purpose. A session bus that goes away must not take the interface down
/// with it, because the user whose detection has broken is exactly the user
/// who needs to ask the daemon what it thinks is happening.
///
/// `start_watcher` builds a watcher rather than being handed one, because the
/// supervisor calls a task's body again on every restart. A restart that
/// reused the watcher the first attempt built would restart the loop around a
/// connection that is already dead, and the task would fail for ever while
/// looking supervised.
///
/// The listener is bound by the caller and shared rather than taken. Binding
/// can fail for a reason a person has to act on - another daemon is already
/// listening there - and that answer belongs before anything starts, rather
/// than inside a task that would quietly retry it.
///
/// The store is opened by the caller for the same reason, and it outlives a
/// restart of detection: a detection task that is started again knows nothing
/// of the one before it and goes on from what that one kept. `recogniser` is
/// what a file's name is decided against until the list's task puts a list
/// in its place, and `now` reads the time of day the store writes beside what
/// it keeps. `accounts` carries out a login and a
/// logout the socket is asked for, and `list` says what the list's task reads
/// from and when.
///
/// `tell` is told of a task as the task is started again and as it stops,
/// whatever the other tasks are doing. A daemon whose detection has stopped
/// goes on answering its socket, and nothing else says that it records
/// nothing. The supervisor deals with no task that ended until `tell` has
/// returned, so a `tell` that writes hands the line to a
/// [`Voice`](voice::Voice).
///
/// Returns once no task is left running, which for a daemon means every one of
/// them stopped permanently. What stopped each is in its [`TaskRecord`], and
/// `tell` has been told of each by then.
#[expect(
    clippy::too_many_arguments,
    reason = "each part is built by the caller, so that a test can give its own"
)]
pub async fn run<W, B, F, T, S>(
    mut start_watcher: B,
    listener: Arc<UnixListener>,
    recogniser: Recogniser,
    store: Arc<Mutex<Store>>,
    now: fn() -> SystemTime,
    period: Duration,
    accounts: Arc<dyn Accounts>,
    list: Listing<S>,
    tell: T,
) -> Vec<TaskRecord>
where
    B: FnMut() -> F + Send + 'static,
    F: Future<Output = Result<W, WatchError>> + Send + 'static,
    W: PlayerWatcher + Send + 'static,
    T: FnMut(&Notice) + Send + 'static,
    S: Source + Send + Sync + 'static,
{
    let wiring = Wiring {
        bus: Arc::new(EventBus::new()),
        policy: Arc::new(RwLock::new(PolicyTable::allowing_video_players())),
        seen: Seen::new(),
        recogniser: Current::new(recogniser),
        wakes: Wakes::new(),
        decided: Decided::new(),
        store,
        now,
        period,
    };
    let server = Arc::new(Server::new(
        Arc::clone(&wiring.bus),
        Arc::clone(&wiring.policy),
        wiring.seen.clone(),
        wiring.decided.clone(),
        accounts,
        wiring.wakes.clone(),
    ));
    let reader = Arc::new(Reader::new(
        list.source,
        list.reading,
        Arc::clone(&wiring.store),
        wiring.recogniser.clone(),
        wiring.wakes.clone(),
        list.say,
    ));

    let mut supervisor = Supervisor::telling(tell);

    supervisor.supervise("detection", move || {
        let wiring = wiring.clone();
        // Called here rather than awaited here: the body must return a future,
        // and this call is what a restart repeats.
        let building = start_watcher();

        async move {
            let watcher = building
                .await
                .map_err(|unreachable| TaskError::Transient(unreachable.into()))?;

            Detection::new(watcher, wiring).run().await
        }
    });

    supervisor.supervise("socket", move || {
        let server = Arc::clone(&server);
        let listener = Arc::clone(&listener);

        async move { server.listen(listener).await }
    });

    supervisor.supervise("list", move || {
        let reader = Arc::clone(&reader);

        async move { reader.run().await }
    });

    supervisor.run().await
}
