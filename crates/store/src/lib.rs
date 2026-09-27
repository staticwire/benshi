//! The canonical local store.
//!
//! One SQLite database holds what was watched, what is being watched, and the
//! operations still to be delivered to a list. Migrations are numbered and each
//! has a test.
//!
//! Recording an episode does not depend on a list being reachable. A network
//! problem, a dead API or an expired token is a delivery problem, so marking an
//! episode locally and enqueueing its sync operation belong in **one
//! transaction across two tables**, and the queue holds idempotent operations
//! rather than state snapshots.
//!
//! **The store never decides where it lives.** [`Store::open`] takes a finished
//! path and never looks one up. A missing directory is created and a missing
//! file is an empty database migrated from nothing, which is what makes the
//! same crate right on a desktop, where the file is permanent, and on a
//! television, where the system may delete it between runs. Which directory
//! that is belongs to the layer that knows the platform.

mod migrate;
pub mod queue;
mod record;
pub mod watching;

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use migrate::SCHEMA_VERSION;
pub use record::Viewing;

/// Why the store could not be opened or written.
///
/// Every one of these is permanent rather than transient: a database that
/// cannot be opened is reported before anything starts, and a statement that
/// fails stops the task that ran it rather than letting the daemon go on
/// detecting while it records nothing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The directory for the database could not be made.
    #[error("{path} could not be created: {cause}")]
    Directory {
        /// The directory that was asked for.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        cause: std::io::Error,
    },
    /// The database file could not be opened or set up.
    #[error("{path} could not be opened: {cause}")]
    Open {
        /// The file that was asked for.
        path: PathBuf,
        /// What SQLite said.
        #[source]
        cause: rusqlite::Error,
    },
    /// A migration did not apply, and the schema is where it was before it.
    #[error("migration {number} did not apply: {cause}")]
    Migrate {
        /// Which migration, counting from one.
        number: u32,
        /// What SQLite said.
        #[source]
        cause: rusqlite::Error,
    },
    /// The database was written by a build that knows more migrations than
    /// this one.
    ///
    /// Refused rather than opened, because this build cannot see the tables
    /// the newer one made and would write into a schema it does not
    /// understand.
    #[error("the database is at schema {found} and this build knows only up to {known}")]
    FromTheFuture {
        /// The schema the file is at.
        found: u32,
        /// The schema this build brings a file to.
        known: u32,
    },
    /// A statement failed on a database that is already open. The schema
    /// version read that starts the migrations is one of them.
    #[error("the database could not be read or written: {0}")]
    Write(#[source] rusqlite::Error),
    /// A row in the queue that cannot be read as an operation.
    #[error("operation {id} in the queue cannot be read: {cause}")]
    Operation {
        /// The row's `id` in `sync_queue`.
        id: i64,
        /// What the kind or the payload failed on.
        #[source]
        cause: serde_json::Error,
    },
    /// A row in the queue whose show is not in `shows`.
    ///
    /// Foreign keys keep one from being written through this crate.
    /// `sqlite3` leaves them off unless it is asked, so a row written by
    /// hand or a show deleted by hand leaves one.
    #[error("operation {id} in the queue is for a show that is not there")]
    Orphan {
        /// The row's `id` in `sync_queue`.
        id: i64,
    },
    /// A row of `watching` whose media is not a path as this crate writes
    /// one: a per-cent sign with no two hexadecimal digits after it.
    #[error("the media of row {id} in `watching` is not an escaped path: {text:?}")]
    Media {
        /// The row's `id` in `watching`.
        id: i64,
        /// The text the column holds.
        text: String,
    },
    /// An instant the columns cannot hold: before 1970, or in the year 10000
    /// or later, neither of which RFC 3339 text here can spell.
    ///
    /// A clock reading either is broken, and the answer is to say so rather
    /// than to write a date that is not the one it read.
    #[error("the clock reads {at:?}, which is before 1970 or past the year 9999")]
    Instant {
        /// What the clock read.
        at: std::time::SystemTime,
    },
}

/// One open database, at the current schema.
#[derive(Debug)]
pub struct Store {
    connection: Connection,
}

