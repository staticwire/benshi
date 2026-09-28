//! What the binary says where the session names no runtime directory.
//!
//! The XDG Base Directory Specification asks an application that falls back on
//! a replacement for the runtime directory to print a warning. Both halves of
//! the binary work the socket's path out, so both say it: the daemon before it
//! binds, and the client before it connects.
//!
//! The binary itself is run, in a session the test chose, because what is
//! under test is what a person reads.

#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long a test waits for the daemon to say something before calling it
/// silent. Only ever spent on a failure.
const PATIENCE: Duration = Duration::from_secs(10);

/// A session bus that is not there, so that a daemon under test reaches no
/// player of the person running the tests.
const NO_BUS: &str = "unix:path=/nowhere/bus";

/// A directory of one test in one run of the tests, gone when the test is
/// over.
///
/// Named by the process as well as by the test, so that two runs of the tests
/// at once do not bind in one directory.
struct Directory(PathBuf);

impl Directory {
    fn of(test: &str) -> Self {
        let directory =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("the directory can be made");

        Self(directory)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // Whatever is left in it is a socket of a daemon that was stopped.
        // A directory that stays behind fails nothing, so a failure to remove
        // it is not worth a panic inside a panic.
        drop(fs::remove_dir_all(&self.0));
    }
}

/// What the daemon says in this session up to the line that says it listens,
/// that line included.
fn the_daemon_says(session: &[(&str, &OsStr)]) -> Vec<String> {
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_benshi"))
        .arg("--no-tray")
        .env_clear()
        .envs(session.iter().copied())
        .env("DBUS_SESSION_BUS_ADDRESS", NO_BUS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    let said = daemon.stderr.take().expect("what it says is piped");
    let (lines, heard) = mpsc::channel();
    // Read on a thread of its own, so that a daemon that says nothing fails
    // the test where it would otherwise hold it for ever.
    let reader = thread::spawn(move || {
        for line in BufReader::new(said).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let mut said = Vec::new();
    while let Ok(line) = heard.recv_timeout(PATIENCE) {
        let listening = line.starts_with("benshi: listening on ");
        said.push(line);
        if listening {
            break;
        }
    }

    daemon.kill().expect("the daemon can be stopped");
    daemon.wait().expect("the daemon has stopped");
    drop(heard);
    reader.join().expect("the reader has finished");

    said
}

/// What the client says in this session, asked for the sources.
fn the_client_says(session: &[(&str, &OsStr)], arguments: &[&OsStr]) -> Vec<String> {
    let client = Command::new(env!("CARGO_BIN_EXE_benshi"))
        .arg("sources")
        .args(arguments)
        .env_clear()
        .envs(session.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");

    String::from_utf8_lossy(&client.stderr)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_daemon_says_that_the_runtime_directory_is_not_set() {
    let temporary = Directory::of("d-none");
    let socket = temporary.path().join("benshi").join("benshi.sock");

    let said = the_daemon_says(&[("TMPDIR", temporary.path().as_os_str())]);

    assert_eq!(
        said,
        [
            format!(
                "benshi: `XDG_RUNTIME_DIR` is not set, so the socket is {}",
                socket.display()
            ),
            format!("benshi: listening on {}", socket.display()),
        ]
    );
}

#[test]
fn the_daemon_says_what_is_wrong_with_the_runtime_directory_it_was_given() {
    let temporary = Directory::of("d-relative");
    let socket = temporary.path().join("benshi").join("benshi.sock");

    let said = the_daemon_says(&[
        ("TMPDIR", temporary.path().as_os_str()),
        ("XDG_RUNTIME_DIR", OsStr::new("run/1000")),
    ]);

    assert_eq!(
        said,
        [
            format!(
                "benshi: `XDG_RUNTIME_DIR` holds `run/1000`, which is a relative path, so the \
                 socket is {}",
                socket.display()
            ),
            format!("benshi: listening on {}", socket.display()),
        ]
    );
}

#[test]
fn the_daemon_says_nothing_of_a_runtime_directory_it_uses() {
    let temporary = Directory::of("d-runtime");
    let runtime = temporary.path().join("run");

    let said = the_daemon_says(&[
        ("TMPDIR", temporary.path().as_os_str()),
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
    ]);

    assert_eq!(
        said,
        [format!(
            "benshi: listening on {}",
            runtime.join("benshi.sock").display()
        )]
    );
}

#[test]
fn the_client_says_that_the_runtime_directory_is_not_set() {
    let temporary = Directory::of("c-none");
    let socket = temporary.path().join("benshi").join("benshi.sock");

    let said = the_client_says(&[("TMPDIR", temporary.path().as_os_str())], &[]);

    assert_eq!(
        said.first(),
        Some(&format!(
            "benshi: `XDG_RUNTIME_DIR` is not set, so the socket is {}",
            socket.display()
        )),
        "{said:?}"
    );
    assert_eq!(said.len(), 2, "{said:?}");
}

#[test]
fn the_client_says_nothing_of_a_runtime_directory_it_uses() {
    let temporary = Directory::of("c-runtime");
    let runtime = temporary.path().join("run");

    let said = the_client_says(
        &[
            ("TMPDIR", temporary.path().as_os_str()),
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ],
        &[],
    );

    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains(&runtime.join("benshi.sock").display().to_string()),
        "{said:?}"
    );
}

#[test]
fn the_client_says_nothing_of_the_session_where_the_socket_was_named() {
    let temporary = Directory::of("c-named");
    let named = temporary.path().join("elsewhere.sock");

    let said = the_client_says(
        &[("TMPDIR", temporary.path().as_os_str())],
        &[OsStr::new("--socket"), named.as_os_str()],
    );

    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains(&named.display().to_string()), "{said:?}");
}
