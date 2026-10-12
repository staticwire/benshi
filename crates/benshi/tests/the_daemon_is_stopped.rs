//! What the daemon does when it is told to stop, and how long it takes to
//! end.
//!
//! A signal that asks a process to end ends one that does nothing about it at
//! once: what it had said and not yet written is lost, and nothing says that
//! it was stopped and not killed. The daemon says what stopped it in its last
//! line and ends as a success. A signal that was ignored as the daemon
//! started stays ignored.
//!
//! The binary itself is run, in a session the test chose, and sent the signal
//! by `kill`.

#![cfg(target_os = "linux")]

mod common;

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use common::{Daemon, Directory, NO_TRAY, SUCCEEDED, nobody_reads, sources_of};

/// How a process ends on Linux whose `main` answered `ExitCode::FAILURE`.
const FAILED: Option<i32> = Some(1);

/// How long a daemon that has ended waits for its last lines to be read.
const LAST_WORDS: Duration = Duration::from_secs(5);

/// How much later than that a daemon nobody reads may end, on a machine that
/// is busy.
const SLACK: Duration = Duration::from_secs(2);

#[test]
fn a_daemon_sent_a_signal_says_what_stopped_it_and_ends_as_a_success() {
    for signal in ["TERM", "INT", "HUP"] {
        let temporary = Directory::of(&format!("s-{}", signal.to_lowercase()));
        let runtime = temporary.path().join("run");
        let data = temporary.path().join("data");
        let mut daemon = Daemon::start(&[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ]);
        let started = daemon.says();
        assert!(
            started
                .last()
                .is_some_and(|line| line.starts_with("benshi: recording in ")),
            "the daemon did not start: {started:?}"
        );

        let sent = Instant::now();
        daemon.is_sent(signal);

        let ended = daemon.ended().expect("a daemon that is stopped ends");
        assert_eq!(
            ended.code(),
            SUCCEEDED,
            "sent SIG{signal}, it ended {ended}"
        );
        // A daemon that is read has nothing to wait for.
        assert!(
            sent.elapsed() < LAST_WORDS,
            "sent SIG{signal} and read, it took {:?} to end",
            sent.elapsed()
        );
        // The pipe is let go of as the daemon ends, so the lines run out.
        let mut last = None;
        while let Some(line) = daemon.says_next() {
            last = Some(line);
        }
        assert_eq!(
            last.as_deref(),
            Some(format!("benshi: stopped by SIG{signal}").as_str())
        );
    }
}

#[test]
fn a_daemon_stopped_while_nobody_reads_waits_for_a_reader_and_then_ends() {
    // A voice that goes waits for what it holds to be written, and where
    // nobody reads that is for ever. A daemon that was stopped waits so long
    // and no longer.
    let temporary = Directory::of("s-unread");
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
    let asked = sources_of(&mut daemon, &runtime.join("benshi.sock"));
    assert_eq!(asked.code, SUCCEEDED, "the daemon did not start");

    let sent = Instant::now();
    daemon.is_sent("TERM");

    let ended = daemon
        .ends_within(LAST_WORDS + SLACK)
        .expect("a daemon that is stopped ends, read or not");
    assert_eq!(ended.code(), SUCCEEDED, "it ended {ended}");
    assert!(
        sent.elapsed() >= LAST_WORDS,
        "it waited {:?} for its last lines to be read",
        sent.elapsed()
    );
}

#[test]
fn a_daemon_that_failed_while_nobody_reads_waits_for_a_reader_and_then_ends() {
    // With no directory to record in the daemon says so and ends, and what
    // it said waits for a reader. No signal ends the process by then: each
    // of the three is taken as the daemon starts.
    let temporary = Directory::of("s-failed-unread");
    let runtime = temporary.path().join("run");
    let (_unread, stderr) = nobody_reads();
    let started = Instant::now();
    let mut daemon = Daemon::saying_to(
        &[OsStr::new(NO_TRAY)],
        &[("XDG_RUNTIME_DIR", runtime.as_os_str())],
        stderr,
    );

    let ended = daemon
        .ends_within(LAST_WORDS + SLACK)
        .expect("a daemon that failed ends, read or not");
    assert_eq!(ended.code(), FAILED, "it ended {ended}");
    assert!(
        started.elapsed() >= LAST_WORDS,
        "it waited {:?} for its last lines to be read",
        started.elapsed()
    );
}

#[test]
fn a_signal_ignored_as_the_daemon_started_does_not_stop_it() {
    // What `nohup` asks for with `HUP`, and a shell script that runs a job
    // in the background with `INT`.
    for (ignored, stopping) in [("HUP", "TERM"), ("INT", "TERM"), ("TERM", "INT")] {
        let temporary = Directory::of(&format!("s-ignored-{}", ignored.to_lowercase()));
        let runtime = temporary.path().join("run");
        let data = temporary.path().join("data");
        let mut daemon = Daemon::ignoring(
            ignored,
            &[
                ("XDG_RUNTIME_DIR", runtime.as_os_str()),
                ("BENSHI_DATA_DIR", data.as_os_str()),
            ],
        );
        let started = daemon.says();
        assert!(
            started
                .last()
                .is_some_and(|line| line.starts_with("benshi: recording in ")),
            "the daemon did not start: {started:?}"
        );

        daemon.is_sent(ignored);

        // Asked after the signal was sent: a daemon that answers was not
        // stopped by it.
        let asked = sources_of(&mut daemon, &runtime.join("benshi.sock"));
        assert_eq!(
            asked.code,
            SUCCEEDED,
            "started with SIG{ignored} ignored and sent it, the daemon said {:?}",
            daemon.says_next()
        );

        daemon.is_sent(stopping);
        let ended = daemon.ended().expect("a daemon that is stopped ends");
        assert_eq!(ended.code(), SUCCEEDED, "it ended {ended}");
        let mut last = None;
        while let Some(line) = daemon.says_next() {
            last = Some(line);
        }
        assert_eq!(
            last.as_deref(),
            Some(format!("benshi: stopped by SIG{stopping}").as_str())
        );
    }
}
