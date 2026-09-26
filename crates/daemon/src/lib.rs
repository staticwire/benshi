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
use std::sync::{Arc, RwLock};
use std::time::Duration;

use benshi_core::policy::PolicyTable;
use benshi_detect::{PlayerWatcher, WatchError};
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::bus::EventBus;
use crate::detection::{Detection, Seen};
#[cfg(unix)]
use crate::ipc::Server;
use crate::recognition::{Decided, Recogniser};
#[cfg(unix)]
use crate::supervisor::{Supervisor, TaskError, TaskRecord};

pub mod bus;

pub mod detection;

#[cfg(unix)]
pub mod ipc;

pub mod protocol;

pub mod recognition;

pub mod supervisor;

/// Run the daemon until every one of its tasks has stopped.
///
/// Built where the socket is, because a daemon with no interface is not one:
/// nothing could ask it what it had seen, and nothing could stop it short of
/// killing it. The platform that needs a named pipe instead gains its own.
///
/// Two supervised tasks over state they share: detection, which reads the
/// platform and publishes what it saw, and the socket, which answers clients
/// from it. They are separate tasks on purpose. A session bus that goes away
/// must not take the interface down with it, because the user whose detection
/// has broken is exactly the user who needs to ask the daemon what it thinks
/// is happening.
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
/// Returns once no task is left running, which for a daemon means every one of
/// them stopped permanently. What stopped each is in its [`TaskRecord`].
pub async fn run<W, B, F>(
    mut start_watcher: B,
    listener: Arc<UnixListener>,
    period: Duration,
) -> Vec<TaskRecord>
where
    B: FnMut() -> F + Send + 'static,
    F: Future<Output = Result<W, WatchError>> + Send + 'static,
    W: PlayerWatcher + Send + 'static,
{
    let bus = Arc::new(EventBus::new());
    let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
    let seen = Seen::new();
    let decided = Decided::new();
    // Nothing to match against until a list arrives, so every file is refused
    // and `benshi why` says so.
    let recogniser = Arc::new(Recogniser::empty());
    let server = Arc::new(Server::new(
        Arc::clone(&bus),
        Arc::clone(&policy),
        seen.clone(),
        decided.clone(),
    ));

    let mut supervisor = Supervisor::new();

    supervisor.supervise("detection", move || {
        let bus = Arc::clone(&bus);
        let policy = Arc::clone(&policy);
        let seen = seen.clone();
        let recogniser = Arc::clone(&recogniser);
        let decided = decided.clone();
        // Called here rather than awaited here: the body must return a future,
        // and this call is what a restart repeats.
        let building = start_watcher();

        async move {
            let watcher = building
                .await
                .map_err(|unreachable| TaskError::Transient(unreachable.into()))?;
            let mut detection =
                Detection::new(watcher, bus, policy, seen, recogniser, decided, period);

            detection.run().await
        }
    });

    supervisor.supervise("socket", move || {
        let server = Arc::clone(&server);
        let listener = Arc::clone(&listener);

        async move { server.listen(listener).await }
    });

    supervisor.run().await
}
