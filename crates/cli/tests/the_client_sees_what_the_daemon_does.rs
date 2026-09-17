//! The client against a daemon that is actually detecting.
//!
//! Every other test in this crate gives the daemon a listing to hold. This one
//! makes the detection task produce it, so the seam between the two is covered:
//! a round writes what it saw, a connection reads it, and the client prints it.
//!
//! The platform is a fake. What is under test is the wiring, and a real one
//! would make the run depend on what happens to be playing.

#![cfg(unix)]
// A fake answers from memory, which is the whole point of a fake, and the trait
// asks for a future either way. The `async` therefore stays where there is
// nothing to await, rather than being spelled out as the future it desugars to.
#![allow(clippy::unused_async_trait_impl)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use benshi_cli::commands::{Command, PolicyArgument, run};
use benshi_core::clock::Timestamp;
use benshi_core::path::RawPath;
use benshi_core::policy::PolicyTable;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use benshi_daemon::bus::{BusEvent, EventBus};
use benshi_daemon::detection::{Detection, Seen};
use benshi_daemon::ipc::{Server, bind};
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;
use tokio::time::timeout;

/// How long a test waits before calling something lost.
const PATIENCE: Duration = Duration::from_secs(5);

/// Short, because nothing here waits on wall-clock time on purpose.
const PERIOD: Duration = Duration::from_millis(5);

/// The identity the fake platform reports.
const PLAYER: &str = "mpv.instance1701";

/// A platform with one player on it, which counts how often it was read.
///
/// The count is what lets a test say "several more rounds have happened"
/// without sleeping for them, which is the difference between a test that is
/// deterministic and one that usually passes.
#[derive(Clone, Default)]
struct OnePlayer(Arc<AtomicUsize>);

impl OnePlayer {
    fn source() -> SourceInfo {
        SourceInfo {
            player: PlayerId(PLAYER.to_owned()),
            app: AppName("mpv".to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    fn rounds(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Wait until at least `wanted` rounds have been taken.
    async fn after(&self, wanted: usize) {
        timeout(PATIENCE, async {
            while self.rounds() < wanted {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the detection task stopped at round {}", self.rounds()));
    }
}

impl PlayerWatcher for OnePlayer {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        Ok(vec![Self::source()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        let source = Self::source();

        Ok(PollOutcome {
            snapshots: vec![PlayerSnapshot {
                player: source.player.clone(),
                media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
                state: source.state,
                position: Known::Value(Duration::from_secs(5)),
                duration: Known::Value(Duration::from_mins(23)),
                observed_at: Timestamp::epoch(),
            }],
            sources: vec![source],
            failures: Vec::new(),
        })
    }
}

/// A daemon detecting through `platform` and answering on a socket of its own.
struct Daemon {
    _home: TempDir,
    socket: PathBuf,
    bus: Arc<EventBus>,
    platform: OnePlayer,
}

impl Daemon {
    fn start() -> Self {
        let platform = OnePlayer::default();
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let seen = Seen::new();

        let mut detection = Detection::new(
            platform.clone(),
            Arc::clone(&bus),
            Arc::clone(&policy),
            seen.clone(),
            PERIOD,
        );
        drop(tokio::spawn(async move { detection.run().await }));

        let home = tempfile::tempdir().expect("a directory of our own");
        let socket = home.path().join("run").join("benshi.sock");
        let listener = bind(&socket).expect("the socket binds");
        let server = Arc::new(Server::new(Arc::clone(&bus), policy, seen));
        drop(tokio::spawn(server.listen(Arc::new(listener))));

        Self {
            _home: home,
            socket,
            bus,
            platform,
        }
    }

    async fn ask(&self, command: Command) -> (ExitCode, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(command, &self.socket, &mut out, &mut err).await;

        assert!(
            err.is_empty(),
            "the client complained: {}",
            String::from_utf8_lossy(&err)
        );

        (outcome, String::from_utf8(out).expect("valid utf-8"))
    }
}

/// Every reading on the bus so far, drained without waiting.
fn readings(events: &mut Receiver<BusEvent>) -> Vec<PlayerId> {
    let mut seen = Vec::new();

    while let Ok(event) = events.try_recv() {
        if let BusEvent::Snapshot(snapshot) = event {
            seen.push(snapshot.player);
        }
    }

    seen
}

#[tokio::test]
async fn a_listing_shows_the_source_a_round_saw() {
    let daemon = Daemon::start();
    daemon.platform.after(1).await;

    let (outcome, shown) = daemon.ask(Command::Sources).await;

    assert_eq!(outcome, ExitCode::SUCCESS);
    assert!(shown.contains(PLAYER), "{shown}");
    assert!(shown.contains("auto"), "{shown}");
    assert!(shown.contains("pos+dur+pause+loc"), "{shown}");
}

#[tokio::test]
async fn a_deny_typed_at_the_client_silences_the_source_without_a_restart() {
    // The whole chain in one test: the client writes a policy over the socket,
    // the detection task reads the same table on its next round, and the
    // readings stop reaching the bus. Nothing is restarted in between.
    let daemon = Daemon::start();
    let mut events = daemon.bus.subscribe();
    daemon.platform.after(1).await;

    timeout(PATIENCE, async {
        while readings(&mut events).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the source was published before the deny");

    let (outcome, _shown) = daemon
        .ask(Command::Policy {
            app: "mpv".to_owned(),
            policy: PolicyArgument::Deny,
        })
        .await;
    assert_eq!(outcome, ExitCode::SUCCESS);

    // Drain whatever was in flight, then let several rounds pass. Counting the
    // rounds rather than sleeping for them is what makes this deterministic.
    let _in_flight = readings(&mut events);
    let quiet_from = daemon.platform.rounds();
    daemon.platform.after(quiet_from + 5).await;

    assert!(
        readings(&mut events).is_empty(),
        "a denied source kept publishing after the policy changed"
    );
}
