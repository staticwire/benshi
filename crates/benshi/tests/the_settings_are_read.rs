//! What the daemon says of the settings it runs on, and where a file of
//! settings it cannot use leaves it.
//!
//! The file is read before the socket is bound and the database opened, so
//! a daemon that cannot use it ends having touched neither.
//!
//! The binary itself is run, in a session the test chose, because what is
//! under test is what a person reads.

#![cfg(target_os = "linux")]

mod common;

use std::fs;

use common::{Directory, the_daemon_says};

/// How a process ends on Linux whose `main` answered `ExitCode::FAILURE`.
const FAILED: Option<i32> = Some(1);

#[test]
fn the_daemon_says_which_file_its_settings_are_read_from() {
    let temporary = Directory::of("g-read");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let config = temporary.path().join("config");
    let file = config.join("benshi").join("config.toml");
    fs::create_dir_all(config.join("benshi")).expect("a configuration directory");
    fs::write(&file, "[send]\nwhen = \"manual\"\n").expect("a file of settings");

    let (said, _ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);

    assert_eq!(
        said,
        [
            format!("benshi: settings from {}", file.display()),
            format!(
                "benshi: listening on {}",
                runtime.join("benshi.sock").display()
            ),
            format!("benshi: recording in {}", data.join("benshi.db").display()),
        ]
    );
}

#[test]
fn a_daemon_with_no_file_of_settings_says_it_runs_on_the_defaults() {
    let temporary = Directory::of("g-none");
    let runtime = temporary.path().join("run");
    let config = temporary.path().join("config");

    let (said, _ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("BENSHI_DATA_DIR", temporary.path().join("data").as_os_str()),
    ]);

    assert_eq!(
        said.first(),
        Some(&format!(
            "benshi: default settings: no {}",
            config.join("benshi").join("config.toml").display()
        )),
        "{said:?}"
    );
}

#[test]
fn a_daemon_whose_settings_cannot_be_used_says_why_and_ends_before_it_listens() {
    let temporary = Directory::of("g-unusable");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    let config = temporary.path().join("config");
    let file = config.join("benshi").join("config.toml");
    fs::create_dir_all(config.join("benshi")).expect("a configuration directory");
    fs::write(&file, "[send]\nwhen = \"sometimes\"\n").expect("a file of settings");

    let (said, ended) = the_daemon_says(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
    ]);

    assert_eq!(
        said,
        [format!(
            "benshi: {} cannot be used: line 2, column 8: unknown variant `sometimes`, \
             expected one of `each`, `close`, `interval`, `manual`",
            file.display()
        )]
    );
    assert_eq!(ended.and_then(|status| status.code()), FAILED);
    let socket = runtime.join("benshi.sock");
    assert!(!socket.exists(), "{} is there", socket.display());
    assert!(!data.exists(), "{} is there", data.display());
}
