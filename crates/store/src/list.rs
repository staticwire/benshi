//! What each list held when it was last read.
//!
//! A list is filed whole: every entry of one backend's list, in one
//! transaction. An entry the backend has not linked to a show yet is filed as
//! a show of its own and linked to it in `backend_ids`. **A link outlives the
//! entry leaving the list**, so an entry that comes back is the show it was,
//! with what was watched of it. What the entries hold, in `list_entries`, is
//! replaced whole at each filing.
//!
//! The title and the spellings of an entry that left the list are kept in
//! `names` as names of its show. A file named after it is that show where
//! recognition refuses the name of the file.

use std::collections::HashSet;
use std::iter;
use std::time::Duration;

use benshi_core::ShowId;
use benshi_core::list::{Entry, EntryId, Rating};
use benshi_core::recognize::normalize::Key;
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{Error, Store};

impl Store {
    /// File every entry of one backend's list, in place of what it held.
    ///
    /// An entry with no show is filed as a show titled as the entry is, and
    /// linked to it. A show that is linked takes the entry's title. An entry
    /// of the list before that is not among these keeps its show and its
    /// link, and its names are kept as names of its show: the title of the
    /// show, and the spellings the entry was filed with where they are still
    /// there and read. A key kept already keeps the show it has.
    ///
    /// The length of an episode is kept in whole milliseconds, and one longer
    /// than the column holds is kept as the longest it holds.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when a statement fails for any other reason, an entry
    /// that is in the list twice among them. Nothing is filed with either.
    pub fn file(&mut self, backend: &str, entries: &[Entry]) -> Result<(), Error> {
        let transaction = self.connection.transaction()?;
        keep_names_of_departed(&transaction, backend, entries)?;
        transaction.execute("DELETE FROM list_entries WHERE backend = ?1", [backend])?;
        for entry in entries {
            link(&transaction, backend, entry)?;
            transaction.execute(
                "INSERT INTO list_entries (backend, entry, spellings, format, episodes, \
                 episode_ms, year, released, status, progress, rewatches, score) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    backend,
                    entry.id.0,
                    serde_json::to_string(&entry.spellings).map_err(unwritable)?,
                    entry.format.map(name).transpose()?,
                    entry.episodes,
                    entry
                        .episode_length
                        .map(|length| i64::try_from(length.as_millis()).unwrap_or(i64::MAX)),
                    entry.year,
                    entry.released,
                    name(entry.status)?,
                    entry.progress,
                    entry.rewatches,
                    entry.score.map(Rating::points),
                ],
            )?;
        }
        transaction.commit()?;

        Ok(())
    }

    /// Every show of a backend's list and what its entry holds, lowest show
    /// first.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when the database cannot be read because another
    /// connection has it, and [`Error::Write`] when it cannot be read for any
    /// other reason, a count, a length or a year out of the range of its
    /// field included. [`Error::Showless`] for a row whose show is not there
    /// and [`Error::Entry`] for a row that cannot be read as an entry, a
    /// score out of range among them, each naming the entry: the reading
    /// fails whole rather than answering without it.
    pub fn listed(&self, backend: &str) -> Result<Vec<(ShowId, Entry)>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT backend_ids.show, shows.title, entry, spellings, format, episodes, \
             episode_ms, year, released, status, progress, rewatches, score \
             FROM list_entries \
             LEFT JOIN backend_ids USING (backend, entry) \
             LEFT JOIN shows ON shows.id = backend_ids.show \
             WHERE backend = ?1 \
             ORDER BY backend_ids.show",
        )?;

        statement
            .query_and_then([backend], |row| read(backend, row))?
            .collect()
    }
}

