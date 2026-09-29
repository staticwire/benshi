//! What the daemon says of the database it records in, and what it leaves.
//!
//! The database is opened before anything starts, as the socket is bound, so
//! that a daemon with nowhere to record says so and ends where it would
//! otherwise detect and record nothing.
//!
//! The binary itself is run, in a session the test chose, because what is
//! under test is what a person reads.

#![cfg(target_os = "linux")]

mod common;

use std::fs;

use benshi_store::{SCHEMA_VERSION, Store};
use common::{Daemon, Directory, the_daemon_says};

/// How a process ends on Linux whose `main` answered `ExitCode::FAILURE`.
const FAILED: Option<i32> = Some(1);

#[test]
fn the_daemon_says_where_it_records_and_the_database_is_there() {
    let temporary = Directory::of("s-records");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let database = data.join("benshi.db");

    let (said, _ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_DATA_HOME", temporary.path().join("share").as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);

    assert_eq!(
        said,
        [
            format!(
                "benshi: listening on {}",
                runtime.join("benshi.sock").display()
            ),
            format!("benshi: recording in {}", database.display()),
        ]
    );
    // Looked for before it is opened, because opening makes a file that is
    // not there.
    assert!(database.is_file(), "no file at {}", database.display());
    let opened = Store::open(&database).expect("the database the daemon made opens");
    assert_eq!(opened.schema_version(), SCHEMA_VERSION);
}

#[test]
fn the_database_is_in_the_data_directory_of_the_session() {
    let temporary = Directory::of("s-default");
    let runtime = temporary.path().join("run");
    let share = temporary.path().join("share");
    let database = share.join("benshi").join("benshi.db");

    let (said, _ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_DATA_HOME", share.as_os_str()),
    ]);

    assert_eq!(
        said.last(),
        Some(&format!("benshi: recording in {}", database.display())),
        "{said:?}"
    );
    assert!(database.is_file(), "no file at {}", database.display());
}

#[test]
fn a_daemon_with_nowhere_to_record_says_so_and_ends() {
    let temporary = Directory::of("s-nowhere");
    let runtime = temporary.path().join("run");

    let (said, ended) = the_daemon_says(&[("XDG_RUNTIME_DIR", runtime.as_os_str())]);

    assert_eq!(
        said,
        [
            "benshi: no data directory: no absolute path in `BENSHI_DATA_DIR`, `XDG_DATA_HOME` \
          or `HOME`"
        ]
    );
    assert_eq!(ended.and_then(|status| status.code()), FAILED);
}

#[test]
fn a_daemon_whose_database_cannot_be_opened_says_why_and_ends() {
    let temporary = Directory::of("s-unopened");
    let runtime = temporary.path().join("run");
    let in_the_way = temporary.path().join("a-file");
    fs::write(&in_the_way, b"").expect("a file can be written");
    let data = in_the_way.join("data");

    let (said, ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);

    assert_eq!(
        said,
        [format!(
            "benshi: {} cannot be used: {} could not be created: Not a directory (os error 20)",
            data.join("benshi.db").display(),
            data.display()
        )]
    );
    assert_eq!(ended.and_then(|status| status.code()), FAILED);
}

#[test]
fn a_daemon_that_cannot_listen_opens_no_database() {
    // The socket is what says a daemon is running, so it is bound first. A
    // second daemon, of another build, would otherwise bring the database to
    // its own schema under the one that is writing to it.
    let temporary = Directory::of("s-second");
    let runtime = temporary.path().join("run");
    let first = Daemon::start(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        (
            "BENSHI_DATA_DIR",
            temporary.path().join("first").as_os_str(),
        ),
    ]);
    let started = first.says();
    assert_eq!(started.len(), 2, "the first daemon started: {started:?}");
    let untouched = temporary.path().join("second");

    let (said, ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("BENSHI_DATA_DIR", untouched.as_os_str()),
    ]);

    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].starts_with(&format!(
            "benshi: {} cannot be used: ",
            runtime.join("benshi.sock").display()
        )),
        "{said:?}"
    );
    assert_eq!(ended.and_then(|status| status.code()), FAILED);
    assert!(
        !untouched.exists(),
        "a daemon that could not listen made {}",
        untouched.display()
    );
}
