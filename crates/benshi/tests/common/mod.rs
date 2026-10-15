//! The binary run in a session a test chose.
//!
//! The binary itself is run, because what is under test is what a person
//! reads. A process a test starts holds a copy of every descriptor open in the
//! test binary until it runs its own program, so a test binary that uses this
//! never counts on a socket of its own being gone the moment it lets go of it.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// How long a test waits for the binary to say something or to end before
/// calling it lost. Only ever spent on a failure.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// A session bus that is not there, so that a daemon under test reaches no
/// player of the person running the tests. A session that names a bus of the
/// test's own has that one.
const NO_BUS: &str = "unix:path=/nowhere/bus";

/// How a process ends on Linux whose `main` answered `ExitCode::SUCCESS`.
pub const SUCCEEDED: Option<i32> = Some(0);

/// What a client shows of a daemon that has seen no source.
// Dead in a test binary that asks no daemon for its sources.
#[allow(dead_code)]
pub const NO_SOURCE: &str = "no source is open\n";

/// What the last line a daemon says as it starts begins with.
const STARTED: &str = "benshi: recording in ";

/// What a daemon says of its settings in a session that names no
/// configuration directory, which a session a test chose does unless it sets
/// one.
// Dead in a test binary that reads no daemon's first lines whole.
#[allow(dead_code)]
pub const NO_SETTINGS: &str = "benshi: default settings: no configuration directory: no absolute \
                               path in `XDG_CONFIG_HOME` or `HOME`";

/// The flag that asks for a daemon without a tray icon.
pub const NO_TRAY: &str = "--no-tray";

/// How many descriptors the binary may open besides the three it is started
/// with.
#[derive(Debug, Clone, Copy)]
pub enum Descriptors {
    /// As many as the tests themselves may.
    Plenty,
    /// One and no more. A runtime opens more than one, so it cannot be
    /// started.
    // Dead in a test binary whose every runtime starts.
    #[allow(dead_code)]
    One,
}

/// The binary asked for `arguments` in a session the test chose, with nothing
/// to read, nowhere to answer and nothing of the session the tests run in.
pub fn the_binary(
    arguments: &[&OsStr],
    session: &[(&str, &OsStr)],
    descriptors: Descriptors,
) -> Command {
    let binary = match descriptors {
        Descriptors::Plenty => unignoring(env!("CARGO_BIN_EXE_benshi")),
        // Under a limit of four a descriptor is one of 0 to 3, and the first
        // three are the streams the binary is started with. The shell lets
        // go of 3 first, so that it is free whatever the tests were started
        // with.
        Descriptors::One => after("exec 3>&-; ulimit -n 4"),
    };

    in_session(binary, arguments, session)
}

/// `program` run with each signal that stops a daemon doing what the system
/// does with it, whatever the tests were started with.
///
/// A process starts with the signals ignored that its parent ignored, and a
/// shell script that runs the tests in the background starts them with
/// `SIGINT` ignored. A shell cannot undo that in what it starts, and `env`
/// can.
fn unignoring(program: &str) -> Command {
    let mut env = Command::new("/usr/bin/env");
    env.args(["--default-signal=TERM,INT,HUP", program]);

    env
}

/// The binary as a shell starts it that did `first`: in the process of the
/// shell, with what `first` left.
fn after(first: &str) -> Command {
    let mut shell = unignoring("/bin/sh");
    shell.args([
        "-c",
        &format!("{first}; exec \"$@\""),
        "sh",
        env!("CARGO_BIN_EXE_benshi"),
    ]);

    shell
}

/// `binary` asked for `arguments` in a session the test chose, with nothing
/// to read, nowhere to answer and nothing of the session the tests run in.
fn in_session(mut binary: Command, arguments: &[&OsStr], session: &[(&str, &OsStr)]) -> Command {
    binary
        .args(arguments)
        .env_clear()
        .env("DBUS_SESSION_BUS_ADDRESS", NO_BUS)
        .envs(session.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null());

    binary
}

/// Somewhere that cannot be written to. Every write to `/dev/full` fails, as
/// one does to a terminal that has gone and to a pipe whose reader has gone.
pub fn nowhere() -> Stdio {
    File::options()
        .write(true)
        .open("/dev/full")
        .expect("`/dev/full` opens for writing")
        .into()
}

/// One run of the binary that was waited for until it ended.
// `shown` and `said` are dead in a test binary that reads only the code of a
// run.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Run {
    /// What it ended with, and nothing where a signal ended it. One that is
    /// still running after `PATIENCE` is ended by the test.
    pub code: Option<i32>,
    /// What it answered on stdout.
    pub shown: String,
    /// What it said on stderr, and nothing where it had nowhere to say it.
    pub said: String,
}

/// Runs the binary until it ends, hearing what it answers and what it says
/// where the command left that to be heard.
pub fn run(mut binary: Command) -> Run {
    let running = binary
        .stdout(Stdio::piped())
        .spawn()
        .expect("the binary runs");

    ends(running)
}

