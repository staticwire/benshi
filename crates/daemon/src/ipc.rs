//! The local socket the client talks to.
//!
//! One connection carries newline-delimited JSON in both directions: a request
//! on one line, and the answers to it on one line each. The wire format lives
//! in [`crate::protocol`]; this module is the transport and the answering.
//!
//! Answering is written over any byte stream rather than over a socket, so the
//! protocol can be exercised without a filesystem, and so that the named pipe
//! Windows will need has to supply only its own binding and accept loop.

use std::env;
use std::fs;
use std::io::{Error, ErrorKind, Result};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use benshi_core::policy::PolicyTable;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinSet;

use crate::bus::EventBus;
use crate::detection::Seen;
use crate::protocol::{Request, Response, SourceListing};
use crate::supervisor::TaskError;

/// The socket's name inside whichever directory holds it.
const SOCKET_NAME: &str = "benshi.sock";

/// The directory the fallback puts the socket in, beneath the temporary one.
const FALLBACK_DIRECTORY: &str = "benshi";

/// The mode the socket's directory must have: reachable by its owner and by
/// nobody else.
const PRIVATE: u32 = 0o700;

/// What is said when the policy table cannot be trusted.
const POISONED: &str = "the policy table was poisoned by a panic in another task";

/// Where the daemon listens.
///
/// `$XDG_RUNTIME_DIR` when the session sets one, which every session managed by
/// systemd-logind does. It is the right place: per-user, mode 0700, usually a
/// tmpfs, and emptied when the user's last session ends, so a socket cannot
/// outlive the login it belonged to.
#[must_use]
pub fn socket_path() -> PathBuf {
    socket_path_from(
        env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        &env::temp_dir(),
    )
}

/// Where the daemon listens, given a session's directories.
///
/// Without a runtime directory the socket goes into a directory of its own
/// beneath the temporary one rather than straight into it. A temporary
/// directory is usually `/tmp`, which every user on the machine can write to,
/// so a name both users compute the same is a name either of them may create
/// first: whoever loses cannot bind, and a client asking for that name has no
/// way to tell whose daemon answers. One directory, mode 0700, closes both: the
/// second user cannot enter it and finds out at once rather than by accident.
fn socket_path_from(runtime_dir: Option<PathBuf>, temporary_dir: &Path) -> PathBuf {
    runtime_dir.map_or_else(
        || temporary_dir.join(FALLBACK_DIRECTORY).join(SOCKET_NAME),
        |dir| dir.join(SOCKET_NAME),
    )
}

/// Whether something is still listening on a socket that already exists.
///
/// A unix socket is a file that outlives the process that bound it, so a daemon
/// killed outright leaves one behind and the next start would be refused. The
/// only way to tell a leftover from a running daemon is to try it. A refused
/// connection is the one answer that proves nobody is there; every other answer
/// is read as a daemon that is running, because that is the side to be wrong
/// on.
fn still_listening(path: &Path) -> bool {
    !matches!(
        std::os::unix::net::UnixStream::connect(path),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused
    )
}

/// Make sure the socket's directory exists and no one else can reach it.
///
/// Recursive, so a directory that is already there is left as it is and checked
/// rather than made again.
fn private_directory(directory: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE)
        .create(directory)?;

    let mode = fs::metadata(directory)?.permissions().mode() & 0o777;
    if mode == PRIVATE {
        return Ok(());
    }

    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!(
            "{} is mode {mode:o} and must be {PRIVATE:o}: a socket in a directory \
             others can reach is one others can replace",
            directory.display()
        ),
    ))
}

