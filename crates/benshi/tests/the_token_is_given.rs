//! What the daemon says of a token for AniList given from the environment.
//!
//! The settings of these tests read the list at no time, so that no daemon
//! here asks AniList anything.
//!
//! The binary itself is run, in a session the test chose, because what is
//! under test is what a person reads.

#![cfg(target_os = "linux")]

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use common::{Daemon, Directory};

/// The settings of a daemon that reads the list at no time.
const READING_NOTHING: &str = "[list]\nrefresh = \"off\"\nat_start = false\non_miss = \"off\"\n";

/// A session of its own in `temporary`, with settings that read the list at
/// no time.
fn a_session(temporary: &Path) -> Vec<(&'static str, std::ffi::OsString)> {
    let config = temporary.join("config");
    fs::create_dir_all(config.join("benshi")).expect("a configuration directory");
    fs::write(config.join("benshi").join("config.toml"), READING_NOTHING)
        .expect("a file of settings");

    vec![
        ("XDG_RUNTIME_DIR", temporary.join("run").into_os_string()),
        ("XDG_CONFIG_HOME", config.into_os_string()),
        ("BENSHI_DATA_DIR", temporary.join("data").into_os_string()),
    ]
}

/// What a daemon in this session says next once it has said where it
/// records.
fn said_after_starting(session: &[(&str, &OsStr)]) -> Option<String> {
    let daemon = Daemon::start(session);
    let started = daemon.says();
    assert!(
        started
            .last()
            .is_some_and(|line| line.starts_with("benshi: recording in ")),
        "{started:?}"
    );

    daemon.says_next()
}

#[test]
fn a_daemon_given_a_token_from_the_environment_says_so_as_it_starts() {
    let temporary = Directory::of("k-given");
    let mut session = a_session(temporary.path());
    session.push(("BENSHI_TOKEN_ANILIST", "a token".into()));
    let session: Vec<(&str, &OsStr)> = session
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();

    let next = said_after_starting(&session);

    assert_eq!(
        next.as_deref(),
        Some("benshi: AniList token from BENSHI_TOKEN_ANILIST")
    );
}

#[test]
fn a_daemon_given_an_empty_token_says_nothing_of_one_as_it_starts() {
    let temporary = Directory::of("k-empty");
    let mut session = a_session(temporary.path());
    session.push(("BENSHI_TOKEN_ANILIST", " ".into()));
    let session: Vec<(&str, &OsStr)> = session
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();

    let next = said_after_starting(&session);

    assert!(
        next.as_deref()
            .is_none_or(|line| !line.starts_with("benshi: AniList token")),
        "{next:?}"
    );
}

#[test]
fn a_daemon_given_no_token_says_nothing_of_one_as_it_starts() {
    let temporary = Directory::of("k-none");
    let session = a_session(temporary.path());
    let session: Vec<(&str, &OsStr)> = session
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();

    let next = said_after_starting(&session);

    assert!(
        next.as_deref()
            .is_none_or(|line| !line.starts_with("benshi: AniList token")),
        "{next:?}"
    );
}
