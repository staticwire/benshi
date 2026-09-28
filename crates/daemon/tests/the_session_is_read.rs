//! What the daemon reads from the session it is started in.
//!
//! A process cannot set its own environment without `unsafe`, so each test
//! here runs this binary again in a session it chose, and hands that run what
//! it is to find.
//!
//! The tests are in a binary of their own because of what a child is while it
//! starts. Until it runs its own program it holds a copy of every descriptor
//! open in the process that started it. A listener that a test beside it has
//! dropped goes on answering for that long, and a test that expects a socket
//! with nobody behind it finds a daemon there.
//!
//! Every directory named here exists nowhere, because nothing under test looks
//! one up.

#![cfg(unix)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::Command;

use benshi_daemon::ipc::{fallback, socket_path};
use benshi_daemon::paths::{Role, directory};

/// Holds the socket's path a run is to find. Set in a run a test started, and
/// in no other.
const SOCKET: &str = "BENSHI_TEST_SOCKET";

/// Holds the warning a run is to find, and is not set where it is to find
/// none.
const WARNING: &str = "BENSHI_TEST_WARNING";

/// Holds the data directory a run is to find, as `SOCKET` holds the path.
const DATA: &str = "BENSHI_TEST_DATA";

/// Holds the configuration directory a run is to find.
const CONFIG: &str = "BENSHI_TEST_CONFIG";

/// A directory whose name is not Unicode.
fn not_text(beneath: &str) -> OsString {
    let mut directory = OsString::from(beneath);
    directory.push(OsStr::from_bytes(b"/\xff"));

    directory
}

/// Run one test of this binary again, in a session that holds these variables
/// and no others.
fn again(name: &str, session: &[(&str, OsString)]) {
    let run = Command::new(env::current_exe().expect("the tests know what runs them"))
        .args(["--exact", name])
        .env_clear()
        .envs(session.iter().map(|(variable, held)| (variable, held)))
        .output()
        .expect("the tests can be run again");
    let said = String::from_utf8_lossy(&run.stdout);

    assert!(run.status.success(), "{session:?}: {said}");
    // A name that matches no test is a run that passes.
    assert!(said.contains("1 passed"), "{session:?}: {said}");
}

#[test]
fn the_socket_is_where_the_session_puts_it() {
    if let Some(expected) = env::var_os(SOCKET) {
        assert_eq!(socket_path(), PathBuf::from(expected));
        assert_eq!(
            fallback().map(|fallback| OsString::from(fallback.to_string())),
            env::var_os(WARNING)
        );
        return;
    }

    let unreadable = not_text("/nowhere/run");
    let not_set = "`XDG_RUNTIME_DIR` is not set";
    let empty = "`XDG_RUNTIME_DIR` is empty";
    let relative = "`XDG_RUNTIME_DIR` holds `run/1000`, which is a relative path";
    let mut sessions = vec![
        (
            Some("/nowhere/run/1000".into()),
            Some("/nowhere/tmp".into()),
            PathBuf::from("/nowhere/run/1000/benshi.sock"),
            None,
        ),
        (
            Some("/nowhere/run/1001".into()),
            Some(OsString::new()),
            PathBuf::from("/nowhere/run/1001/benshi.sock"),
            None,
        ),
        (
            Some(unreadable.clone()),
            Some("/nowhere/tmp".into()),
            PathBuf::from(unreadable).join("benshi.sock"),
            None,
        ),
        (
            None,
            Some("/nowhere/tmp".into()),
            PathBuf::from("/nowhere/tmp/benshi/benshi.sock"),
            Some(not_set),
        ),
        (
            Some(OsString::new()),
            Some("/nowhere/tmp".into()),
            PathBuf::from("/nowhere/tmp/benshi/benshi.sock"),
            Some(empty),
        ),
        (
            Some("run/1000".into()),
            Some("/nowhere/tmp".into()),
            PathBuf::from("/nowhere/tmp/benshi/benshi.sock"),
            Some(relative),
        ),
        (
            None,
            Some(OsString::new()),
            PathBuf::from("/tmp/benshi/benshi.sock"),
            Some(not_set),
        ),
        (
            Some(OsString::new()),
            Some("tmp/here".into()),
            PathBuf::from("/tmp/benshi/benshi.sock"),
            Some(empty),
        ),
    ];
    // Where no temporary directory is set the system names one, and which one
    // is the system's own business.
    if cfg!(target_os = "linux") {
        sessions.push((
            None,
            None,
            PathBuf::from("/tmp/benshi/benshi.sock"),
            Some(not_set),
        ));
    }

    for (runtime, temporary, expected, why) in sessions {
        let warning = why.map(|why| format!("{why}, so the socket is {}", expected.display()));
        let mut session = vec![(SOCKET, expected.into_os_string())];
        session.extend(warning.map(|warning| (WARNING, warning.into())));
        session.extend(runtime.map(|runtime| ("XDG_RUNTIME_DIR", runtime)));
        session.extend(temporary.map(|temporary| ("TMPDIR", temporary)));

        again("the_socket_is_where_the_session_puts_it", &session);
    }
}

#[test]
fn the_directories_are_where_the_session_puts_them() {
    if let (Some(data), Some(config)) = (env::var_os(DATA), env::var_os(CONFIG)) {
        assert_eq!(directory(Role::Data), Ok(PathBuf::from(data)));
        assert_eq!(directory(Role::Config), Ok(PathBuf::from(config)));
        return;
    }

    let (data, config) = if cfg!(target_os = "macos") {
        let both = "/nowhere/home/viewer/Library/Application Support/benshi";
        (both, both)
    } else {
        ("/nowhere/xdg/data/benshi", "/nowhere/xdg/config/benshi")
    };
    let unreadable = not_text("/nowhere/disk");
    let overrides = [
        (None, OsString::from(data)),
        (Some(unreadable.clone()), unreadable),
        (
            Some("~/.benshi".into()),
            "/nowhere/home/viewer/.benshi".into(),
        ),
    ];

    for (given, data) in overrides {
        let mut session = vec![
            (DATA, data),
            (CONFIG, config.into()),
            ("XDG_DATA_HOME", "/nowhere/xdg/data".into()),
            ("XDG_CONFIG_HOME", "/nowhere/xdg/config".into()),
            ("HOME", "/nowhere/home/viewer".into()),
        ];
        session.extend(given.map(|given| ("BENSHI_DATA_DIR", given)));

        again("the_directories_are_where_the_session_puts_them", &session);
    }
}
