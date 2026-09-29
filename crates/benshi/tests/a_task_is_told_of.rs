//! What the binary says of a task of its daemon, as it happens.
//!
//! A daemon whose detection has stopped goes on answering its socket, so what
//! happened to detection is said where a person reads it and when it
//! happens.
//!
//! The binary itself is run, in a session the test chose. No session bus is
//! there, so the first attempt of detection fails in a way that passes.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use common::{Daemon, Directory};

/// What the daemon says of the first attempt of detection in a session with
/// no session bus, up to the reason.
const STARTED_AGAIN: &str = "benshi: detection failed and is started again in 1s: ";

/// How long after that the second attempt has failed and been said, with as
/// much again to spare: it is started a second after the first.
const SAID_TWICE: Duration = Duration::from_secs(2);

#[test]
fn the_daemon_says_that_detection_is_started_again() {
    let temporary = Directory::of("t-again");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let daemon = Daemon::start(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);
    let started = daemon.says();
    assert_eq!(started.len(), 2, "the daemon started: {started:?}");

    let said = daemon
        .says_next()
        .expect("the daemon said something once it had started");

    assert!(said.starts_with(STARTED_AGAIN), "{said}");
}

#[test]
fn a_daemon_that_cannot_say_it_goes_on_running() {
    // A daemon that outlives the terminal it was started from writes to a
    // terminal that has gone, and the write fails. This one is read for its
    // two lines and the first it says of detection, and the second is the
    // one it cannot write.
    let temporary = Directory::of("t-unheard");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let mut daemon = Daemon::heard_for(
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        3,
    );
    let started = daemon.says();
    assert_eq!(started.len(), 2, "the daemon started: {started:?}");
    let said = daemon.says_next().expect("the daemon said something");
    assert!(said.starts_with(STARTED_AGAIN), "{said}");

    let ended = daemon.ends_within(SAID_TWICE);

    assert_eq!(
        ended.map(|status| status.code()),
        None,
        "the daemon ended over a line it could not write"
    );
}