/// Bind the daemon's socket, preparing its directory.
///
/// A socket left behind by a daemon that did not shut down cleanly is replaced.
/// One a daemon is still listening on is not: taking it would leave the first
/// daemon running and unreachable, with nothing anywhere to say so.
///
/// # Errors
///
/// [`ErrorKind::AddrInUse`] when a daemon is already listening there,
/// [`ErrorKind::PermissionDenied`] when the directory is open to other users,
/// [`ErrorKind::InvalidInput`] for a path with no directory to put a socket in,
/// and whatever the filesystem says otherwise.
///
/// # Panics
///
/// If called outside a tokio runtime with I/O enabled, because that is where
/// the listener registers itself. The tests that bind are `#[tokio::test]` for
/// that reason and not because they await anything.
pub fn bind(path: &Path) -> Result<UnixListener> {
    let directory = path
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "a socket path has a directory"))?;
    private_directory(directory)?;

    if path.exists() {
        if still_listening(path) {
            return Err(Error::new(
                ErrorKind::AddrInUse,
                format!("a daemon is already listening on {}", path.display()),
            ));
        }
        fs::remove_file(path)?;
    }

    UnixListener::bind(path)
}

/// The daemon's side of the local socket.
///
/// Holds what a connection is answered from and nothing of its own. Cloning is
/// not needed: connections share one server through an [`Arc`].
#[derive(Debug)]
pub struct Server {
    bus: Arc<EventBus>,
    policy: Arc<RwLock<PolicyTable>>,
    seen: Seen,
}

impl Server {
    /// A server over the bus a `Watch` streams, the table a listing reads and a
    /// policy is set in, and the record each detection round leaves behind.
    #[must_use]
    pub fn new(bus: Arc<EventBus>, policy: Arc<RwLock<PolicyTable>>, seen: Seen) -> Self {
        Self { bus, policy, seen }
    }

    /// Accept connections until the listener fails.
    ///
    /// Each connection's handle is owned here rather than spawned and dropped,
    /// so a panic while answering reaches the supervisor instead of vanishing
    /// into a detached task. A connection that merely failed is that client's
    /// business and ends only that connection.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError::Transient`] when the listener stops accepting. The
    /// socket is local, so this is a resource limit rather than a fault in the
    /// daemon, and it passes.
    ///
    /// # Panics
    ///
    /// If answering a connection panicked. That is a bug in this process and is
    /// re-raised here so the supervisor records it.
    pub async fn listen(
        self: Arc<Self>,
        listener: UnixListener,
    ) -> std::result::Result<(), TaskError> {
        let mut connections = JoinSet::new();

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _peer) = accepted
                        .map_err(|failed| TaskError::Transient(failed.into()))?;
                    let server = Arc::clone(&self);
                    connections.spawn(async move { server.serve(stream).await });
                }
                Some(finished) = connections.join_next() => {
                    match finished {
                        // The connection is over. A failure on it was that
                        // client's business and ended only that connection.
                        Ok(_answered) => (),
                        Err(joined) if joined.is_panic() => {
                            std::panic::resume_unwind(joined.into_panic())
                        }
                        // Nothing here aborts a connection and the set outlives
                        // the loop, so a cancellation cannot arise. It is
                        // matched rather than asserted away because a cancelled
                        // connection has already ended, which is the outcome
                        // above.
                        Err(_cancelled) => (),
                    }
                }
            }
        }
    }

    /// Answer one connection until the client goes away.
    ///
    /// A [`Request::Watch`] is the last request on its connection: what follows
    /// it is a stream rather than an answer, and nothing more is read.
    ///
    /// # Errors
    ///
    /// Returns an I/O failure on the connection. A client that closed is not
    /// one of those: `benshi watch` ends with Ctrl-C, and a daemon that
    /// recorded a failure every time someone stopped watching would report
    /// nothing worth reading.
    pub async fn serve<S>(&self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match self.answer(stream).await {
            Err(error) if client_left(&error) => Ok(()),
            other => other,
        }
    }

    /// Read requests and answer them, failing on anything the connection does.
    async fn answer<S>(&self, stream: S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader).lines();

        while let Some(line) = lines.next_line().await? {
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(Request::Sources) => Response::Sources(self.listing()),
                Ok(Request::SetPolicy { app, policy }) => {
                    self.policy.write().expect(POISONED).set(&app, policy);
                    Response::Ok
                }
                // A stream rather than an answer, so this connection reads
                // nothing further.
                Ok(Request::Watch) => return self.stream_events(&mut writer).await,
                Err(malformed) => Response::Error {
                    message: malformed.to_string(),
                },
            };

            reply(&mut writer, &response).await?;
        }

        Ok(())
    }

    /// What the last detection round saw, with the policy in force for each.
    ///
    /// The table is read once for the whole listing, so every line answers
    /// about the same moment.
    ///
    /// # Panics
    ///
    /// If the policy table was poisoned by a panic elsewhere. A listing built
    /// from a table nobody can trust would answer "why is my player being
    /// ignored" wrongly, which is the one thing it exists not to do.
    fn listing(&self) -> Vec<SourceListing> {
        let table = self.policy.read().expect(POISONED);

        self.seen
            .sources()
            .iter()
            .map(|source| SourceListing::of(source, table.policy_for(&source.app)))
            .collect()
    }

    /// Write every event to the client until it goes away.
    async fn stream_events<W>(&self, writer: &mut W) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut events = self.bus.subscribe();

        loop {
            let response = match events.recv().await {
                Ok(event) => Response::Event(event),
                Err(RecvError::Lagged(missed)) => Response::Lagged { missed },
                // The daemon is shutting down. Nothing further will be
                // published, so the stream is over rather than broken.
                Err(RecvError::Closed) => return Ok(()),
            };

            reply(writer, &response).await?;
        }
    }
}

