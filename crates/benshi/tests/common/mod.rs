//! The binary run as a daemon, in a session a test chose.
//!
//! The binary itself is run, because what is under test is what a person
//! reads. A test binary that uses this opens no socket of its own: a process
//! it starts holds a copy of every descriptor open in it until that process
//! runs its own program.

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a test waits for the daemon to say something or to end before
/// calling it lost. Only ever spent on a failure.
const PATIENCE: Duration = Duration::from_secs(10);

/// A session bus that is not there, so that a daemon under test reaches no
/// player of the person running the tests.
const NO_BUS: &str = "unix:path=/nowhere/bus";

/// What the last line a daemon says as it starts begins with.
const STARTED: &str = "benshi: recording in ";

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
        let mut running = Command::new(env!("CARGO_BIN_EXE_benshi"))
            .arg("--no-tray")
            .env_clear()
            .envs(session.iter().copied())
            .env("DBUS_SESSION_BUS_ADDRESS", NO_BUS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
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
        let waited_from = Instant::now();
        loop {
            let ended = self.running.try_wait().expect("the daemon can be asked");
            if ended.is_some() || waited_from.elapsed() > patience {
                return ended;
            }
            thread::sleep(Duration::from_millis(10));
        }
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
