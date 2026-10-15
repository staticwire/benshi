//! Where the secret store finds the session bus, and what it says where there
//! is none to find.
//!
//! A process cannot set its own environment without `unsafe`, so each test
//! here runs this binary again in a session it chose, with
//! `DBUS_SESSION_BUS_ADDRESS` naming a bus of the test's making.
//!
//! The tests are in a binary of their own: a child holds a copy of every
//! descriptor of the process that started it until it runs its own program,
//! and a listener a test beside it let go of would go on answering for that
//! long.

#![cfg(target_os = "linux")]

use std::env;
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;

use benshi_daemon::secrets::{Asking, Error, SecretService};

/// Set in a run a test started, and in no other.
const AGAIN: &str = "BENSHI_TEST_AGAIN";

/// A deadline that a test which waits for it spends whole.
const SHORT: Duration = Duration::from_millis(200);

/// A bound on a test whose call gives up on its own, so that one which does
/// not ends the test.
const BOUND: Duration = Duration::from_secs(5);

/// Run one test of this binary again, with this address for the session bus
/// and nothing else of this session.
fn again(name: &str, address: &str) {
    let run = Command::new(env::current_exe().expect("the tests know what runs them"))
        .args(["--exact", name])
        .env_clear()
        .env(AGAIN, "1")
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .output()
        .expect("the tests can be run again");
    let said = String::from_utf8_lossy(&run.stdout);

    assert!(run.status.success(), "{address}: {said}");
    // A name that matches no test is a run that passes.
    assert!(said.contains("1 passed"), "{address}: {said}");
}

/// What connecting in this session answers.
fn connected() -> Result<SecretService, Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(async {
            tokio::time::timeout(BOUND, SecretService::connect(SHORT, Asking::Never))
                .await
                .expect("connecting gives up on its own")
        })
}

#[test]
fn a_session_with_no_bus_has_no_store_to_reach() {
    if env::var_os(AGAIN).is_some() {
        let connected = connected();

        assert!(
            matches!(&connected, Err(Error::Unreachable(_))),
            "{connected:?}"
        );
        return;
    }

    again(
        "a_session_with_no_bus_has_no_store_to_reach",
        "unix:path=/nowhere/bus",
    );
}

#[test]
fn a_bus_that_does_not_answer_is_given_up_on_at_the_deadline() {
    if env::var_os(AGAIN).is_some() {
        let connected = connected();

        assert!(
            matches!(&connected, Err(Error::Timeout { deadline }) if *deadline == SHORT),
            "{connected:?}"
        );
        return;
    }

    // A socket that takes a connection into its backlog and never reads it,
    // so the bus's greeting is never answered.
    let directory = tempfile::tempdir().expect("a directory");
    let bus = directory.path().join("bus");
    let _listening = UnixListener::bind(&bus).expect("a socket that listens");

    again(
        "a_bus_that_does_not_answer_is_given_up_on_at_the_deadline",
        &format!("unix:path={}", bus.display()),
    );
}