/// A `watch` of the daemon at `socket`, started and left running, which is
/// heard once it has ended.
// Dead in a test binary that watches no daemon.
#[allow(dead_code)]
pub fn watching(socket: &Path) -> Child {
    the_binary(
        &[
            OsStr::new("watch"),
            OsStr::new("--socket"),
            socket.as_os_str(),
        ],
        &[],
        Descriptors::Plenty,
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("the binary runs")
}

/// Whether the daemon at `socket` has accepted a connection.
///
/// Read from `/proc/net/unix`, which lists every connection to a socket
/// under the socket's path: the listener with its state `01`, a connection
/// waiting in the backlog with `02`, and one that was accepted with `03`.
// Dead in a test binary that connects no client of its own.
#[allow(dead_code)]
pub fn has_a_connection(socket: &Path) -> bool {
    let path = socket.to_str().expect("the socket's path is text");
    let listed = std::fs::read_to_string("/proc/net/unix").expect("`/proc/net/unix` can be read");

    listed.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.get(5) == Some(&"03") && fields.last() == Some(&path)
    })
}

/// Waits for `running` until it ends, hearing what it answered and what it
/// said where the command left that to be heard.
pub fn ends(mut running: Child) -> Run {
    let ended = ends_within(&mut running, PATIENCE);
    if ended.is_none() {
        // Neither is worth a panic: the code that is missing says that the
        // binary did not end.
        drop(running.kill());
        drop(running.wait());
    }

    Run {
        code: ended.and_then(|status| status.code()),
        shown: heard(running.stdout.take()),
        said: heard(running.stderr.take()),
    }
}

