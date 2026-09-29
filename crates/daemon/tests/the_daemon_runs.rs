//! The whole daemon, assembled the way the binary assembles it.
//!
//! Every other test in this crate exercises one part against fakes of the
//! others. This one calls the function the binary calls, so that the wiring
//! itself is under test: whether detection and the socket were given the same
//! records, the same bus and the same policy table, whether detection was
//! given the store and the list the daemon was started with, whether one task
//! failing takes the other down, and whether a restart starts from a watcher
//! of its own.
//!
//! The platform is a fake, because a real one would make the run depend on
//! what happens to be playing. Everything above it is the production article.

#![cfg(unix)]
// A fake answers from memory, which is the whole point of a fake, and the trait
// asks for a future either way. The `async` therefore stays where there is
// nothing to await, rather than being spelled out as the future it desugars to.
#![allow(clippy::unused_async_trait_impl)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use benshi_core::clock::{Clock, TestClock, Timestamp};
use benshi_core::path::RawPath;
use benshi_core::policy::Policy;
use benshi_core::recognise::altname::Altnames;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use benshi_daemon::bus::BusEvent;
use benshi_daemon::ipc::bind;
use benshi_daemon::protocol::{Request, Response, SourceListing};
use benshi_daemon::recognition::Recogniser;
use benshi_daemon::supervisor::RESTART_LIMIT;
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
use benshi_store::Store;
use benshi_store::queue::{Kind, Operation};
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

/// How far apart the fake platform stamps two readings.
const STEP: Duration = Duration::from_secs(5);

/// The length the fake platform reports for its file.
const LENGTH: Duration = Duration::from_mins(4);

/// What [`the_time`] reads, as a store writes it.
const THE_TIME: &str = "2033-05-18T03:33:20Z";