impl Store {
    /// Open the database at this path, creating what is missing and bringing
    /// it to the current schema.
    ///
    /// The path is taken as given: which directory it is in belongs to the
    /// caller, who knows the platform. The directory is created if it does
    /// not exist, on Unix with a mode that keeps other users out, because the
    /// file holds what somebody watched. A file that does not exist is an
    /// empty database and every migration is applied to it; a file that does
    /// is brought up to date.
    ///
    /// The connection is put into WAL mode with `synchronous` at `NORMAL`.
    /// The store is built for a write a second while anything plays, and the
    /// default rollback journal at `synchronous = FULL` syncs the journal and
    /// then the database file on every commit. WAL at `NORMAL` syncs neither
    /// on a commit, only around a checkpoint. A power loss or a kernel crash
    /// can then lose the last transactions committed; the database stays
    /// consistent, and this process crashing on its own loses nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Directory`] and [`Error::Open`] for what the filesystem and
    /// SQLite refuse, [`Error::Migrate`] for a migration that did not apply,
    /// and [`Error::FromTheFuture`] for a file a newer build wrote.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(directory) = path.parent() {
            private_directory(directory).map_err(|cause| Error::Directory {
                path: directory.to_path_buf(),
                cause,
            })?;
        }
        let did_not_open = |cause| Error::Open {
            path: path.to_path_buf(),
            cause,
        };
        let mut connection = Connection::open(path).map_err(did_not_open)?;
        prepare(&connection).map_err(did_not_open)?;
        migrate::apply(&mut connection, migrate::MIGRATIONS)?;

        Ok(Self { connection })
    }

    /// The schema the file is at, read from the file rather than assumed.
    ///
    /// Equal to [`SCHEMA_VERSION`] after [`Store::open`], and worth printing
    /// when the daemon starts: a database a newer build wrote is refused with
    /// both numbers, and this is the one a person compares against.
    ///
    /// # Panics
    ///
    /// If the version cannot be read. It is in the header of every SQLite
    /// file, so a failure here is the connection failing rather than a value
    /// that is not there.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version is in the header of every SQLite file")
    }
}

/// File a show under its title where it is not filed, and answer its row.
pub(crate) fn filed(connection: &Connection, title: &str) -> Result<i64, Error> {
    connection
        .execute("INSERT OR IGNORE INTO shows (title) VALUES (?1)", [title])
        .map_err(Error::Write)?;
    connection
        .query_row("SELECT id FROM shows WHERE title = ?1", [title], |row| {
            row.get(0)
        })
        .map_err(Error::Write)
}

/// Make sure the database's directory exists, private where this call is what
/// creates it.
///
/// The builder is recursive, so a missing parent is created as well, with the
/// same mode. A directory that is already there is left as it is, mode
/// included.
fn private_directory(directory: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)
}

