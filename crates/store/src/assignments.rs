//! The names the user gave entries by hand.
//!
//! What `benshi map` makes: a name, as typed, that means one entry of the
//! list from then on. The text is kept verbatim, as
//! [`Altnames`](benshi_core::recognize::altname::Altnames) keeps it, with the
//! show of the entry it names. The key is derived from the text each time a
//! recognizer is built, and never kept in its place.

use benshi_core::ShowId;
use rusqlite::params;

use crate::{Error, Store};

impl Store {
    /// Keep that `spelled` means `show`, in place of the names `displacing`
    /// holds.
    ///
    /// Text named again takes its new show and keeps its place among the
    /// others.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when a statement fails for any other reason, a show
    /// the store has not filed among them. The transaction is rolled back
    /// with either.
    pub fn assign(
        &mut self,
        spelled: &str,
        show: ShowId,
        displacing: &[&str],
    ) -> Result<(), Error> {
        let transaction = self.connection.transaction()?;
        for displaced in displacing.iter().filter(|displaced| **displaced != spelled) {
            transaction.execute("DELETE FROM assignments WHERE spelled = ?1", [displaced])?;
        }
        transaction.execute(
            "INSERT INTO assignments (spelled, show) VALUES (?1, ?2) \
             ON CONFLICT (spelled) DO UPDATE SET show = excluded.show",
            params![spelled, show.0],
        )?;
        transaction.commit()?;

        Ok(())
    }

    /// Take back the assignment of this text, and answer whether there was
    /// one.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when the statement fails for any other reason.
    pub fn unassign(&mut self, spelled: &str) -> Result<bool, Error> {
        let taken = self
            .connection
            .execute("DELETE FROM assignments WHERE spelled = ?1", [spelled])?;
        Ok(taken > 0)
    }

    /// Every assignment kept, the text and its show, in the order they were
    /// made.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when the database cannot be read because another
    /// connection has it, and [`Error::Write`] when it cannot be read for
    /// any other reason.
    pub fn assignments(&self) -> Result<Vec<(String, ShowId)>, Error> {
        let mut statement = self
            .connection
            .prepare("SELECT spelled, show FROM assignments ORDER BY rowid")?;
        let assignments = statement
            .query_map([], |row| Ok((row.get(0)?, ShowId(row.get(1)?))))?
            .collect::<Result<_, _>>()?;
        Ok(assignments)
    }
}

#[cfg(test)]
mod tests {
    use benshi_core::ShowId;
    use tempfile::TempDir;

    use crate::{Error, Store};

    /// The first entry a fresh store files.
    const NISEKOI: ShowId = ShowId(1);
    /// The second, a sequel told apart by a mark.
    const SEQUEL: ShowId = ShowId(2);

    fn a_store() -> (TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        for (show, title) in [(NISEKOI, "Nisekoi"), (SEQUEL, "Nisekoi:")] {
            assert_eq!(store.new_show(title).expect("the show is filed"), show);
        }
        (home, store)
    }

    fn kept(pairs: &[(&str, ShowId)]) -> Vec<(String, ShowId)> {
        pairs
            .iter()
            .map(|(spelled, show)| ((*spelled).to_owned(), *show))
            .collect()
    }

    #[test]
    fn an_assignment_kept_is_read_back() {
        let (_home, mut store) = a_store();

        store
            .assign("Nisekoi", SEQUEL, &[])
            .expect("the assignment is kept");

        assert_eq!(
            store.assignments().expect("the store answers"),
            kept(&[("Nisekoi", SEQUEL)])
        );
    }

    #[test]
    fn an_assignment_takes_the_place_of_those_it_displaces() {
        // Spellings of one key: the last displaces the others, and a name
        // of another key stays.
        let (_home, mut store) = a_store();
        for spelled in ["Nisekoi", "Another Name", "nisekoi"] {
            store
                .assign(spelled, SEQUEL, &[])
                .expect("the assignment is kept");
        }

        store
            .assign("NISEKOI", NISEKOI, &["Nisekoi", "nisekoi"])
            .expect("the assignment is kept");

        assert_eq!(
            store.assignments().expect("the store answers"),
            kept(&[("Another Name", SEQUEL), ("NISEKOI", NISEKOI)])
        );
    }

    #[test]
    fn the_same_text_named_again_is_one_assignment_in_its_place() {
        let (_home, mut store) = a_store();
        for spelled in ["Nisekoi", "Another Name"] {
            store
                .assign(spelled, SEQUEL, &[])
                .expect("the assignment is kept");
        }

        store
            .assign("Nisekoi", NISEKOI, &["Nisekoi"])
            .expect("the assignment is kept");

        assert_eq!(
            store.assignments().expect("the store answers"),
            kept(&[("Nisekoi", NISEKOI), ("Another Name", SEQUEL)])
        );
    }

    #[test]
    fn an_assignment_taken_back_is_gone() {
        let (_home, mut store) = a_store();
        for spelled in ["Nisekoi", "Another Name"] {
            store
                .assign(spelled, SEQUEL, &[])
                .expect("the assignment is kept");
        }

        let taken = store.unassign("Nisekoi").expect("the store answers");
        let again = store.unassign("Nisekoi").expect("the store answers");

        assert!(taken);
        assert!(!again, "nothing was left to take back");
        assert_eq!(
            store.assignments().expect("the store answers"),
            kept(&[("Another Name", SEQUEL)])
        );
    }

    #[test]
    fn assignments_are_read_in_the_order_they_were_made() {
        let (_home, mut store) = a_store();
        for (spelled, show) in [("Zeta", SEQUEL), ("Alpha", NISEKOI), ("Mu", SEQUEL)] {
            store
                .assign(spelled, show, &[])
                .expect("the assignment is kept");
        }

        assert_eq!(
            store.assignments().expect("the store answers"),
            kept(&[("Zeta", SEQUEL), ("Alpha", NISEKOI), ("Mu", SEQUEL)])
        );
    }

    #[test]
    fn an_assignment_of_a_show_that_is_not_filed_is_refused_and_nothing_is_kept() {
        let (_home, mut store) = a_store();
        store
            .assign("Nisekoi", SEQUEL, &[])
            .expect("the assignment is kept");

        let refused = store.assign("Show Title", ShowId(42), &["Nisekoi"]);

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        assert_eq!(
            store.assignments().expect("the store answers"),
            kept(&[("Nisekoi", SEQUEL)]),
            "the one it was to displace is still there"
        );
    }

    #[test]
    fn an_assignment_is_still_there_after_the_store_is_opened_again() {
        let (home, mut store) = a_store();
        store
            .assign("Nisekoi", SEQUEL, &[])
            .expect("the assignment is kept");
        drop(store);

        let reopened =
            Store::open(&home.path().join("benshi.db")).expect("the database opens again");

        assert_eq!(
            reopened.assignments().expect("the store answers"),
            kept(&[("Nisekoi", SEQUEL)])
        );
    }
}
