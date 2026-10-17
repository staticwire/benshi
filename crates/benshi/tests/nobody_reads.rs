//! What the daemon does while nobody reads what it says.
//!
//! A pipe, a socket and a terminal each hold so much, and a write to one that
//! is full waits until it is read. A daemon that waited with it would serve
//! nobody on its socket and start no task again. So the daemon says what it
//! says without waiting for it to be written.
//!
//! The binary itself is run, in a session the test chose. Its stderr is a
//! stream socket that is full before the daemon has it, and the test keeps
//! the other end and does not read it. A service of systemd has a stream
//! socket for its stderr. The session bus is a socket of the test's that lets
//! a connection in and lets go of it, so an attempt of detection is a
//! connection the test counts. The settings of that daemon read the list at
//! no time, so the list's task makes no connection of its own.

#![cfg(target_os = "linux")]

mod common;

use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::{
    Daemon, Directory, NO_SOURCE, NO_TRAY, PATIENCE, SUCCEEDED, nobody_reads, reading_nothing,
    sources_of,
};

/// How many times detection has reached for the session bus once it has been
/// started again twice.
const AGAIN_TWICE: usize = 3;

/// A session bus that lets a connection in and lets go of it, so that an
/// attempt of detection fails in a way that passes.
struct Bus {
    listener: UnixListener,
    address: OsString,
}

impl Bus {
    fn at(socket: &Path) -> Self {
        let listener = UnixListener::bind(socket).expect("a socket can be bound");
        listener
            .set_nonblocking(true)
            .expect("a socket can be kept from waiting");
        // An address holds a letter, a digit and six marks as they are, and
        // any other byte as a percent sign and two hex digits.
        let path: String = socket
            .as_os_str()
            .as_bytes()
            .iter()
            .map(|&byte| match byte {
                b'0'..=b'9'
                | b'A'..=b'Z'
                | b'a'..=b'z'
                | b'-'
                | b'_'
                | b'/'
                | b'.'
                | b'\\'
                | b'*' => char::from(byte).to_string(),
                other => format!("%{other:02x}"),
            })
            .collect();

        Self {
            listener,
            address: format!("unix:path={path}").into(),
        }
    }

    /// How many times the bus was reached for, up to `times`, by the time
    /// that many have come or the test has lost its patience.
    fn reached(&self, times: usize) -> usize {
        let waited_from = Instant::now();
        let mut reached = 0;
        while reached < times && waited_from.elapsed() < PATIENCE {
            match self.listener.accept() {
                Ok(_let_go_of) => reached += 1,
                Err(nobody) if nobody.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(other) => panic!("the bus could not be listened on: {other}"),
            }
        }

        reached
    }
}

/// Holds that the daemon answers a client that asks for its sources.
fn answers(daemon: &mut Daemon, socket: &Path) {
    let asked = sources_of(daemon, socket);

    assert_eq!(
        (asked.code, asked.shown.as_str()),
        (SUCCEEDED, NO_SOURCE),
        "the client said {:?} of a daemon that ended with {:?}",
        asked.said,
        daemon.ends_within(Duration::ZERO)
    );
}

#[test]
fn a_daemon_nobody_reads_answers_a_client() {
    let temporary = Directory::of("u-listens");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let (_unread, stderr) = nobody_reads();
    let mut daemon = Daemon::saying_to(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        stderr,
    );

    answers(&mut daemon, &runtime.join("benshi.sock"));
}

#[test]
fn a_daemon_asked_for_a_tray_that_nobody_reads_answers_a_client() {
    let temporary = Directory::of("u-tray");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let (_unread, stderr) = nobody_reads();
    let mut daemon = Daemon::saying_to(
        &[],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        stderr,
    );

    answers(&mut daemon, &runtime.join("benshi.sock"));
}

#[test]
fn a_daemon_on_the_fallback_that_nobody_reads_answers_a_client() {
    let temporary = Directory::of("u-fallback");
    let data = temporary.path().join("data");
    let (_unread, stderr) = nobody_reads();
    let mut daemon = Daemon::saying_to(
        &[OsStr::new(NO_TRAY)],
        &[
            ("TMPDIR", temporary.path().as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        stderr,
    );

    answers(
        &mut daemon,
        &temporary.path().join("benshi").join("benshi.sock"),
    );
}

#[test]
fn a_daemon_nobody_reads_starts_detection_again() {
    let temporary = Directory::of("u-again");
    let runtime = temporary.path().join("run");
    let config = reading_nothing(temporary.path());
    let data = temporary.path().join("data");
    let bus = Bus::at(&temporary.path().join("bus"));
    let (_unread, stderr) = nobody_reads();
    let _daemon = Daemon::saying_to(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("XDG_CONFIG_HOME", config.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
            ("DBUS_SESSION_BUS_ADDRESS", bus.address.as_os_str()),
        ],
        stderr,
    );

    assert_eq!(
        bus.reached(AGAIN_TWICE),
        AGAIN_TWICE,
        "detection was held by a line nobody read"
    );
}
