//! The binary run in a session a test chose.
//!
//! The binary itself is run, because what is under test is what a person
//! reads. A test binary that uses this opens no socket of its own: a process
//! it starts holds a copy of every descriptor open in it until that process
//! runs its own program.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a test waits for the binary to say something or to end before
/// calling it lost. Only ever spent on a failure.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// A session bus that is not there, so that a daemon under test reaches no
/// player of the person running the tests.
const NO_BUS: &str = "unix:path=/nowhere/bus";

/// What the last line a daemon says as it starts begins with.
const STARTED: &str = "benshi: recording in ";

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
    let mut binary = match descriptors {
        Descriptors::Plenty => Command::new(env!("CARGO_BIN_EXE_benshi")),
        Descriptors::One => {
            // Under a limit of four a descriptor is one of 0 to 3, and the
            // first three are the streams the binary is started with. The
            // shell lets go of 3 first, so that it is free whatever the
            // tests were started with.
            let mut shell = Command::new("/bin/sh");
            shell.args([
                "-c",
                "exec 3>&-; ulimit -n 4; exec \"$@\"",
                "sh",
                env!("CARGO_BIN_EXE_benshi"),
            ]);
            shell
        }
    };
    binary
        .args(arguments)
        .env_clear()
        .envs(session.iter().copied())
        .env("DBUS_SESSION_BUS_ADDRESS", NO_BUS)
        .stdin(Stdio::null())
        .stdout(Stdio::null());

    binary
}

/// Somewhere that cannot be written to. Every write to `/dev/full` fails, as
/// one does to a terminal that has gone and to a pipe whose reader has gone.
// Dead in a test binary that hears everything it starts.
#[allow(dead_code)]
pub fn nowhere() -> Stdio {
    File::options()
        .write(true)
        .open("/dev/full")
        .expect("`/dev/full` opens for writing")
        .into()
}

/// One run of the binary that was waited for until it ended.
// Dead where `run` is.
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
// Dead in a test binary that runs the binary through `Daemon` alone.
#[allow(dead_code)]
pub fn run(mut binary: Command) -> Run {
    let mut running = binary
        .stdout(Stdio::piped())
        .spawn()
        .expect("the binary runs");
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
// Dead where `run` is.
#[allow(dead_code)]
fn heard(written: Option<impl Read>) -> String {
    let mut all = String::new();
    if let Some(mut written) = written {
        written
            .read_to_string(&mut all)
            .expect("what the binary wrote is text");
    }

    all
}

/// A directory of one test in one run of the tests, gone when the test is
/// over.
///
/// Named by the process as well as by the test, so that two runs of the tests
/// at once do not bind in one directory.
pub struct Directory(PathBuf);

impl Directory {
    pub fn of(test: &str) -> Self {
        let directory =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("the directory can be made");

        Self(directory)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // Whatever is left in it is what a daemon that was stopped left. A
        // directory that stays behind fails nothing, so a failure to remove
        // it is not worth a panic inside a panic.
        drop(fs::remove_dir_all(&self.0));
    }
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

    fn hearing(arguments: &[&OsStr], session: &[(&str, &OsStr)], limit: usize) -> Self {
        let mut running = the_binary(arguments, session, Descriptors::Plenty)
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
        let running = the_binary(arguments, session, Descriptors::Plenty)
            .stderr(nowhere())
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

    /// How it ended, and nothing where it is still running.
    // Dead in a test binary that stops every daemon it starts.
    #[allow(dead_code)]
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
