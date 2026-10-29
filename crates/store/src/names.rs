//! The names a list does not hold, each kept as a show.
//!
//! A name that spells a title and reaches no entry of the list is a show with
//! no link, and the name is found again by its key. The names of an entry
//! that left the list are kept as the show it is linked to, as
//! [`Store::file`] says. The key is kept in its parts, as [`Key`] answers
//! them, so the key a name was kept under is read back as it was and never
//! built again from text.

use benshi_core::ShowId;
use benshi_core::recognize::normalize::Key;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Store};

impl Store {
    /// The show a name the list does not hold is kept as.
    ///
    /// Found by the name's key alone, the same show every time: a name that
    /// differs in its marks, its season or its part has a key of its own. A
    /// key that is not kept is filed as a show titled `title`. A key
    /// [`Store::file`] kept for an entry that left the list is the show of
    /// that entry. A row's season or part of -1 is found as none, because the
    /// index that keeps a key to one row reads none as -1.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when a statement fails for any other reason. Nothing
    /// is filed with either.
    pub fn unlisted(&mut self, key: &Key, title: &str) -> Result<ShowId, Error> {
        let transaction = self.connection.transaction()?;
        let kept: Option<i64> = transaction
            .query_row(
                "SELECT show FROM names \
                 WHERE letters = ?1 AND marks = ?2 \
                 AND IFNULL(season, -1) = IFNULL(?3, -1) AND IFNULL(part, -1) = IFNULL(?4, -1)",
                params![key.letters(), key.marks(), key.season(), key.part()],
                |row| row.get(0),
            )
            .optional()?;
        let show = if let Some(show) = kept {
            show
        } else {
            let show: i64 = transaction.query_row(
                "INSERT INTO shows (title) VALUES (?1) RETURNING id",
                [title],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO names (show, letters, marks, season, part) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![show, key.letters(), key.marks(), key.season(), key.part()],
            )?;
            show
        };
        transaction.commit()?;

        Ok(ShowId(show))
    }

    /// Every name kept, with the show it is kept as, in the order they were
    /// kept.
    ///
    /// # Errors
    ///
    /// [`Error::Name`] for a row whose letters or marks [`Key::from_parts`]
    /// refuses, naming its show. [`Error::Busy`] when the database cannot be
    /// read because another connection has it, and [`Error::Write`] when it
    /// cannot be read for any other reason, a season or a part out of the
    /// range of its field included.
    pub fn unlisted_names(&self) -> Result<Vec<(Key, ShowId)>, Error> {
        let mut statement = self
            .connection
            .prepare("SELECT show, letters, marks, season, part FROM names ORDER BY rowid")?;

        statement
            .query_and_then([], |row| {
                let show = ShowId(row.get(0)?);
                let letters: String = row.get(1)?;
                let marks: String = row.get(2)?;
                let key = Key::from_parts(&letters, &marks, row.get(3)?, row.get(4)?)
                    .ok_or(Error::Name { show })?;
                Ok((key, show))
            })?
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use benshi_core::ShowId;
    use benshi_core::recognize::normalize::Key;
    use rusqlite::Connection;

    use crate::{Error, Store};

    /// A store of its own, in a directory that goes with it.
    fn a_store() -> (tempfile::TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("the database opens");
        (home, store)
    }

    /// The key a list entry spelled this way is filed under.
    fn key(spelled: &str) -> Key {
        Key::from_title(spelled).expect("a title with letters in it has a key")
    }

    #[test]
    fn a_name_is_kept_as_a_show_of_its_own_and_found_again_by_its_key() {
        let (home, mut store) = a_store();

        let first = store
            .unlisted(&key("Some Other Show"), "Some Other Show")
            .expect("the name is kept");
        let again = store
            .unlisted(&key("some other show"), "some other show")
            .expect("the name is found");
        drop(store);
        let mut opened =
            Store::open(&home.path().join("benshi.db")).expect("the database opens again");
        let after = opened
            .unlisted(&key("Some Other Show"), "Some Other Show")
            .expect("the name is found again");

        assert_eq!(again, first);
        assert_eq!(after, first);
        let connection = Connection::open(home.path().join("benshi.db")).expect("the file opens");
        let titles: Vec<String> = connection
            .prepare("SELECT title FROM shows ORDER BY id")
            .expect("a query")
            .query_map([], |row| row.get(0))
            .expect("the query runs")
            .collect::<Result<_, _>>()
            .expect("every row reads");
        assert_eq!(titles, ["Some Other Show"]);
        let links: u32 = connection
            .query_row("SELECT count(*) FROM backend_ids", [], |row| row.get(0))
            .expect("the count reads");
        assert_eq!(links, 0, "a show kept for a name is linked to no entry");
    }

    #[test]
    fn names_that_differ_in_marks_season_or_part_are_shows_of_their_own() {
        let (_home, mut store) = a_store();
        let mut shows = Vec::new();
        for spelled in [
            "Show Title",
            "Show Title:",
            "Show Title Season 2",
            "Show Title Part 2",
            "Show Title Season 2 Part 2",
        ] {
            shows.push(
                store
                    .unlisted(&key(spelled), spelled)
                    .expect("the name is kept"),
            );
        }

        let distinct: BTreeSet<ShowId> = shows.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            shows.len(),
            "two names shared a show: {shows:?}"
        );
    }