/// Keep the names of every entry linked on the backend that `entries` does
/// not hold as names of its show, as [`Store::file`] says.
fn keep_names_of_departed(
    transaction: &Transaction,
    backend: &str,
    entries: &[Entry],
) -> Result<(), Error> {
    let listed: HashSet<i64> = entries.iter().map(|entry| entry.id.0).collect();
    let linked: Vec<(i64, i64, String, Option<String>)> = transaction
        .prepare(
            "SELECT backend_ids.entry, backend_ids.show, shows.title, list_entries.spellings \
             FROM backend_ids \
             JOIN shows ON shows.id = backend_ids.show \
             LEFT JOIN list_entries USING (backend, entry) \
             WHERE backend_ids.backend = ?1",
        )?
        .query_map([backend], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;

    for (entry, show, title, spellings) in linked {
        if listed.contains(&entry) {
            continue;
        }
        // Spellings that do not read are passed over. This filing replaces
        // the row, and a refusal here would refuse every filing after it.
        let spellings: Vec<String> = spellings
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        for spelled in iter::once(title).chain(spellings) {
            let Some(key) = Key::from_title(&spelled) else {
                continue;
            };
            transaction.execute(
                "INSERT INTO names (show, letters, marks, season, part) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
                params![show, key.letters(), key.marks(), key.season(), key.part()],
            )?;
        }
    }

    Ok(())
}

/// Link an entry to its show, filing the show where the backend has none,
/// and give the show the entry's title.
fn link(transaction: &Transaction, backend: &str, entry: &Entry) -> Result<(), Error> {
    let linked: Option<i64> = transaction
        .query_row(
            "SELECT show FROM backend_ids WHERE backend = ?1 AND entry = ?2",
            params![backend, entry.id.0],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(show) = linked {
        transaction.execute(
            "UPDATE shows SET title = ?2 WHERE id = ?1",
            params![show, entry.title],
        )?;
    } else {
        let show: i64 = transaction.query_row(
            "INSERT INTO shows (title) VALUES (?1) RETURNING id",
            [&entry.title],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO backend_ids (show, backend, entry) VALUES (?1, ?2, ?3)",
            params![show, backend, entry.id.0],
        )?;
    }

    Ok(())
}

/// The show and the entry one row of [`Store::listed`] holds.
fn read(backend: &str, row: &Row) -> Result<(ShowId, Entry), Error> {
    let id = EntryId(row.get(2)?);
    let show: Option<i64> = row.get(0)?;
    let title: Option<String> = row.get(1)?;
    let (Some(show), Some(title)) = (show, title) else {
        return Err(Error::Showless {
            backend: backend.to_owned(),
            entry: id,
        });
    };
    let unread = |cause| Error::Entry {
        backend: backend.to_owned(),
        entry: id,
        cause,
    };
    let episode_ms: Option<i64> = row.get(6)?;
    let episode_length = episode_ms
        .map(|ms| {
            u64::try_from(ms)
                .map(Duration::from_millis)
                .map_err(|_| Error::Write(rusqlite::Error::IntegralValueOutOfRange(6, ms)))
        })
        .transpose()?;
    let format: Option<String> = row.get(4)?;
    let score: Option<i64> = row.get(12)?;

    Ok((
        ShowId(show),
        Entry {
            id,
            title,
            spellings: serde_json::from_str(&row.get::<_, String>(3)?).map_err(unread)?,
            format: format.map(named).transpose().map_err(unread)?,
            episodes: row.get(5)?,
            episode_length,
            year: row.get(7)?,
            released: row.get(8)?,
            status: named(row.get(9)?).map_err(unread)?,
            progress: row.get(10)?,
            rewatches: row.get(11)?,
            score: serde_json::from_value(Value::from(score)).map_err(unread)?,
        },
    ))
}

/// The name a variant is kept under: the text serde writes it as.
fn name(variant: impl Serialize) -> Result<String, Error> {
    serde_json::to_value(variant)
        .and_then(serde_json::from_value)
        .map_err(unwritable)
}

/// The variant a name is kept for.
fn named<T: DeserializeOwned>(name: String) -> serde_json::Result<T> {
    serde_json::from_value(Value::String(name))
}

/// A value that could not be made into a column.
fn unwritable(cause: serde_json::Error) -> Error {
    Error::Write(rusqlite::Error::ToSqlConversionFailure(cause.into()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use benshi_core::ShowId;
    use benshi_core::list::{Entry, EntryId, Format, Rating, Status};
    use benshi_core::recognize::normalize::Key;
    use rusqlite::types::Value;
    use tempfile::TempDir;

    use crate::{Error, Store};

    const BACKEND: &str = "anilist";

    fn a_store() -> (TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        (home, store)
    }

    /// A series of twelve episodes, three of them seen.
    fn a_series(id: i64, title: &str) -> Entry {
        Entry {
            id: EntryId(id),
            title: title.to_owned(),
            spellings: vec![format!("{title} Spelled Otherwise"), format!("{title} 2")],
            format: Some(Format::Tv),
            episodes: Some(12),
            episode_length: Some(Duration::from_mins(24)),
            year: Some(2024),
            released: true,
            status: Status::Watching,
            progress: 3,
            rewatches: 0,
            score: None,
        }
    }

    /// A film seen twice, with a score.
    fn a_film(id: i64) -> Entry {
        Entry {
            id: EntryId(id),
            title: "A Film".to_owned(),
            spellings: Vec::new(),
            format: Some(Format::Film),
            episodes: Some(1),
            episode_length: Some(Duration::from_mins(95)),
            year: Some(1988),
            released: true,
            status: Status::Completed,
            progress: 1,
            rewatches: 1,
            score: Some(Rating::new(80).expect("a rating from 1 to 100")),
        }
    }

    /// A work that has not been released, of which the service says little.
    fn a_work_to_come(id: i64) -> Entry {
        Entry {
            id: EntryId(id),
            title: "A Work To Come".to_owned(),
            spellings: Vec::new(),
            format: None,
            episodes: None,
            episode_length: None,
            year: None,
            released: false,
            status: Status::Planned,
            progress: 0,
            rewatches: 0,
            score: None,
        }
    }

    /// Every row of a table, in the order of its key.
    fn rows(store: &Store, table: &str) -> Vec<Vec<Value>> {
        let order = match table {
            "shows" => "id",
            "names" => "rowid",
            _ => "backend, entry",
        };
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
            .expect("the table is there");
        let width = statement.column_count();
        statement
            .query_map([], |row| (0..width).map(|at| row.get(at)).collect())
            .expect("the rows are read")
            .collect::<Result<_, _>>()
            .expect("every value is read")
    }

    /// Every row of the three tables a list is filed in.
    fn every_row(store: &Store) -> [Vec<Vec<Value>>; 3] {
        ["shows", "backend_ids", "list_entries"].map(|table| rows(store, table))
    }

    fn count(store: &Store, table: &str) -> usize {
        rows(store, table).len()
    }

    #[test]
    fn a_list_filed_is_listed_as_it_was_filed() {
        let (_home, mut store) = a_store();
        let list = [
            a_series(21, "Show Title"),
            a_film(112_151),
            a_work_to_come(9),
        ];

        store.file(BACKEND, &list).expect("the list is filed");

        assert_eq!(
            store.listed(BACKEND).expect("the list is read"),
            [
                (ShowId(1), list[0].clone()),
                (ShowId(2), list[1].clone()),
                (ShowId(3), list[2].clone()),
            ]
        );
    }

    #[test]
    fn two_entries_of_one_title_are_two_shows() {
        // A series and its special can carry one romaji title.
        let (_home, mut store) = a_store();

        let special = Entry {
            format: Some(Format::Special),
            ..a_series(2319, "Show Title")
        };
        store
            .file(BACKEND, &[a_series(443, "Show Title"), special])
            .expect("the list is filed");

        let shows: Vec<ShowId> = store
            .listed(BACKEND)
            .expect("the list is read")
            .into_iter()
            .map(|(show, _)| show)
            .collect();
        assert_eq!(shows, [ShowId(1), ShowId(2)]);
    }

    #[test]
    fn an_entry_that_left_the_list_keeps_its_show_and_its_link() {
        // What was watched of it stays with a show, and the show stays the
        // entry's.
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title"), a_film(112_151)])
            .expect("the list is filed");

        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("the list is filed again");

        assert_eq!(
            store.listed(BACKEND).expect("the list is read"),
            [(ShowId(1), a_series(21, "Show Title"))]
        );
        assert_eq!(count(&store, "shows"), 2);
        assert_eq!(
            rows(&store, "backend_ids"),
            [
                [
                    Value::Integer(1),
                    Value::Text(BACKEND.to_owned()),
                    Value::Integer(21)
                ],
                [
                    Value::Integer(2),
                    Value::Text(BACKEND.to_owned()),
                    Value::Integer(112_151)
                ],
            ]
        );
        assert_eq!(count(&store, "list_entries"), 1);
    }

    #[test]
    fn an_entry_that_comes_back_to_the_list_is_the_show_it_was() {
        let (_home, mut store) = a_store();
        let list = [a_series(21, "Show Title"), a_film(112_151)];
        store.file(BACKEND, &list).expect("the list is filed");
        let first = store.listed(BACKEND).expect("the list is read");
        store
            .file(BACKEND, &list[..1])
            .expect("the list is filed without the film");

        store
            .file(BACKEND, &list)
            .expect("the list is filed with the film again");

        assert_eq!(store.listed(BACKEND).expect("the list is read"), first);
        assert_eq!(count(&store, "shows"), 2);
    }

    /// The show a name is kept as, by the key of its spelling.
    fn kept_as(store: &mut Store, spelled: &str) -> ShowId {
        let key = Key::from_title(spelled).expect("a title with letters in it has a key");
        store.unlisted(&key, spelled).expect("the name is kept").0
    }

    #[test]
    fn a_name_of_an_entry_that_left_the_list_is_the_show_it_was() {
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(900_001, "Departed Show")])
            .expect("the list is filed");
        let show = store.listed(BACKEND).expect("the list is read")[0].0;

        store
            .file(BACKEND, &[])
            .expect("the list is filed without it");

        for spelled in [
            "Departed Show",
            "Departed Show Spelled Otherwise",
            "Departed Show 2",
        ] {
            assert_eq!(kept_as(&mut store, spelled), show, "{spelled}");
        }
        assert_eq!(count(&store, "shows"), 1);
    }

    #[test]
    fn a_name_of_an_entry_that_left_keeps_its_season_and_its_part() {
        let (_home, mut store) = a_store();
        store
            .file(
                BACKEND,
                &[a_series(900_001, "Departed Show Season 2 Part 2")],
            )
            .expect("the list is filed");
        let show = store.listed(BACKEND).expect("the list is read")[0].0;

        store
            .file(BACKEND, &[])
            .expect("the list is filed without it");

        assert_eq!(kept_as(&mut store, "Departed Show Season 2 Part 2"), show);
    }

    #[test]
    fn a_name_kept_before_its_entry_left_keeps_the_show_it_had() {
        let (_home, mut store) = a_store();
        let kept = kept_as(&mut store, "Departed Show");
        store
            .file(BACKEND, &[a_series(900_001, "Departed Show")])
            .expect("the list is filed");

        store
            .file(BACKEND, &[])
            .expect("the list is filed without it");

        assert_eq!(kept_as(&mut store, "Departed Show"), kept);
    }

    #[test]
    fn an_entry_whose_spellings_are_gone_is_found_by_the_title_of_its_show() {
        // An entry that left at an earlier filing has no row in
        // `list_entries`, so the title of its show is all there is to keep.
        let (home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(900_001, "Departed Show")])
            .expect("the list is filed");
        let show = store.listed(BACKEND).expect("the list is read")[0].0;
        store
            .file(BACKEND, &[])
            .expect("the list is filed without it");
        rusqlite::Connection::open(home.path().join("benshi.db"))
            .expect("the file opens")
            .execute("DELETE FROM names", [])
            .expect("the names are taken out by hand");

        store
            .file(BACKEND, &[])
            .expect("the list is filed without it again");

        assert_eq!(kept_as(&mut store, "Departed Show"), show);
        assert_ne!(kept_as(&mut store, "Departed Show Spelled Otherwise"), show);
    }

    #[test]
    fn an_entry_leaving_whose_spellings_do_not_read_keeps_the_title_of_its_show() {
        let (home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(900_001, "Departed Show")])
            .expect("the list is filed");
        let show = store.listed(BACKEND).expect("the list is read")[0].0;
        rusqlite::Connection::open(home.path().join("benshi.db"))
            .expect("the file opens")
            .execute("UPDATE list_entries SET spellings = 'not a list'", [])
            .expect("the row is changed by hand");

        store
            .file(BACKEND, &[])
            .expect("the list is filed without it");

        assert_eq!(count(&store, "list_entries"), 0);
        assert_eq!(kept_as(&mut store, "Departed Show"), show);
    }

    #[test]
    fn filing_the_same_list_twice_changes_nothing() {
        let (_home, mut store) = a_store();
        let list = [
            a_series(21, "Show Title"),
            a_film(112_151),
            a_work_to_come(9),
        ];
        store.file(BACKEND, &list).expect("the list is filed");
        let once = every_row(&store);

        store.file(BACKEND, &list).expect("the list is filed again");

        assert_eq!(every_row(&store), once);
    }

    #[test]
    fn what_an_entry_holds_is_replaced_by_the_next_filing() {
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("the list is filed");
        let watched_on = Entry {
            progress: 4,
            score: Rating::new(70),
            ..a_series(21, "Show Title")
        };

        store
            .file(BACKEND, std::slice::from_ref(&watched_on))
            .expect("the list is filed again");

        assert_eq!(
            store.listed(BACKEND).expect("the list is read"),
            [(ShowId(1), watched_on)]
        );
    }

    #[test]
    fn a_title_the_service_changed_is_the_title_of_the_show_from_then_on() {
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("the list is filed");
        let renamed = Entry {
            title: "Show Title Renamed".to_owned(),
            ..a_series(21, "Show Title")
        };

        store
            .file(BACKEND, std::slice::from_ref(&renamed))
            .expect("the list is filed again");

        assert_eq!(
            rows(&store, "shows"),
            [[
                Value::Integer(1),
                Value::Text("Show Title Renamed".to_owned())
            ]]
        );
        assert_eq!(
            store.listed(BACKEND).expect("the list is read"),
            [(ShowId(1), renamed)]
        );
    }

    #[test]
    fn one_number_on_two_backends_is_two_shows_each_listed_by_its_own() {
        // One number on two services names two works.
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("one list is filed");

        store
            .file("another", &[a_series(21, "Another Show")])
            .expect("the other list is filed");

        assert_eq!(
            store.listed(BACKEND).expect("the list is read"),
            [(ShowId(1), a_series(21, "Show Title"))]
        );
        assert_eq!(
            store.listed("another").expect("the list is read"),
            [(ShowId(2), a_series(21, "Another Show"))]
        );
    }

    #[test]
    fn the_list_of_one_backend_leaves_the_list_of_another_as_it_was() {
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("one list is filed");
        store
            .file("another", &[a_series(21, "Another Show")])
            .expect("the other list is filed");

        store
            .file("another", &[])
            .expect("the other list is filed empty");

        assert_eq!(
            store.listed(BACKEND).expect("the list is read"),
            [(ShowId(1), a_series(21, "Show Title"))]
        );
    }

    #[test]
    fn a_list_that_holds_an_entry_twice_is_refused_and_nothing_of_it_is_filed() {
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("the list is filed");
        let before = every_row(&store);

        let refused = store.file(BACKEND, &[a_film(112_151), a_film(112_151)]);

        assert!(
            matches!(
                &refused,
                Err(Error::Write(rusqlite::Error::SqliteFailure(failure, _)))
                    if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
            ),
            "got {refused:?}"
        );
        assert_eq!(every_row(&store), before);
    }

    #[test]
    fn an_entry_is_kept_in_columns_sqlite3_reads() {
        let (_home, mut store) = a_store();

        store
            .file(BACKEND, &[a_film(112_151)])
            .expect("the list is filed");

        assert_eq!(
            rows(&store, "list_entries"),
            [[
                Value::Text(BACKEND.to_owned()),
                Value::Integer(112_151),
                Value::Text("[]".to_owned()),
                Value::Text("film".to_owned()),
                Value::Integer(1),
                Value::Integer(5_700_000),
                Value::Integer(1988),
                Value::Integer(1),
                Value::Text("completed".to_owned()),
                Value::Integer(1),
                Value::Integer(1),
                Value::Integer(80),
            ]]
        );
    }

    #[test]
    fn every_format_is_kept_under_a_name_of_its_own() {
        // The name is what the file holds, so a variant renamed in the code
        // must not rename it in the file.
        let (_home, mut store) = a_store();
        let every = [
            (Format::Tv, "tv"),
            (Format::TvShort, "tv_short"),
            (Format::Film, "film"),
            (Format::Special, "special"),
            (Format::Ova, "ova"),
            (Format::Ona, "ona"),
            (Format::Music, "music"),
        ];
        let list: Vec<Entry> = (1..)
            .zip(every)
            .map(|(id, (format, _))| {
                match format {
                    Format::Tv
                    | Format::TvShort
                    | Format::Film
                    | Format::Special
                    | Format::Ova
                    | Format::Ona
                    | Format::Music => {}
                }
                Entry {
                    format: Some(format),
                    ..a_series(id, "Show Title")
                }
            })
            .collect();

        store.file(BACKEND, &list).expect("the list is filed");

        let kept: Vec<String> = store
            .connection
            .prepare("SELECT format FROM list_entries ORDER BY entry")
            .expect("the statement prepares")
            .query_map([], |row| row.get(0))
            .expect("the statement runs")
            .collect::<Result<_, _>>()
            .expect("every row has a format");
        assert_eq!(kept, every.map(|(_, name)| name));
    }

    #[test]
    fn an_entry_that_cannot_be_read_is_refused_and_named() {
        // Refused whole rather than read around: an entry left out would be
        // a show no file is matched to, and nothing would say so.
        for (column, value) in [
            ("status", "'binging'"),
            ("format", "'serial'"),
            ("score", "0"),
            ("score", "101"),
            ("spellings", "'Show Title'"),
        ] {
            let (_home, mut store) = a_store();
            store
                .file(BACKEND, &[a_series(21, "Show Title")])
                .expect("the list is filed");
            store
                .connection
                .execute(&format!("UPDATE list_entries SET {column} = {value}"), [])
                .expect("the row is changed by hand");

            let refused = store.listed(BACKEND);

            assert!(
                matches!(
                    &refused,
                    Err(Error::Entry { backend, entry: EntryId(21), .. }) if backend == BACKEND
                ),
                "{column} of {value} got {refused:?}"
            );
        }
    }

    #[test]
    fn an_entry_whose_show_is_not_there_is_refused_and_named() {
        // Foreign keys keep such a row from being written through the store.
        // `sqlite3` leaves them off unless it is asked, so a link or a show
        // deleted by hand leaves one.
        for deleted in ["DELETE FROM backend_ids", "DELETE FROM shows"] {
            let (_home, mut store) = a_store();
            store
                .file(BACKEND, &[a_series(21, "Show Title")])
                .expect("the list is filed");
            store
                .connection
                .execute_batch(&format!(
                    "PRAGMA foreign_keys = OFF; {deleted}; PRAGMA foreign_keys = ON;"
                ))
                .expect("the row goes with nothing checking it");

            let refused = store.listed(BACKEND);

            assert!(
                matches!(
                    &refused,
                    Err(Error::Showless { backend, entry: EntryId(21) }) if backend == BACKEND
                ),
                "{deleted} got {refused:?}"
            );
        }
    }

    #[test]
    fn a_length_longer_than_the_column_holds_is_kept_as_the_longest_it_holds() {
        let (_home, mut store) = a_store();
        let endless = Entry {
            episode_length: Some(Duration::MAX),
            ..a_series(21, "Show Title")
        };

        store.file(BACKEND, &[endless]).expect("the list is filed");

        let episode_ms: i64 = store
            .connection
            .query_row("SELECT episode_ms FROM list_entries", [], |row| row.get(0))
            .expect("one row");
        assert_eq!(episode_ms, i64::MAX);
    }

    #[test]
    fn a_length_below_zero_is_refused() {
        let (_home, mut store) = a_store();
        store
            .file(BACKEND, &[a_series(21, "Show Title")])
            .expect("the list is filed");
        store
            .connection
            .execute("UPDATE list_entries SET episode_ms = -5", [])
            .expect("the row is changed by hand");

        let refused = store.listed(BACKEND);

        assert!(
            matches!(
                refused,
                Err(Error::Write(rusqlite::Error::IntegralValueOutOfRange(
                    6, -5
                )))
            ),
            "got {refused:?}"
        );
    }
}