/// Set the connection up the way every connection here is set up.
///
/// `busy_timeout` is one second, and it is set first so that the switch to
/// WAL below waits under it as well. WAL takes one writer at a time, so a
/// write here waits while another process holds the write lock: a second
/// daemon, or `sqlite3` in the user's hands in a write transaction or on a
/// checkpoint that is not passive. Without the timeout such a write fails at
/// once with `SQLITE_BUSY`, and with it a lock nobody releases is still
/// reported after a second rather than waited on forever.
///
/// `PRAGMA journal_mode` answers with the mode in force, which is not always
/// the one asked for, and `pragma_update` discards that answer. A file that
/// cannot take WAL is therefore not an error here, and the mode is asserted
/// in a test instead.
fn prepare(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "busy_timeout", 1000)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{Error, Store};
    use crate::migrate::SCHEMA_VERSION;

    /// A directory of our own, and a path two levels under it that does not
    /// exist yet, so that opening has to create the directory as well as the
    /// file.
    fn a_home() -> (TempDir, PathBuf) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let database = home.path().join("data").join("benshi.db");
        (home, database)
    }

    /// A pragma's value, read straight off the connection.
    fn pragma<T: rusqlite::types::FromSql>(store: &Store, name: &str) -> T {
        store
            .connection
            .pragma_query_value(None, name, |row| row.get(0))
            .expect("the pragma is readable")
    }

    #[test]
    fn opening_a_path_that_does_not_exist_creates_the_database_at_the_current_schema() {
        let (_home, database) = a_home();

        let store = Store::open(&database).expect("a fresh database opens");

        assert!(database.is_file(), "the file was created");
        assert_eq!(store.schema_version(), SCHEMA_VERSION);
        // Through a second connection to the file, the way `sqlite3` in the
        // user's hands would read it, and not through the store's own.
        let from_the_file: u32 = rusqlite::Connection::open(&database)
            .expect("the file opens a second time")
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the version is in the header");
        assert_eq!(from_the_file, SCHEMA_VERSION);
    }

    #[test]
    fn opening_the_same_database_again_keeps_what_it_holds() {
        let (_home, database) = a_home();
        let first = Store::open(&database).expect("a fresh database opens");
        first
            .connection
            .execute("INSERT INTO shows (title) VALUES ('Show Title')", [])
            .expect("a row goes in");
        drop(first);

        let again = Store::open(&database).expect("an existing database opens");

        let titles: u32 = again
            .connection
            .query_row("SELECT count(*) FROM shows", [], |row| row.get(0))
            .expect("the table is still there");
        assert_eq!(titles, 1, "the row survived a second open");
        assert_eq!(again.schema_version(), SCHEMA_VERSION);
    }

    #[test]
    fn the_schema_version_is_read_from_the_file_rather_than_assumed() {
        // The number a person compares against when a newer build's file is
        // refused, so it has to be the file's and not this build's.
        let (_home, database) = a_home();
        let store = Store::open(&database).expect("a fresh database opens");
        store
            .connection
            .pragma_update(None, "user_version", 7)
            .expect("the version is writable");

        assert_eq!(store.schema_version(), 7);
    }

    #[test]
    fn a_database_from_the_future_is_refused_rather_than_written_into() {
        // An older binary opening a newer database does not know the schema
        // in front of it. Writing into it anyway would be writing into tables
        // it cannot see, so the answer is a refusal that names both numbers.
        let (_home, database) = a_home();
        drop(Store::open(&database).expect("a fresh database opens"));
        let from_the_future = SCHEMA_VERSION + 1;
        rusqlite::Connection::open(&database)
            .expect("the file opens")
            .pragma_update(None, "user_version", from_the_future)
            .expect("the version is writable");

        let refused = Store::open(&database);

        assert!(
            matches!(
                refused,
                Err(Error::FromTheFuture { found, known })
                    if found == from_the_future && known == SCHEMA_VERSION
            ),
            "got {refused:?}"
        );
    }

    #[test]
    fn the_database_is_in_wal_mode_and_syncs_only_at_checkpoints() {
        // The store is built for a write a second while anything plays. The
        // default rollback journal at FULL syncs the journal and then the
        // database file on every one of them; WAL at NORMAL syncs neither on
        // a commit, only around a checkpoint.
        let (_home, database) = a_home();

        let store = Store::open(&database).expect("a fresh database opens");

        assert_eq!(pragma::<String>(&store, "journal_mode"), "wal");
        assert_eq!(pragma::<u32>(&store, "synchronous"), 1, "NORMAL is 1");
    }

    #[test]
    fn foreign_keys_are_enforced() {
        // Off by default in SQLite, per connection, and a row pointing at a
        // show that is not there is exactly what the tables must refuse.
        let (_home, database) = a_home();
        let store = Store::open(&database).expect("a fresh database opens");

        let orphan = store.connection.execute(
            "INSERT INTO episodes_seen (show, episode, seen_at, media) \
             VALUES (42, 3, '2026-09-27T00:00:00Z', 'ep 03.mkv')",
            [],
        );

        assert!(
            orphan.is_err(),
            "a row for a show that is not there went in"
        );
    }

    #[test]
    fn the_busy_timeout_is_one_second() {
        // WAL takes one writer at a time and a reader blocks nobody, so what
        // a write here waits on is another writer: a second daemon, or
        // `sqlite3` in the user's hands. Without the timeout it fails at once.
        let (_home, database) = a_home();

        let store = Store::open(&database).expect("a fresh database opens");

        assert_eq!(pragma::<u32>(&store, "busy_timeout"), 1000);
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_the_store_creates_is_reachable_by_nobody_else() {
        use std::os::unix::fs::PermissionsExt;

        let (_home, database) = a_home();

        drop(Store::open(&database).expect("a fresh database opens"));

        let directory = database.parent().expect("the file has a directory");
        let mode = std::fs::metadata(directory)
            .expect("the directory exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the file holds what somebody watched");
    }
}
