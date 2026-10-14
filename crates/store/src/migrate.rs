//! Numbered migrations, each in its own transaction.
//!
//! `PRAGMA user_version` holds how many have been applied. It is stored in the
//! database's own header, so `sqlite3` reads it without this crate, and it is
//! written inside each migration's transaction, so a migration that fails
//! leaves the number where it was and the next start tries the same one
//! again rather than skipping a half-applied one.

use rusqlite::Connection;

use crate::Error;

/// The schema this build brings a database to.
///
/// Written by hand rather than computed, so that adding a migration is a
/// change in two places that a test holds together.
pub const SCHEMA_VERSION: u32 = 2;

/// Every migration, in order, each a script of statements.
///
/// A script is applied whole inside one transaction. A table is created
/// without `IF NOT EXISTS` on purpose: the version says whether a script
/// ran, and a script that could run twice would hide a version written wrong.
pub(crate) const MIGRATIONS: &[&str] = &[
    // `shows` is the local entity every other table points at. `episode` is
    // NULL for a film, in `episodes_seen` and in `watching` alike, and no
    // uniqueness is declared over `(show, episode)`: a UNIQUE index counts
    // two NULLs as distinct, so it would hold the episodes and let a film in
    // twice. One row per thing in `watching` is kept by the code that writes
    // it. `registered` is a flag, 0 or 1.
    "CREATE TABLE shows (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL UNIQUE
    );
    CREATE TABLE episodes_seen (
        id INTEGER PRIMARY KEY,
        show INTEGER NOT NULL REFERENCES shows(id),
        episode INTEGER,
        seen_at TEXT NOT NULL,
        media TEXT NOT NULL
    );
    CREATE TABLE sync_queue (
        id INTEGER PRIMARY KEY,
        show INTEGER NOT NULL REFERENCES shows(id),
        kind TEXT NOT NULL,
        payload TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE TABLE watching (
        id INTEGER PRIMARY KEY,
        show INTEGER NOT NULL REFERENCES shows(id),
        episode INTEGER,
        media TEXT NOT NULL,
        watched_ms INTEGER NOT NULL,
        registered INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL
    );",
    // `shows` loses the uniqueness of its title, because two entries of one
    // list can share a title and each is a show of its own. `ALTER TABLE` in
    // SQLite cannot drop a constraint, so the table is built again: a new
    // one, the rows copied with their ids, the old one dropped, and the new
    // one renamed to `shows`, the name the other tables point at.
    "CREATE TABLE new_shows (
        id INTEGER PRIMARY KEY,
        title TEXT NOT NULL
    );
    INSERT INTO new_shows (id, title) SELECT id, title FROM shows;
    DROP TABLE shows;
    ALTER TABLE new_shows RENAME TO shows;",
];

/// Apply every migration the database has not seen, in order.
///
/// A database ahead of the list is refused rather than written into.
///
/// The foreign keys are off while the migrations run, and set back as they
/// were after them. A migration may drop a table that other tables point at,
/// and with the foreign keys on SQLite deletes its rows first, which the rows
/// pointing at them refuse. The switch does nothing inside a transaction, so
/// it is made around them.
pub(crate) fn apply(connection: &mut Connection, migrations: &[&str]) -> Result<(), Error> {
    let found: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let known = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
    if found > known {
        return Err(Error::FromTheFuture { found, known });
    }

    let enforced: bool = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    connection.pragma_update(None, "foreign_keys", false)?;
    let applied = apply_after(connection, migrations, found);
    connection.pragma_update(None, "foreign_keys", enforced)?;

    applied
}

