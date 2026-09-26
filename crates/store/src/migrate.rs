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
pub const SCHEMA_VERSION: u32 = 1;

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
];

/// Apply every migration the database has not seen, in order.
///
/// A database ahead of the list is refused rather than written into.
pub(crate) fn apply(connection: &mut Connection, migrations: &[&str]) -> Result<(), Error> {
    let found: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(Error::Write)?;
    let known = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
    if found > known {
        return Err(Error::FromTheFuture { found, known });
    }

    let applied = usize::try_from(found).unwrap_or(usize::MAX);
    for (number, script) in (1..=known).zip(migrations).skip(applied) {
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
    fn a_title_is_filed_once() {
        // Two rows for one title would be two shows for one thing, and every
        // episode after the first would have to choose between them.
        let mut connection = Connection::open_in_memory().expect("memory opens");
        apply(&mut connection, &MIGRATIONS[..1]).expect("the first migration applies");
        connection
            .execute("INSERT INTO shows (title) VALUES ('Show Title')", [])
            .expect("the first row goes in");

        let again = connection.execute("INSERT INTO shows (title) VALUES ('Show Title')", []);

        assert!(
            again.is_err(),
            "the second row for the same title was refused"
        );
    }
}
