//! What the binary does where what it says cannot be written.
//!
//! A daemon that outlives the terminal it was started from writes to a
//! terminal that has gone, and the write fails. So does one to a pipe whose
//! reader has gone and to a file on a disk that is full. A line that cannot be
//! written has nobody to read it, so the binary goes on as it does where the
//! line is read: a daemon listens and answers, and what ends with a failure
//! ends with that failure.
//!
//! The binary itself is run, in a session the test chose. A run that a test
//! does not hear says things to `/dev/full`. A daemon is asked for its sources
//! by the binary run as a client, which is answered once the daemon serves its
//! socket.

#![cfg(target_os = "linux")]

mod common;

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use common::{
    Daemon, Descriptors, Directory, NO_SOURCE, NO_TRAY, SUCCEEDED, nowhere, run, sources_of,
    the_binary,
};

/// How a process ends on Linux whose `main` answered `ExitCode::FAILURE`.
const FAILED: Option<i32> = Some(1);

/// What the binary says of a runtime it has one descriptor to start with.
const NO_RUNTIME: &str =
    "benshi: the runtime could not be started: Too many open files (os error 24)\n";

/// How long after it serves its socket a daemon with no session bus has said
/// twice that detection is started again, with as much again to spare: it
/// says so as it starts detection and a second after.
const SAID_TWICE: Duration = Duration::from_secs(2);

/// Holds that the daemon answers a client and is running two seconds later,
/// by when it has twice had something to say of detection.
fn goes_on(daemon: &mut Daemon, socket: &Path) {
    let asked = sources_of(daemon, socket);

    assert_eq!(
        (asked.code, asked.shown.as_str()),
        (SUCCEEDED, NO_SOURCE),
        "the client said {:?} of a daemon that ended with {:?}",
        asked.said,
        daemon.ends_within(Duration::ZERO)
    );
    assert_eq!(
        daemon.ends_within(SAID_TWICE).map(|status| status.code()),
        None,
        "the daemon ended over a line it could not write"
    );
}

/// How the binary ends in this session with nowhere to say why.
fn ends_unheard(
    arguments: &[&OsStr],
    session: &[(&str, &OsStr)],
    with: Descriptors,
) -> Option<i32> {
    let mut binary = the_binary(arguments, session, with);
    binary.stderr(nowhere());

    run(binary).code
}

#[test]
fn a_daemon_that_cannot_say_where_it_listens_goes_on() {
    let temporary = Directory::of("n-listens");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let mut daemon = Daemon::unheard(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
    );

    goes_on(&mut daemon, &runtime.join("benshi.sock"));
}

#[test]
fn a_daemon_that_cannot_say_there_is_no_tray_goes_on() {
    let temporary = Directory::of("n-tray");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let mut daemon = Daemon::unheard(
        &[],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
    );

    goes_on(&mut daemon, &runtime.join("benshi.sock"));
}

#[test]
fn a_daemon_that_cannot_say_where_its_socket_went_goes_on() {
    let temporary = Directory::of("n-fallback");
    let data = temporary.path().join("data");
    let mut daemon = Daemon::unheard(
        &[OsStr::new(NO_TRAY)],
        &[
            ("TMPDIR", temporary.path().as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
    );

    goes_on(
        &mut daemon,
        &temporary.path().join("benshi").join("benshi.sock"),
    );
}

#[test]
fn a_daemon_with_nowhere_to_record_ends_as_a_failure_unheard() {
    let temporary = Directory::of("n-nowhere");
    let runtime = temporary.path().join("run");

    let ended = ends_unheard(
        &[OsStr::new(NO_TRAY)],
        &[("XDG_RUNTIME_DIR", runtime.as_os_str())],
        Descriptors::Plenty,
    );

    assert_eq!(ended, FAILED);
}

#[test]
fn a_daemon_whose_database_cannot_be_opened_ends_as_a_failure_unheard() {
    let temporary = Directory::of("n-unopened");
    let runtime = temporary.path().join("run");
    let in_the_way = temporary.path().join("a-file");
    fs::write(&in_the_way, b"").expect("a file can be written");
    let data = in_the_way.join("data");

    let ended = ends_unheard(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        Descriptors::Plenty,
    );

    assert_eq!(ended, FAILED);
}

#[test]
fn a_daemon_that_cannot_listen_ends_as_a_failure_unheard() {
    let temporary = Directory::of("n-second");
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

    let ended = ends_unheard(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            (
                "BENSHI_DATA_DIR",
                temporary.path().join("second").as_os_str(),
            ),
        ],
        Descriptors::Plenty,
    );

    assert_eq!(ended, FAILED);
}

#[test]
fn a_daemon_whose_runtime_cannot_be_started_says_so_and_ends() {
    let temporary = Directory::of("n-runtime");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let mut daemon = the_binary(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        Descriptors::One,
    );
    daemon.stderr(Stdio::piped());

    let ended = run(daemon);

    assert_eq!((ended.code, ended.said.as_str()), (FAILED, NO_RUNTIME));
}

#[test]
fn a_daemon_whose_runtime_cannot_be_started_ends_as_a_failure_unheard() {
    let temporary = Directory::of("n-runtime-u");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");

    let ended = ends_unheard(
        &[OsStr::new(NO_TRAY)],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
        Descriptors::One,
    );

    assert_eq!(ended, FAILED);
}

#[test]
fn a_client_that_cannot_say_where_it_looks_is_answered_as_one_that_can() {
    // The session names no runtime directory, so the client has that to say
    // before it asks.
    let temporary = Directory::of("n-client");
    let data = temporary.path().join("data");
    let socket = temporary.path().join("benshi").join("benshi.sock");
    let session = [("TMPDIR", temporary.path().as_os_str())];
    let daemon = Daemon::start(&[
        ("TMPDIR", temporary.path().as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);
    let started = daemon.says();
    assert_eq!(started.len(), 3, "the daemon started: {started:?}");
    let mut heard = the_binary(&[OsStr::new("sources")], &session, Descriptors::Plenty);
    heard.stderr(Stdio::piped());
    let mut unheard = the_binary(&[OsStr::new("sources")], &session, Descriptors::Plenty);
    unheard.stderr(nowhere());

    let heard = run(heard);
    let unheard = run(unheard);

    assert_eq!(
        heard.said,
        format!(
            "benshi: `XDG_RUNTIME_DIR` is not set, so the socket is {}\n",
            socket.display()
        )
    );
    assert_eq!((heard.code, heard.shown.as_str()), (SUCCEEDED, NO_SOURCE));
    assert_eq!(
        (unheard.code, unheard.shown.as_str()),
        (SUCCEEDED, NO_SOURCE)
    );
}

#[test]
fn a_client_whose_runtime_cannot_be_started_says_so_and_ends() {
    let temporary = Directory::of("n-c-runtime");
    let runtime = temporary.path().join("run");
    let mut client = the_binary(
        &[OsStr::new("sources")],
        &[("XDG_RUNTIME_DIR", runtime.as_os_str())],
        Descriptors::One,
    );
    client.stderr(Stdio::piped());

    let ended = run(client);

    assert_eq!((ended.code, ended.said.as_str()), (FAILED, NO_RUNTIME));
}

#[test]
fn a_client_whose_runtime_cannot_be_started_ends_as_a_failure_unheard() {
    let temporary = Directory::of("n-c-runtime-u");
    let runtime = temporary.path().join("run");

    let ended = ends_unheard(
        &[OsStr::new("sources")],
        &[("XDG_RUNTIME_DIR", runtime.as_os_str())],
        Descriptors::One,
    );

    assert_eq!(ended, FAILED);
}