/// Apply the migrations after the first `found`, each in a transaction of
/// its own.
fn apply_after(connection: &mut Connection, migrations: &[&str], found: u32) -> Result<(), Error> {
    let applied = usize::try_from(found).unwrap_or(usize::MAX);
    for (number, script) in (1..).zip(migrations).skip(applied) {
        let failed = |cause| Error::Migrate { number, cause };
        let transaction = connection.transaction().map_err(failed)?;
        // The version goes in before the script, so that the transaction is
        // the only thing keeping a failed migration's number out of the file.
        transaction
            .pragma_update(None, "user_version", number)
            .map_err(failed)?;
        transaction.execute_batch(script).map_err(failed)?;
        transaction.commit().map_err(failed)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use rusqlite::types::Value;

    use super::{MIGRATIONS, SCHEMA_VERSION, apply};
    use crate::Error;

    /// The schema version a connection is at.
    fn version(connection: &Connection) -> u32 {
        connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("the version is readable")
    }

    /// The columns of a table, in order, as the schema declares them.
    fn columns(connection: &Connection, table: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("the pragma prepares");
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("the pragma runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("every column has a name")
    }

    #[test]
    fn the_schema_version_is_the_number_of_migrations() {
        // `user_version` is the count of what was applied, so the two cannot
        // drift apart without this test saying so.
        assert_eq!(
            SCHEMA_VERSION,
            u32::try_from(MIGRATIONS.len()).expect("a small number")
        );
    }

    #[test]
    fn migrations_already_applied_are_not_applied_again() {
        // The scripts are plain `CREATE TABLE`, so applying one twice fails
        // on the second time. A second `apply` that succeeds has skipped them.
        let mut connection = Connection::open_in_memory().expect("memory opens");
        let scripts = [
            "CREATE TABLE one (id INTEGER)",
            "CREATE TABLE two (id INTEGER)",
        ];

        apply(&mut connection, &scripts).expect("the first pass applies both");
        apply(&mut connection, &scripts).expect("the second pass applies neither");

        assert_eq!(version(&connection), 2);
    }

    #[test]
    fn a_migration_that_fails_leaves_the_version_where_it_was() {
        // Each migration runs in its own transaction with the version written
        // inside it, so a failure rolls back the version as well as the
        // statements, and the next start tries the same migration again
        // rather than skipping over a half-applied one.
        let mut connection = Connection::open_in_memory().expect("memory opens");
        let scripts = [
            "CREATE TABLE one (id INTEGER)",
            "CREATE TABLE two (id INTEGER); THIS IS NOT SQL",
        ];

        let failed = apply(&mut connection, &scripts);

        assert!(
            matches!(failed, Err(Error::Migrate { number: 2, .. })),
            "got {failed:?}"
        );
        assert_eq!(version(&connection), 1);
        assert_eq!(
            columns(&connection, "two"),
            Vec::<String>::new(),
            "the failed migration's own table was rolled back with it"
        );
    }

    #[test]
    fn migration_one_creates_the_four_tables() {
        let mut connection = Connection::open_in_memory().expect("memory opens");

        apply(&mut connection, &MIGRATIONS[..1]).expect("the first migration applies");

        assert_eq!(columns(&connection, "shows"), ["id", "title"]);
        assert_eq!(
            columns(&connection, "episodes_seen"),
            ["id", "show", "episode", "seen_at", "media"]
        );
        assert_eq!(
            columns(&connection, "sync_queue"),
            ["id", "show", "kind", "payload", "created_at"]
        );
        assert_eq!(
            columns(&connection, "watching"),
            [
                "id",
                "show",
                "episode",
                "media",
                "watched_ms",
                "registered",
                "updated_at"
            ]
        );
    }

    #[test]
    fn two_shows_may_share_a_title() {
        // A title does not name an entry: AniList holds entries that share
        // one, and each is a show of its own.
        let mut connection = Connection::open_in_memory().expect("memory opens");
        apply(&mut connection, MIGRATIONS).expect("every migration applies");

        for _ in 0..2 {
            connection
                .execute("INSERT INTO shows (title) VALUES ('Show Title')", [])
                .expect("a show of that title goes in");
        }

        let shows: u32 = connection
            .query_row("SELECT count(*) FROM shows", [], |row| row.get(0))
            .expect("the table is there");
        assert_eq!(shows, 2);
    }

    /// Every row of every table, in the order of their ids.
    fn every_row(connection: &Connection) -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        for table in ["shows", "episodes_seen", "sync_queue", "watching"] {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
                .expect("the table is there");
            let width = statement.column_count();
            let read = statement
                .query_map([], |row| {
                    (0..width).map(|at| row.get::<_, Value>(at)).collect()
                })
                .expect("the rows are read")
                .collect::<Result<Vec<Vec<Value>>, _>>()
                .expect("every value is read");
            rows.extend(read);
        }
        rows
    }

    #[test]
    fn a_database_of_the_first_schema_keeps_every_row_through_the_second() {
        // `shows` is rebuilt, and every other table points at it. With the
        // foreign keys a store's connection enforces, dropping it is refused.
        let mut connection = Connection::open_in_memory().expect("memory opens");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("the foreign keys are enforced");
        apply(&mut connection, &MIGRATIONS[..1]).expect("the first migration applies");
        connection
            .execute_batch(
                "INSERT INTO shows (id, title) VALUES (4, 'Show Title'), (9, 'A Film');
                 INSERT INTO episodes_seen (show, episode, seen_at, media)
                     VALUES (4, 3, '2026-09-27T00:00:00Z', 'Show Title - 03.mkv'),
                            (9, NULL, '2026-09-28T00:00:00Z', 'A Film.mkv');
                 INSERT INTO sync_queue (show, kind, payload, created_at)
                     VALUES (4, 'progress', '{\"episode\":3}', '2026-09-27T00:00:00Z');
                 INSERT INTO watching (show, episode, media, watched_ms, registered, updated_at)
                     VALUES (9, NULL, 'A Film.mkv', 600000, 1, '2026-09-28T00:00:00Z');",
            )
            .expect("a row goes into every table");
        let before = every_row(&connection);

        apply(&mut connection, MIGRATIONS).expect("the second migration applies");

        assert_eq!(version(&connection), 2);
        assert_eq!(every_row(&connection), before);
        let enforced: bool = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("the pragma is readable");
        assert!(enforced, "the foreign keys are enforced again");
        let broken: u32 = connection
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .expect("the check runs");
        assert_eq!(broken, 0, "every row points at a show that is there");
    }

    #[test]
    fn foreign_keys_that_were_off_are_left_off() {
        // The SQLite this crate builds turns them on in every connection it
        // opens.
        let mut connection = Connection::open_in_memory().expect("memory opens");
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("the foreign keys are turned off");

        apply(&mut connection, MIGRATIONS).expect("every migration applies");

        let enforced: bool = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("the pragma is readable");
        assert!(!enforced);
    }
}
