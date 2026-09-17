//! The whole daemon, assembled the way the binary assembles it.
//!
//! Every other test in this crate exercises one part against fakes of the
//! others. This one calls the function the binary calls, so that the wiring
//! itself is under test: whether detection and the socket were given the same
//! record of what was seen, whether one task failing takes the other down, and
//! whether a restart starts from a watcher of its own.
//!
//! The platform is a fake, because a real one would make the run depend on
//! what happens to be playing. Everything above it is the production article.

#![cfg(unix)]
// A fake answers from memory, which is the whole point of a fake, and the trait
// asks for a future either way. The `async` therefore stays where there is
// nothing to await, rather than being spelled out as the future it desugars to.
#![allow(clippy::unused_async_trait_impl)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use benshi_core::clock::Timestamp;
use benshi_core::path::RawPath;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use benshi_daemon::ipc::bind;
use benshi_daemon::protocol::{Request, Response, SourceListing};
use benshi_daemon::supervisor::RESTART_LIMIT;
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

/// How long a test waits before calling something lost.
///
/// Only ever spent on a failure. A daemon that never answers has to fail the
/// run rather than hang it.
const PATIENCE: Duration = Duration::from_secs(5);

/// Short, because nothing here waits on wall-clock time on purpose.
const PERIOD: Duration = Duration::from_millis(5);

/// The identity the fake platform reports.
const PLAYER: &str = "mpv.instance1701";

/// A platform with one player on it, counting how often a watcher was built.
///
/// The count is the point: the supervisor calls a task's body again on every
/// restart, and the body is what builds the watcher. A restart that reused the
/// first one would reconnect to nothing, which on a lost session bus is the
/// failure the restart exists to fix.
#[derive(Clone, Default)]
struct Platform {
    built: Arc<AtomicUsize>,
    fell_over: Arc<AtomicUsize>,
    fails: bool,
    panics: bool,
}

impl Platform {
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

    fn reading() -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId(PLAYER.to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/[Group] Show - 03.mkv".to_vec(),
            )),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(17)),
            duration: Known::Value(Duration::from_secs(1420)),
            observed_at: Timestamp::epoch(),
        }
    }

    /// How many watchers have been built so far.
    ///
    /// Counted where the body is called, which is when a restart is scheduled
    /// rather than when the attempt runs. Measured: waiting on this to decide
    /// that an attempt has *finished* is what made the first version of
    /// `detection_giving_up_for_good_leaves_the_socket_answering` pass a daemon
    /// whose two loops were one task.
    fn watchers(&self) -> usize {
        self.built.load(Ordering::SeqCst)
    }

    /// How many attempts have actually reached the platform and panicked.
    fn collapses(&self) -> usize {
        self.fell_over.load(Ordering::SeqCst)
    }

    /// A factory of watchers, as the daemon takes one.
    ///
    /// `use<>` because the daemon holds the factory for as long as it runs, so
    /// it must capture the count rather than borrow the platform it came from.
    fn factory(&self) -> impl FnMut() -> std::future::Ready<Result<Watcher, WatchError>> + use<> {
        let built = Arc::clone(&self.built);
        let fell_over = Arc::clone(&self.fell_over);
        let fails = self.fails;
        let panics = self.panics;

        move || {
            built.fetch_add(1, Ordering::SeqCst);
            let fell_over = Arc::clone(&fell_over);

            std::future::ready(Ok(Watcher {
                fell_over,
                fails,
                panics,
            }))
        }
    }
}

/// One built watcher.
struct Watcher {
    fell_over: Arc<AtomicUsize>,
    fails: bool,
    panics: bool,
}

impl Watcher {
    fn refuse() -> WatchError {
        WatchError::Unavailable(PlayerId(PLAYER.to_owned()))
    }

    /// Fail the way a bug fails, counting the attempt as it goes down.
    fn collapse(&self) {
        self.fell_over.fetch_add(1, Ordering::SeqCst);
        panic!("a platform that cannot be read at all");
    }
}

impl PlayerWatcher for Watcher {
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        if self.panics {
            self.collapse();
        }
        if self.fails {
            return Err(Self::refuse());
        }

        Ok(vec![Platform::source()])
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        if self.panics {
            self.collapse();
        }
        if self.fails {
            return Err(Self::refuse());
        }

        Ok(PollOutcome {
            sources: vec![Platform::source()],
            snapshots: vec![Platform::reading()],
            failures: Vec::new(),
        })
    }
}

