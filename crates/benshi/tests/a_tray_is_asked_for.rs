//! What the daemon says of a tray that was asked for.
//!
//! A bare `benshi` asks for a daemon with a tray, and there is no tray. The
//! daemon says so and runs without one.
//!
//! The binary itself is run, in a session the test chose, because what is
//! under test is what a person reads.

#![cfg(target_os = "linux")]

mod common;

use common::{Daemon, Directory};

#[test]
fn a_daemon_asked_for_a_tray_says_that_there_is_none() {
    let temporary = Directory::of("y-tray");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let daemon = Daemon::asked(
        &[],
        &[
            ("XDG_RUNTIME_DIR", runtime.as_os_str()),
            ("BENSHI_DATA_DIR", data.as_os_str()),
        ],
    );

    let said = daemon.says();

    assert_eq!(
        said,
        [
            "benshi: there is no tray yet, running without one".to_owned(),
            format!(
                "benshi: listening on {}",
                runtime.join("benshi.sock").display()
            ),
            format!("benshi: recording in {}", data.join("benshi.db").display()),
        ]
    );
}
