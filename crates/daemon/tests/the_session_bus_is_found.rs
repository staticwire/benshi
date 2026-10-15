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
use std::future::Future;
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;

use benshi_core::token::Token;
use benshi_daemon::accounts::{self, Accounts, OnAniList};
use benshi_daemon::listing::{FromAniList, Source};
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

/// What `call` comes to in this session.
fn answered<T>(call: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(async {
            tokio::time::timeout(BOUND, call)
                .await
                .expect("the call gives up on its own")
        })
}

/// What connecting in this session answers.
fn connected() -> Result<SecretService, Error> {
    answered(SecretService::connect(SHORT, Asking::Never))
}

/// The address of a service at a port of this machine that nobody listens
/// on, held by the socket handed back, so that no test here reaches
/// AniList.
fn nowhere() -> (String, tokio::net::TcpSocket) {
    let start = u16::try_from(std::process::id() % 20_000).expect("below 20000");
    for offset in 0..20_000 {
        let port = 20_000 + (start + offset) % 20_000;
        let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
        if socket.bind(([127, 0, 0, 1], port).into()).is_ok() {
            return (format!("http://127.0.0.1:{port}/"), socket);
        }
    }
    panic!("no port between 20000 and 40000 is free");
}

/// Logins checked with [`nowhere`].
fn accounts() -> (OnAniList, tokio::net::TcpSocket) {
    let (address, held) = nowhere();

    (OnAniList::new(&address), held)
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

#[test]
fn a_login_in_a_session_with_no_bus_says_the_token_was_not_kept() {
    if env::var_os(AGAIN).is_some() {
        let (accounts, _held) = accounts();
        let logged = answered(accounts.login(Token::new("a token")));

        assert!(
            matches!(
                &logged,
                Err(accounts::Error::NotKept(Error::Unreachable(_)))
            ),
            "{logged:?}"
        );
        return;
    }

    again(
        "a_login_in_a_session_with_no_bus_says_the_token_was_not_kept",
        "unix:path=/nowhere/bus",
    );
}

#[test]
fn a_logout_in_a_session_with_no_bus_says_the_token_was_not_removed() {
    if env::var_os(AGAIN).is_some() {
        let (accounts, _held) = accounts();
        let logged_out = answered(accounts.logout());

        assert!(
            matches!(
                &logged_out,
                Err(accounts::Error::NotRemoved(Error::Unreachable(_)))
            ),
            "{logged_out:?}"
        );
        return;
    }

    again(
        "a_logout_in_a_session_with_no_bus_says_the_token_was_not_removed",
        "unix:path=/nowhere/bus",
    );
}

#[test]
fn a_token_given_from_the_environment_is_read_without_asking_the_store() {
    if env::var_os(AGAIN).is_some() {
        let (address, _held) = nowhere();
        let source = FromAniList::new(&address, Some(Token::new("given")));

        let token = answered(source.token()).expect("a token");

        assert_eq!(token.as_ref().map(Token::secret), Some("given"));
        return;
    }

    again(
        "a_token_given_from_the_environment_is_read_without_asking_the_store",
        "unix:path=/nowhere/bus",
    );
}

#[test]
fn a_token_with_none_given_is_asked_of_the_store() {
    if env::var_os(AGAIN).is_some() {
        let (address, _held) = nowhere();
        let source = FromAniList::new(&address, None);

        let token = answered(source.token());

        assert!(matches!(&token, Err(Error::Unreachable(_))), "{token:?}");
        return;
    }

    again(
        "a_token_with_none_given_is_asked_of_the_store",
        "unix:path=/nowhere/bus",
    );
}

#[test]
fn a_list_is_read_from_the_address_given() {
    let (address, _held) = nowhere();
    let source = FromAniList::new(&address, None);

    let list = answered(source.list(&Token::new("a token")));

    assert!(
        matches!(&list, Err(benshi_sync::Error::Unreachable(_))),
        "{list:?}"
    );
}