/// The time of day a daemon here is given where a test reads what it wrote.
fn the_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

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
    clock: Arc<TestClock>,
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

    /// A reading of a file that plays, where the file has got to by `at`.
    fn reading(at: Timestamp) -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId(PLAYER.to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/[Group] Show - 03.mkv".to_vec(),
            )),
            state: PlayState::Playing,
            position: Known::Value(at.since(Timestamp::epoch())),
            duration: Known::Value(LENGTH),
            observed_at: at,
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

    /// How many times the platform has been read, which is how many steps
    /// its clock has been moved.
    fn polls(&self) -> u128 {
        self.clock.now().since(Timestamp::epoch()).as_nanos() / STEP.as_nanos()
    }

    /// Wait until the platform has been read this many times more.
    ///
    /// Counted and never slept for, so that a test says how many rounds have
    /// gone by whatever the machine was doing.
    async fn after(&self, more: u128) {
        let wanted = self.polls() + more;

        timeout(PATIENCE, async {
            while self.polls() < wanted {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("detection stopped at round {}", self.polls()));
    }

    /// A factory of watchers, as the daemon takes one.
    ///
    /// `use<>` because the daemon holds the factory for as long as it runs, so
    /// it must capture the count rather than borrow the platform it came from.
    fn factory(&self) -> impl FnMut() -> std::future::Ready<Result<Watcher, WatchError>> + use<> {
        let built = Arc::clone(&self.built);
        let fell_over = Arc::clone(&self.fell_over);
        let clock = Arc::clone(&self.clock);
        let fails = self.fails;
        let panics = self.panics;

        move || {
            built.fetch_add(1, Ordering::SeqCst);
            let fell_over = Arc::clone(&fell_over);
            let clock = Arc::clone(&clock);

            std::future::ready(Ok(Watcher {
                fell_over,
                clock,
                fails,
                panics,
            }))
        }
    }
}

/// One built watcher.
///
/// Every watcher of one platform reads the platform's clock, so a watcher
/// built after a restart goes on stamping where the one before it stopped.
struct Watcher {
    fell_over: Arc<AtomicUsize>,
    clock: Arc<TestClock>,
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

        self.clock.advance(STEP);

        Ok(PollOutcome {
            sources: vec![Platform::source()],
            snapshots: vec![Platform::reading(self.clock.now())],
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

/// A database in that directory, opened the way the binary opens one.
fn a_store(home: &TempDir) -> Arc<Mutex<Store>> {
    let database = home.path().join("data").join("benshi.db");

    Arc::new(Mutex::new(
        Store::open(&database).expect("the database opens"),
    ))
}

/// A daemon over this platform with nothing to match a name against, as the
/// binary starts one.
fn a_daemon(
    platform: &Platform,
    home: &TempDir,
    socket: &Path,
) -> tokio::task::JoinHandle<Vec<benshi_daemon::supervisor::TaskRecord>> {
    let listener = Arc::new(bind(socket).expect("the socket binds"));

    tokio::spawn(benshi_daemon::run(
        platform.factory(),
        listener,
        Recogniser::empty(),
        a_store(home),
        SystemTime::now,
        PERIOD,
    ))
}

/// A daemon over this platform that has the file it plays on its list, and
/// keeps what it watches in `store` at [`the_time`].
fn a_daemon_with_a_list(
    platform: &Platform,
    store: &Arc<Mutex<Store>>,
    socket: &Path,
) -> tokio::task::JoinHandle<Vec<benshi_daemon::supervisor::TaskRecord>> {
    let listener = Arc::new(bind(socket).expect("the socket binds"));

    tokio::spawn(benshi_daemon::run(
        platform.factory(),
        listener,
        Recogniser::of(["Show"].into_iter().collect(), Altnames::new()),
        Arc::clone(store),
        the_time,
        PERIOD,
    ))
}

/// What the store holds as watched of the episode the platform plays.
fn watched(store: &Arc<Mutex<Store>>) -> Option<Duration> {
    store
        .lock()
        .expect("the store")
        .resume("Show", Some(3))
        .expect("the total reads")
        .map(|kept| kept.watched)
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
    let (home, socket) = a_socket();
    let platform = Platform::default();

    let running = a_daemon(&platform, &home, &socket);

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
async fn a_running_daemon_records_what_its_detection_watched() {
    // The other seam: a round hands what it watched to a store, and the store
    // a test reads is the one the daemon was started with only if `run` gave
    // detection that store, with the list and the clock it was given.
    let (home, socket) = a_socket();
    let store = a_store(&home);
    let platform = Platform::default();

    let running = a_daemon_with_a_list(&platform, &store, &socket);

    let queued = timeout(PATIENCE, async {
        loop {
            let queued = store
                .lock()
                .expect("the store")
                .queued()
                .expect("the queue reads");
            if !queued.is_empty() {
                return queued;
            }
            tokio::time::sleep(PERIOD).await;
        }
    })
    .await
    .expect("the daemon recorded the episode it watched half of");
    running.abort();

    assert_eq!(
        queued,
        [Operation {
            id: 1,
            title: "Show".to_owned(),
            kind: Kind::Progress { episode: Some(3) },
        }]
    );
    let seen_at: String = rusqlite::Connection::open(home.path().join("data").join("benshi.db"))
        .expect("the file opens for reading")
        .query_row("SELECT seen_at FROM episodes_seen", [], |row| row.get(0))
        .expect("the one episode that was recorded");
    assert_eq!(seen_at, THE_TIME);
}

#[tokio::test]
async fn a_running_daemon_explains_what_its_own_detection_decided() {
    // The seam of the listing again, for the other record a round leaves.
    let (home, socket) = a_socket();
    let platform = Platform::default();

    let running = a_daemon(&platform, &home, &socket);

    let explained = timeout(PATIENCE, async {
        loop {
            if let Response::Why(Some(explained)) = ask(&socket, &Request::Why).await {
                return explained;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the daemon explained what it decided");
    running.abort();

    assert_eq!(explained.player, PlayerId(PLAYER.to_owned()));
}

#[tokio::test]
async fn a_running_daemon_streams_what_its_own_detection_published() {
    // A connection that watches is answered from the bus it was given, and a
    // round publishes on the bus it was given. They are one bus only if `run`
    // gave both halves the same.
    let (home, socket) = a_socket();
    let platform = Platform::default();

    let running = a_daemon(&platform, &home, &socket);

    let stream = UnixStream::connect(&socket)
        .await
        .expect("the daemon answers");
    let (reading, mut writing) = tokio::io::split(stream);
    let mut line = serde_json::to_vec(&Request::Watch).expect("serialise");
    line.push(b'\n');
    writing
        .write_all(&line)
        .await
        .expect("the request goes out");
    writing.flush().await.expect("the request goes out");
    let mut lines = BufReader::new(reading).lines();

    let published = timeout(PATIENCE, async {
        loop {
            let said = lines
                .next_line()
                .await
                .expect("the daemon did not drop the connection")
                .expect("the stream goes on");
            if let Response::Event(BusEvent::Snapshot(reading)) =
                serde_json::from_str(&said).expect("an event this client can read")
            {
                return reading;
            }
        }
    })
    .await
    .expect("a reading reached the connection that watches");
    running.abort();

    assert_eq!(published.player, PlayerId(PLAYER.to_owned()));
}

#[tokio::test]
async fn a_policy_set_over_the_socket_is_the_one_detection_reads() {
    // A connection writes the table it was given and a round reads the table
    // it was given. Read here from the store, where what was watched goes on
    // growing for as long as the player is admitted.
    let (home, socket) = a_socket();
    let store = a_store(&home);
    let platform = Platform::default();

    let running = a_daemon_with_a_list(&platform, &store, &socket);
    timeout(PATIENCE, async {
        while watched(&store).is_none() {
            tokio::time::sleep(PERIOD).await;
        }
    })
    .await
    .expect("the daemon kept what it watched");

    let answer = ask(
        &socket,
        &Request::SetPolicy {
            app: AppName("mpv".to_owned()),
            policy: Policy::Deny,
        },
    )
    .await;
    assert!(matches!(answer, Response::Ok), "got {answer:?}");

    // A round that had begun before the deny may still add to the total. The
    // round after it read the deny, and it is over once another has begun.
    platform.after(2).await;
    let at_the_deny = watched(&store);
    platform.after(5).await;
    running.abort();

    assert_eq!(
        watched(&store),
        at_the_deny,
        "a player that was denied went on being counted"
    );
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
    let (home, socket) = a_socket();
    let platform = Platform {
        panics: true,
        ..Platform::default()
    };

    let running = a_daemon(&platform, &home, &socket);

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
    let (home, socket) = a_socket();
    let platform = Platform {
        fails: true,
        ..Platform::default()
    };

    let running = a_daemon(&platform, &home, &socket);

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