    #[test]
    fn every_name_kept_reads_back_with_the_key_it_was_kept_under() {
        let (_home, mut store) = a_store();
        let one = key("Some Other Show");
        let two = key("K-ON!! Season 3 Part 2");
        let first = store
            .unlisted(&one, "Some Other Show")
            .expect("the name is kept");
        let second = store
            .unlisted(&two, "K-ON!! Season 3 Part 2")
            .expect("the name is kept");

        assert_eq!(
            store.unlisted_names().expect("the names read"),
            [(one, first), (two, second)]
        );
    }

    #[test]
    fn a_name_whose_parts_are_no_key_is_refused_with_its_show() {
        let (home, mut store) = a_store();
        let show = store
            .unlisted(&key("Some Other Show"), "Some Other Show")
            .expect("the name is kept");
        Connection::open(home.path().join("benshi.db"))
            .expect("the file opens")
            .execute("UPDATE names SET letters = ''", [])
            .expect("the row is changed by hand");

        assert!(
            matches!(store.unlisted_names(), Err(Error::Name { show: refused }) if refused == show),
            "{:?}",
            store.unlisted_names()
        );
        assert_ne!(show, ShowId(0));
    }

    #[test]
    fn a_season_no_key_can_hold_is_a_failure_to_read() {
        let (home, mut store) = a_store();
        store
            .unlisted(&key("Some Other Show"), "Some Other Show")
            .expect("the name is kept");
        Connection::open(home.path().join("benshi.db"))
            .expect("the file opens")
            .execute("UPDATE names SET season = -1", [])
            .expect("the row is changed by hand");

        assert!(
            matches!(store.unlisted_names(), Err(Error::Write(_))),
            "{:?}",
            store.unlisted_names()
        );
    }

    #[test]
    fn a_season_and_a_part_of_minus_one_are_found_as_none() {
        // The index that keeps a key to one row reads none as -1, so it
        // refuses a row filed with none beside a row of -1.
        let (home, mut store) = a_store();
        let show = store
            .unlisted(&key("Some Other Show"), "Some Other Show")
            .expect("the name is kept");
        Connection::open(home.path().join("benshi.db"))
            .expect("the file opens")
            .execute("UPDATE names SET season = -1, part = -1", [])
            .expect("the row is changed by hand");

        let found = store.unlisted(&key("Some Other Show"), "Some Other Show");

        assert!(matches!(found, Ok(again) if again == show), "{found:?}");
    }

    #[test]
    fn a_name_that_cannot_be_filed_files_no_show() {
        let (home, mut store) = a_store();
        let by_hand = Connection::open(home.path().join("benshi.db")).expect("the file opens");
        by_hand
            .execute_batch(
                "CREATE TRIGGER no_names BEFORE INSERT ON names \
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .expect("the trigger is made");

        let kept = store.unlisted(&key("Some Other Show"), "Some Other Show");

        assert!(matches!(kept, Err(Error::Write(_))), "{kept:?}");
        let shows: i64 = by_hand
            .query_row("SELECT count(*) FROM shows", [], |row| row.get(0))
            .expect("the shows are counted");
        assert_eq!(shows, 0);
    }
}