/// How a process ended where it ended within `patience`, and nothing where it
/// was still running by then.
fn ends_within(running: &mut Child, patience: Duration) -> Option<ExitStatus> {
    let waited_from = Instant::now();
    loop {
        let ended = running.try_wait().expect("the binary can be asked");
        if ended.is_some() || waited_from.elapsed() > patience {
            return ended;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Everything a process that has ended wrote, and nothing where it was given
/// nowhere to write that the test reads.
fn heard(written: Option<impl Read>) -> String {
    let mut all = String::new();
    if let Some(mut written) = written {
        written
            .read_to_string(&mut all)
            .expect("what the binary wrote is text");
    }

    all
}

/// A directory of one test, gone when the test is over.
///
/// Under the temporary directory of the system, as the tests of the other
/// crates keep theirs. A daemon syncs its database as it makes it and as it
/// lets go of it, and on a disk that is busy writing either can take longer
/// than a test allows. Where the temporary directory is in memory a sync
/// costs nothing.
///
/// Named after the test, so that one left behind says whose it was.
pub struct Directory(TempDir);

impl Directory {
    pub fn of(test: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(&format!("benshi-{test}-"))
            .tempdir()
            .expect("a directory of our own");

        Self(directory)
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

/// A stream that is full, and the end of it that nobody reads.
///
/// The test keeps that end for as long as the daemon runs: a write to a
/// stream whose other end has gone fails, where a write to this one waits.
// Dead in a test binary that reads every daemon it starts.
#[allow(dead_code)]
pub fn nobody_reads() -> (UnixStream, Stdio) {
    let (unread, full) = UnixStream::pair().expect("a pair of sockets can be made");
    // Filled without waiting, and handed over as a stream that waits.
    full.set_nonblocking(true)
        .expect("a socket can be kept from waiting");
    let filling = [b'.'; 4096];
    loop {
        match (&full).write(&filling) {
            Ok(_taken) => {}
            Err(waits) if waits.kind() == ErrorKind::WouldBlock => break,
            Err(other) => panic!("the stream could not be filled: {other}"),
        }
    }
    full.set_nonblocking(false)
        .expect("a socket can be made to wait");

    (unread, OwnedFd::from(full).into())
}

/// A daemon in a session the test chose, stopped when this goes.
pub struct Daemon {
    running: Child,
    heard: Receiver<String>,
    reader: Option<JoinHandle<()>>,
}

impl Daemon {
    pub fn start(session: &[(&str, &OsStr)]) -> Self {
        Self::heard_for(session, usize::MAX)
    }

    /// A daemon that is read for this many lines and no more. The pipe is
    /// let go of after the last of them, so a line the daemon writes after
    /// that is one it cannot write.
    pub fn heard_for(session: &[(&str, &OsStr)], limit: usize) -> Self {
        Self::hearing(&[OsStr::new(NO_TRAY)], session, limit)
    }

    /// A daemon asked for `arguments`, which a test that starts one with a
    /// tray leaves empty.
    // Dead in a test binary that asks every daemon for no tray.
    #[allow(dead_code)]
    pub fn asked(arguments: &[&OsStr], session: &[(&str, &OsStr)]) -> Self {
        Self::hearing(arguments, session, usize::MAX)
    }

    /// A daemon started with `signal` ignored, as `nohup` starts one with
    /// `HUP`. The signal is named as `trap` names it.
    // Dead in a test binary whose every daemon starts with no signal ignored.
    #[allow(dead_code)]
    pub fn ignoring(signal: &str, session: &[(&str, &OsStr)]) -> Self {
        let binary = after(&format!("trap '' {signal}"));

        Self::read_for(
            in_session(binary, &[OsStr::new(NO_TRAY)], session),
            usize::MAX,
        )
    }

    fn hearing(arguments: &[&OsStr], session: &[(&str, &OsStr)], limit: usize) -> Self {
        Self::read_for(the_binary(arguments, session, Descriptors::Plenty), limit)
    }

    fn read_for(mut binary: Command, limit: usize) -> Self {
        let mut running = binary
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary runs");
        let said = running.stderr.take().expect("what it says is piped");
        let (lines, heard) = mpsc::channel();
        // Read on a thread of its own, so that a daemon that says nothing
        // fails the test where it would otherwise hold it for ever.
        let reader = thread::spawn(move || {
            let read = BufReader::new(said).lines().map_while(Result::ok);
            for line in read.take(limit) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });

        Self {
            running,
            heard,
            reader: Some(reader),
        }
    }

    /// A daemon with nowhere to write what it says.
    // Dead in a test binary that hears every daemon it starts.
    #[allow(dead_code)]
    pub fn unheard(arguments: &[&OsStr], session: &[(&str, &OsStr)]) -> Self {
        Self::saying_to(arguments, session, nowhere())
    }

    /// A daemon that says what it says to `unheard`, which the test does not
    /// hear.
    pub fn saying_to(arguments: &[&OsStr], session: &[(&str, &OsStr)], unheard: Stdio) -> Self {
        let running = the_binary(arguments, session, Descriptors::Plenty)
            .stderr(unheard)
            .spawn()
            .expect("the binary runs");
        // Nothing sends, so a test that waits for a line is told at once
        // that there is none.
        let (_nothing, heard) = mpsc::channel();

        Self {
            running,
            heard,
            reader: None,
        }
    }

    /// What it says as it starts: every line up to the one that says where
    /// it records, and every line it says where it ends before that one.
    pub fn says(&self) -> Vec<String> {
        let mut said = Vec::new();
        while let Some(line) = self.says_next() {
            let started = line.starts_with(STARTED);
            said.push(line);
            if started {
                break;
            }
        }

        said
    }

    /// The next line it says, and nothing where it says none in time.
    pub fn says_next(&self) -> Option<String> {
        self.heard.recv_timeout(PATIENCE).ok()
    }

    /// Sends it a signal, named as `kill` names it.
    // Dead in a test binary that sends no daemon a signal.
    #[allow(dead_code)]
    pub fn is_sent(&self, signal: &str) {
        let sent = Command::new("kill")
            .args(["-s", signal, &self.running.id().to_string()])
            .status()
            .expect("`kill` runs");

        assert!(sent.success(), "`kill` could not send {signal}");
    }

    /// How it ended, and nothing where it is still running.
    pub fn ended(&mut self) -> Option<ExitStatus> {
        self.ends_within(PATIENCE)
    }

    /// How it ended where it ended within `patience`, and nothing where it
    /// was still running by then.
    pub fn ends_within(&mut self, patience: Duration) -> Option<ExitStatus> {
        ends_within(&mut self.running, patience)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // None of these is worth a panic inside a panic: a daemon that has
        // ended cannot be stopped, and one that cannot be stopped fails
        // nothing here.
        drop(self.running.kill());
        drop(self.running.wait());
        if let Some(reader) = self.reader.take() {
            drop(reader.join());
        }
    }
}

/// What a client is answered that asks the daemon for its sources.
///
/// A client that is refused, or that gave up waiting, asks again for as long
/// as the daemon runs, up to `PATIENCE`. It is refused until the socket is
/// bound, and the file of the socket is there before that; once the socket is
/// bound its connection waits in the backlog until the daemon serves it, and
/// it gives up on that wait at the client's own deadline.
// Dead in a test binary that asks no daemon for its sources.
#[allow(dead_code)]
pub fn sources_of(daemon: &mut Daemon, socket: &Path) -> Run {
    let waited_from = Instant::now();
    loop {
        let mut client = the_binary(
            &[
                OsStr::new("sources"),
                OsStr::new("--socket"),
                socket.as_os_str(),
            ],
            &[],
            Descriptors::Plenty,
        );
        client.stderr(Stdio::piped());
        let asked = run(client);
        let ended = daemon.ends_within(Duration::ZERO).is_some();
        if asked.code == SUCCEEDED || ended || waited_from.elapsed() > PATIENCE {
            return asked;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// What a daemon says as it starts in this session, and how it ended where
/// it did not start.
// Dead in a test binary that reads on after a daemon has started.
#[allow(dead_code)]
pub fn the_daemon_says(session: &[(&str, &OsStr)]) -> (Vec<String>, Option<ExitStatus>) {
    let mut daemon = Daemon::start(session);
    let said = daemon.says();
    let started = said.last().is_some_and(|line| line.starts_with(STARTED));
    let ended = if started { None } else { daemon.ended() };

    (said, ended)
}
