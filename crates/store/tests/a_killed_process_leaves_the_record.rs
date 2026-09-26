//! A process killed outright after recording leaves the record.
//!
//! The half of the recording claim a unit test cannot make. The transaction
//! is committed by one process and read by another after the first was sent
//! `SIGKILL`, with no chance to flush or close anything: what the second one
//! reads is what the file holds, and it has to be exactly one row in each of
//! the two tables.
//!
//! `record_then_sleep` is a binary of this crate for this one purpose. It
//! records one episode, prints a line, and sleeps until it is killed.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use benshi_store::Store;

#[test]
fn a_process_killed_after_recording_leaves_exactly_one_row_in_each_table() {
    let home = tempfile::tempdir().expect("a directory of our own");
    let database = home.path().join("benshi.db");

    let mut child = Command::new(env!("CARGO_BIN_EXE_record_then_sleep"))
        .arg(&database)
        .stdout(Stdio::piped())
        .spawn()
        .expect("the helper starts");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut line = String::new();
    let heard = BufReader::new(stdout).read_line(&mut line);

    // `Child::kill` is SIGKILL on Unix: no handler runs, nothing is flushed.
    // Killed before anything is asserted, because a failed assertion would
    // otherwise leave the helper sleeping for ever; dropping a `Child` does
    // not kill it.
    child.kill().expect("the helper is killed");
    child.wait().expect("the helper is reaped");
    heard.expect("the helper says when it has recorded");
    assert_eq!(
        line.trim_end(),
        "recorded",
        "the helper recorded before it was killed"
    );

    // Opened through the store first, so that it is the store's own open that
    // recovers what the killed process left in the WAL.
    drop(Store::open(&database).expect("the file the killed process wrote opens"));
    let (episodes, operations, linked): (u32, u32, u32) = rusqlite::Connection::open(&database)
        .expect("the file opens for reading")
        .query_row(
            "SELECT (SELECT count(*) FROM episodes_seen), \
                    (SELECT count(*) FROM sync_queue), \
                    (SELECT count(*) FROM episodes_seen e JOIN sync_queue q ON q.show = e.show)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the tables are there");
    assert_eq!(episodes, 1);
    assert_eq!(operations, 1);
    assert_eq!(
        linked, 1,
        "the one operation belongs to the one episode's show"
    );
}
