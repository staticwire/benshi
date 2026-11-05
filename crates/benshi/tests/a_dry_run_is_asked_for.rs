//! What a dry run of delivery prints, asked of a daemon started in a time
//! zone the test chose.
//!
//! The binary itself is run, because the zone a viewing's day is named in is
//! the one the daemon reads where it starts.

#![cfg(target_os = "linux")]

mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};

use benshi_core::list::{Entry, EntryId, Status};
use benshi_core::path::RawPath;
use benshi_store::{Store, Viewing};
use common::{
    Daemon, Descriptors, Directory, SUCCEEDED, reading_nothing, run, sources_of, the_binary,
};

/// 22:30 UTC on November 1, 2026, which is the morning of November 2 in Tokyo.
const LATE: Duration = Duration::from_mins(29_892_870);

/// One entry of twelve episodes on the list, and its third episode seen at
/// [`LATE`], filed in the database at `database`.
fn seeded(database: &Path) {
    let mut store = Store::open(database).expect("the database opens");
    store
        .file(
            "anilist",
            &[Entry {
                id: EntryId(900_001),
                title: "Show Title".to_owned(),
                spellings: Vec::new(),
                format: None,
                episodes: Some(12),
                episode_length: None,
                year: None,
                released: true,
                status: Status::Watching,
                progress: 0,
                rewatches: 0,
                score: None,
            }],
        )
        .expect("the list is filed");
    let (show, _) = store.listed("anilist").expect("the list reads")[0];
    store
        .record(&Viewing {
            show,
            episode: Some(3),
            media: RawPath::from_bytes(b"/media/Show Title - 03.mkv".to_vec()),
            at: UNIX_EPOCH + LATE,
        })
        .expect("the viewing is recorded");
}

#[test]
fn a_viewing_is_on_the_day_the_time_zone_of_the_daemon_names() {
    let temporary = Directory::of("s-dry-run");
    let runtime = temporary.path().join("run");
    let data = temporary.path().join("data");
    seeded(&data.join("benshi.db"));
    let config = reading_nothing(temporary.path());

    let mut daemon = Daemon::start(&[
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
        ("BENSHI_DATA_DIR", data.as_os_str()),
        ("TZ", OsStr::new("Asia/Tokyo")),
    ]);
    let socket = runtime.join("benshi.sock");
    let answering = sources_of(&mut daemon, &socket);
    assert_eq!(answering.code, SUCCEEDED, "{answering:?}");

    let mut client = the_binary(
        &[
            OsStr::new("sync"),
            OsStr::new("now"),
            OsStr::new("--dry-run"),
            OsStr::new("--socket"),
            socket.as_os_str(),
        ],
        &[],
        Descriptors::Plenty,
    );
    client.stderr(Stdio::piped());
    let asked = run(client);

    assert_eq!(asked.code, SUCCEEDED, "{asked:?}");
    assert_eq!(
        asked.shown.lines().nth(2),
        Some("#1 2026-11-02 \"Show Title\" episode 3 -> AniList 900001 episode 3"),
        "{asked:?}"
    );
}