/// Write one answer and make sure it has left.
///
/// Flushed per answer rather than per batch: a stream a client is watching is
/// worth nothing if it arrives when the buffer happens to fill.
async fn reply<W>(writer: &mut W, response: &Response) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');

    writer.write_all(&line).await?;
    writer.flush().await
}

/// Whether an I/O failure is the client having gone away.
fn client_left(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
}

#[cfg(test)]
mod tests {
    use super::{Server, bind, socket_path_from};
    use crate::bus::{BusEvent, EventBus};
    use crate::detection::Seen;
    use crate::protocol::{Request, Response};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::{Policy, PolicyTable};
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };
    use benshi_detect::SourceInfo;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
    use tokio::net::UnixStream;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    /// How long a test waits for an answer before calling it lost.
    ///
    /// Generous, because it is only ever spent on a failure. What matters is
    /// that it exists: a missing answer has to fail the run rather than hang
    /// it, and a hung run reports nothing at all.
    const PATIENCE: Duration = Duration::from_secs(5);

    fn a_source(identity: &str, app: &str) -> SourceInfo {
        SourceInfo {
            player: PlayerId(identity.to_owned()),
            app: AppName(app.to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    fn a_snapshot() -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::Value(Duration::from_mins(23)),
            observed_at: Timestamp::epoch(),
        }
    }

    /// A server over a bus and a table the test keeps hold of.
    struct Daemon {
        server: Arc<Server>,
        bus: Arc<EventBus>,
        policy: Arc<RwLock<PolicyTable>>,
    }

    impl Daemon {
        fn with(sources: Vec<SourceInfo>) -> Self {
            Self::holding(crate::bus::CAPACITY, sources)
        }

        /// A daemon whose bus keeps `capacity` events for a slow reader.
        fn holding(capacity: usize, sources: Vec<SourceInfo>) -> Self {
            let bus = Arc::new(EventBus::with_capacity(capacity));
            let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
            let seen = Seen::new();
            seen.record(sources);

            Self {
                server: Arc::new(Server::new(Arc::clone(&bus), Arc::clone(&policy), seen)),
                bus,
                policy,
            }
        }

        /// Wait until a connection has subscribed, so what follows is not a race.
        async fn watched(&self) {
            timeout(PATIENCE, async {
                while self.bus.subscribers() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("a connection subscribed to the bus");
        }
    }

    /// A client on one end of a served connection.
    ///
    /// The connection is a pair of in-memory pipes rather than a socket: the
    /// protocol does not care which, and a test that needs no filesystem cannot
    /// fail for a reason that has nothing to do with what it checks. The socket
    /// itself is exercised separately, once, where it is the subject.
    struct Client {
        writer: WriteHalf<DuplexStream>,
        lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
        serving: JoinHandle<std::io::Result<()>>,
    }

    impl Client {
        /// Connect to a server, with a pipe of `capacity` bytes each way.
        fn to(daemon: &Daemon, capacity: usize) -> Self {
            let (mine, theirs) = tokio::io::duplex(capacity);
            let server = Arc::clone(&daemon.server);
            let serving = tokio::spawn(async move { server.serve(theirs).await });
            let (reader, writer) = tokio::io::split(mine);

            Self {
                writer,
                lines: BufReader::new(reader).lines(),
                serving,
            }
        }

        async fn ask(&mut self, request: &Request) {
            let line = serde_json::to_string(request).expect("serialise");
            self.send(&line).await;
        }

        async fn send(&mut self, line: &str) {
            self.writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("the request was sent");
            self.writer.flush().await.expect("the request was flushed");
        }

        /// The next answer, or a failure if none arrives.
        async fn answer(&mut self) -> Response {
            let line = timeout(PATIENCE, self.lines.next_line())
                .await
                .expect("an answer arrived")
                .expect("the connection is readable")
                .expect("the connection was not closed");

            serde_json::from_str(&line).expect("the answer is a response")
        }

        /// Close the connection and hand back what the server is doing.
        ///
        /// Both halves are dropped, because the pipe closes only when the last
        /// of them goes. That is what the server sees when a client stops
        /// watching.
        fn hang_up(self) -> JoinHandle<std::io::Result<()>> {
            let Self {
                writer,
                lines,
                serving,
            } = self;
            drop(writer);
            drop(lines);

            serving
        }
    }

    #[tokio::test]
    async fn sources_returns_every_source_with_its_policy_and_capabilities() {
        let daemon = Daemon::with(vec![
            a_source("mpv.instance1701", "mpv"),
            a_source("firefox.instance30062", "firefox"),
        ]);
        let mut client = Client::to(&daemon, 4096);

        client.ask(&Request::Sources).await;
        let Response::Sources(listed) = client.answer().await else {
            panic!("expected a listing");
        };

        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].player, PlayerId("mpv.instance1701".to_owned()));
        assert_eq!(listed[0].policy, Policy::Auto);
        assert!(listed[0].capabilities.position);
        assert_eq!(
            listed[1].player,
            PlayerId("firefox.instance30062".to_owned())
        );
        assert_eq!(
            listed[1].policy,
            Policy::Deny,
            "a browser is denied by default and the listing has to say so"
        );
    }

    #[tokio::test]
    async fn a_listing_before_the_first_round_is_empty_rather_than_an_error() {
        // A client can connect before detection has run once. Nothing has been
        // seen, and that is an answer rather than a fault.
        let daemon = Daemon::with(Vec::new());
        let mut client = Client::to(&daemon, 4096);

        client.ask(&Request::Sources).await;

        assert_eq!(client.answer().await, Response::Sources(Vec::new()));
    }

    #[tokio::test]
    async fn watch_streams_events_as_they_are_published() {
        let daemon = Daemon::with(Vec::new());
        let mut client = Client::to(&daemon, 4096);

        client.ask(&Request::Watch).await;
        daemon.watched().await;
        daemon.bus.publish(BusEvent::SourcesChanged {
            sources: vec![PlayerId("mpv".to_owned())],
        });
        daemon.bus.publish(BusEvent::Snapshot(a_snapshot()));

        assert_eq!(
            client.answer().await,
            Response::Event(BusEvent::SourcesChanged {
                sources: vec![PlayerId("mpv".to_owned())],
            })
        );
        assert_eq!(
            client.answer().await,
            Response::Event(BusEvent::Snapshot(a_snapshot()))
        );
    }

    #[tokio::test]
    async fn a_policy_set_over_the_socket_takes_effect_at_once() {
        // The table the daemon reads is the table this writes, so a policy set
        // here changes what detection publishes without the daemon restarting.
        let daemon = Daemon::with(vec![a_source("mpv", "mpv")]);
        let mut client = Client::to(&daemon, 4096);

        client
            .ask(&Request::SetPolicy {
                app: AppName("mpv".to_owned()),
                policy: Policy::Deny,
            })
            .await;

        assert_eq!(client.answer().await, Response::Ok);
        assert_eq!(
            daemon
                .policy
                .read()
                .expect("the table")
                .policy_for(&AppName("mpv".to_owned())),
            Policy::Deny
        );
    }

    #[tokio::test]
    async fn a_policy_set_over_the_socket_shows_in_the_next_listing() {
        let daemon = Daemon::with(vec![a_source("mpv", "mpv")]);
        let mut client = Client::to(&daemon, 4096);

        client
            .ask(&Request::SetPolicy {
                app: AppName("mpv".to_owned()),
                policy: Policy::Deny,
            })
            .await;
        assert_eq!(client.answer().await, Response::Ok);

        client.ask(&Request::Sources).await;
        let Response::Sources(listed) = client.answer().await else {
            panic!("expected a listing");
        };

        assert_eq!(listed[0].policy, Policy::Deny);
    }

    #[tokio::test]
    async fn a_malformed_line_is_answered_with_an_error_and_the_connection_survives() {
        // A client that sent nonsense is told so and may carry on. Closing the
        // connection would make one typo in a socat session look like a daemon
        // that had died.
        let daemon = Daemon::with(vec![a_source("mpv", "mpv")]);
        let mut client = Client::to(&daemon, 4096);

        client.send("this is not json").await;
        let answer = client.answer().await;

        assert!(
            matches!(answer, Response::Error { .. }),
            "expected an error, got {answer:?}"
        );

        client.ask(&Request::Sources).await;
        assert!(
            matches!(client.answer().await, Response::Sources(_)),
            "the connection did not survive a malformed line"
        );
    }

    #[tokio::test]
    async fn an_empty_line_is_an_error_rather_than_a_closed_connection() {
        let daemon = Daemon::with(Vec::new());
        let mut client = Client::to(&daemon, 4096);

        client.send("").await;

        assert!(matches!(client.answer().await, Response::Error { .. }));
    }

    #[tokio::test]
    async fn a_client_disconnecting_mid_stream_does_not_disturb_the_daemon() {
        // `benshi watch` ends with Ctrl-C, so a client going away in the middle
        // of a stream is the ordinary way a connection ends and must not be
        // recorded as a failure.
        let daemon = Daemon::with(Vec::new());
        let mut client = Client::to(&daemon, 64);

        client.ask(&Request::Watch).await;
        daemon.watched().await;

        let serving = client.hang_up();
        for _ in 0..8 {
            daemon.bus.publish(BusEvent::Snapshot(a_snapshot()));
        }

        let ended = timeout(PATIENCE, serving)
            .await
            .expect("the connection ended")
            .expect("serving did not panic");
        assert!(
            ended.is_ok(),
            "a client going away was reported as a failure: {ended:?}"
        );
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_told_what_it_missed() {
        // Losing events on a diagnostic stream is acceptable. Losing them in
        // silence is not: a gap nobody reports reads as nothing having
        // happened, which is the one wrong answer this stream can give.
        // A bus that holds two events, flooded with eight. Which of the eight
        // survive is the channel's business; that the client hears about the
        // ones that did not is this daemon's.
        let daemon = Daemon::holding(2, Vec::new());
        let mut client = Client::to(&daemon, 4096);

        client.ask(&Request::Watch).await;
        daemon.watched().await;
        for _ in 0..8 {
            daemon.bus.publish(BusEvent::Snapshot(a_snapshot()));
        }

        let mut missed = None;
        for _ in 0..4 {
            match client.answer().await {
                Response::Lagged { missed: gap } => {
                    missed = Some(gap);
                    break;
                }
                Response::Event(_) => (),
                other => panic!("expected events or a gap, got {other:?}"),
            }
        }

        assert!(
            missed.is_some_and(|gap| gap > 0),
            "six events went missing and the client was not told"
        );
    }

    #[tokio::test]
    async fn a_request_and_its_answer_cross_a_real_socket() {
        // Everything else here runs over a pipe, which proves the protocol and
        // says nothing about the transport. This is the one that binds.
        let home = tempfile::tempdir().expect("a directory of our own");
        let path = home.path().join("run").join("benshi.sock");
        let listener = bind(&path).expect("the socket binds");
        let daemon = Daemon::with(vec![a_source("mpv", "mpv")]);
        let server = Arc::clone(&daemon.server);

        let accepting = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("a client arrived");
            server.serve(stream).await
        });

        let stream = UnixStream::connect(&path)
            .await
            .expect("the client connects");
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(b"{\"type\":\"Sources\"}\n")
            .await
            .expect("the request was sent");

        let line = timeout(PATIENCE, lines.next_line())
            .await
            .expect("an answer arrived")
            .expect("the connection is readable")
            .expect("the connection was not closed");
        let answer: Response = serde_json::from_str(&line).expect("a response");

        let Response::Sources(listed) = answer else {
            panic!("expected a listing");
        };
        assert_eq!(listed[0].player, PlayerId("mpv".to_owned()));

        drop(writer);
        drop(lines);
        timeout(PATIENCE, accepting)
            .await
            .expect("serving ended")
            .expect("serving did not panic")
            .expect("serving reported no failure");
    }

    #[tokio::test]
    async fn a_socket_left_behind_by_a_daemon_that_died_is_replaced() {
        // A unix socket outlives the process that bound it, so a daemon killed
        // with SIGKILL leaves a file that binding would otherwise refuse. A
        // daemon that cannot start after a crash is one nobody runs.
        let home = tempfile::tempdir().expect("a directory of our own");
        let path = home.path().join("run").join("benshi.sock");
        let abandoned = bind(&path).expect("the first bind");
        drop(abandoned);
        assert!(path.exists(), "the socket file outlived its listener");

        let listener = bind(&path).expect("the stale socket was replaced");

        drop(listener);
    }

    #[tokio::test]
    async fn a_socket_a_running_daemon_is_listening_on_is_left_alone() {
        // The other half of the same rule, and the dangerous one: removing a
        // live daemon's socket and binding over it would leave the first daemon
        // running and unreachable, with no sign that anything had happened.
        let home = tempfile::tempdir().expect("a directory of our own");
        let path = home.path().join("run").join("benshi.sock");
        let running = bind(&path).expect("the first bind");

        let refused = bind(&path).expect_err("a second daemon must not take the socket");

        assert_eq!(refused.kind(), std::io::ErrorKind::AddrInUse);
        drop(running);
    }

    #[tokio::test]
    async fn a_directory_other_users_can_reach_is_refused() {
        // The fallback lives under a shared temporary directory, where anyone
        // may create a name before the daemon does. A directory others can
        // enter is one where the socket can be replaced under the daemon's
        // feet, so binding inside it is refused rather than trusted.
        let home = tempfile::tempdir().expect("a directory of our own");
        let open = home.path().join("open-to-everyone");
        fs::DirBuilder::new()
            .mode(0o777)
            .create(&open)
            .expect("a permissive directory");

        let refused = bind(&open.join("benshi.sock")).expect_err("must refuse");

        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_directory_the_fallback_makes_is_reachable_by_nobody_else() {
        let home = tempfile::tempdir().expect("a directory of our own");
        let directory = home.path().join("benshi");

        bind(&directory.join("benshi.sock")).expect("the socket binds");

        let mode = fs::metadata(&directory)
            .expect("the directory")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "the directory is readable by others");
    }

    #[test]
    fn the_socket_sits_in_the_runtime_directory_when_the_session_gives_one() {
        let path = socket_path_from(Some(PathBuf::from("/run/user/1000")), Path::new("/tmp"));

        assert_eq!(path, PathBuf::from("/run/user/1000/benshi.sock"));
    }

    #[test]
    fn without_a_runtime_directory_the_socket_gets_a_directory_of_its_own() {
        // A temporary directory is usually shared between users. The socket
        // goes into a directory beneath it rather than straight in, so that one
        // directory's permissions protect it.
        let path = socket_path_from(None, Path::new("/tmp"));

        assert_eq!(path, PathBuf::from("/tmp/benshi/benshi.sock"));
        assert_ne!(
            path.parent(),
            Some(Path::new("/tmp")),
            "the socket must not sit directly in a shared directory"
        );
    }
}