/// A socket in a directory of its own.
fn a_socket() -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("a directory of our own");
    let socket = home.path().join("run").join("benshi.sock");

    (home, socket)
}

/// Ask the daemon one question and read one answer.
async fn ask(socket: &Path, request: &Request) -> Response {
    let stream = UnixStream::connect(socket)
        .await
        .expect("the daemon answers");
    let (reading, mut writing) = tokio::io::split(stream);

    let mut line = serde_json::to_vec(request).expect("serialise");
    line.push(b'\n');
    writing
        .write_all(&line)
        .await
        .expect("the request goes out");
    writing.flush().await.expect("the request goes out");

    let mut lines = BufReader::new(reading).lines();
    let answered = lines
        .next_line()
        .await
        .expect("the daemon did not drop the connection")
        .expect("the daemon answered");

    serde_json::from_str(&answered).expect("an answer this client can read")
}

/// Keep asking for a listing until one satisfies `enough`, or fail.
async fn listing_until(
    socket: &Path,
    what: &str,
    enough: impl Fn(&[SourceListing]) -> bool,
) -> Vec<SourceListing> {
    timeout(PATIENCE, async {
        loop {
            if let Response::Sources(listed) = ask(socket, &Request::Sources).await
                && enough(&listed)
            {
                return listed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"))
}

#[tokio::test]
async fn a_running_daemon_serves_what_its_own_detection_saw() {
    // The seam this test exists for: detection writes what a round found, and
    // a connection reads it. They meet only if `run` gave both halves the same
    // `Seen`, which nothing but assembling the real thing can show.
    let (_home, socket) = a_socket();
    let listener = Arc::new(bind(&socket).expect("the socket binds"));
    let platform = Platform::default();

    let running = tokio::spawn(benshi_daemon::run(platform.factory(), listener, PERIOD));

    let listed = listing_until(&socket, "the daemon listed what it detected", |listed| {
        !listed.is_empty()
    })
    .await;
    running.abort();

    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].player, PlayerId(PLAYER.to_owned()));
    assert_eq!(listed[0].app, AppName("mpv".to_owned()));
}

#[tokio::test]
async fn detection_giving_up_for_good_leaves_the_socket_answering() {
    // The reason there are two supervised tasks rather than one, and the test
    // has to push detection past recovery to show it. A watcher that merely
    // returns an error is retried for ever, so the socket would still be there
    // in an implementation that ran both loops in one task - the task never
    // ends. Panicking spends the restart limit instead, which stops detection
    // permanently, and a single task would take the socket down with it.
    //
    // The panics below are the point of the test and the supervisor records
    // each one, so this test is noisy on purpose.
    let (_home, socket) = a_socket();
    let listener = Arc::new(bind(&socket).expect("the socket binds"));
    let platform = Platform {
        panics: true,
        ..Platform::default()
    };

    let running = tokio::spawn(benshi_daemon::run(platform.factory(), listener, PERIOD));

    // Attempts that actually went down, not attempts that were scheduled. One
    // start and a restart per panic, the last of which spends the limit.
    let spent = usize::try_from(RESTART_LIMIT).expect("a small limit") + 1;
    timeout(PATIENCE, async {
        while platform.collapses() < spent {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "detection went down {} times and never exhausted its restarts",
            platform.collapses()
        );
    });

    let answer = timeout(PATIENCE, ask(&socket, &Request::Sources))
        .await
        .expect("the socket answered after detection had given up");
    running.abort();

    assert!(
        matches!(answer, Response::Sources(_)),
        "got {answer:?} rather than a listing"
    );
}

#[tokio::test]
async fn a_restart_builds_the_watcher_again_rather_than_reusing_it() {
    // A detection task is restarted because the platform went away, so the
    // restart has to reconnect to it. Reusing the watcher the first attempt
    // built would restart the loop around a connection that is already dead,
    // and the task would fail for ever while looking supervised.
    let (_home, socket) = a_socket();
    let listener = Arc::new(bind(&socket).expect("the socket binds"));
    let platform = Platform {
        fails: true,
        ..Platform::default()
    };

    let running = tokio::spawn(benshi_daemon::run(platform.factory(), listener, PERIOD));

    timeout(PATIENCE, async {
        while platform.watchers() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "a failing detection built {} watchers: it was never restarted, or \
             the restart reused the watcher the first attempt built",
            platform.watchers()
        )
    });
    running.abort();
}
