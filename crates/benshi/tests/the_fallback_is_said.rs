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

mod common;

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use common::{Directory, the_daemon_says};

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
    let data = temporary.path().join("data");

    let (said, _ended) = the_daemon_says(&[
        ("TMPDIR", temporary.path().as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);

    assert_eq!(
        said,
        [
            format!(
                "benshi: `XDG_RUNTIME_DIR` is not set, so the socket is {}",
                socket.display()
            ),
            format!("benshi: listening on {}", socket.display()),
            format!("benshi: recording in {}", data.join("benshi.db").display()),
        ]
    );
}

#[test]
fn the_daemon_says_what_is_wrong_with_the_runtime_directory_it_was_given() {
    let temporary = Directory::of("d-relative");
    let socket = temporary.path().join("benshi").join("benshi.sock");
    let data = temporary.path().join("data");

    let (said, _ended) = the_daemon_says(&[
        ("TMPDIR", temporary.path().as_os_str()),
        ("XDG_RUNTIME_DIR", OsStr::new("run/1000")),
        ("BENSHI_DATA_DIR", data.as_os_str()),
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
            format!("benshi: recording in {}", data.join("benshi.db").display()),
        ]
    );
}

#[test]
fn the_daemon_says_nothing_of_a_runtime_directory_it_uses() {
    let temporary = Directory::of("d-runtime");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");

    let (said, _ended) = the_daemon_says(&[
        ("TMPDIR", temporary.path().as_os_str()),
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);

    assert_eq!(
        said,
        [
            format!(
                "benshi: listening on {}",
                runtime.join("benshi.sock").display()
            ),
            format!("benshi: recording in {}", data.join("benshi.db").display()),
        ]
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
