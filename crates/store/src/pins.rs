//! The episodes of a show the user said are episodes of an entry.
//!
//! What `benshi map "<title>" ep 14-26 --anilist <id>:1-13` keeps: a
//! [`Pin`] of one backend. A show has at most one pin of a backend that
//! begins at a given episode.

use benshi_core::ShowId;
use benshi_core::list::EntryId;
use benshi_core::pin::{Episodes, Pin};
use rusqlite::params;
use rusqlite::types::Value;

use crate::{Error, Store};

impl Store {
    /// Keep a pin of `backend`, in place of the pin of its show that begins
    /// at the same episode, and in that pin's place among the others.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when the statement fails for any other reason, a show
    /// the store has not filed among them.
    pub fn pin(&mut self, backend: &str, pin: &Pin) -> Result<(), Error> {
        let (episodes, entry_episodes) = (pin.episodes(), pin.entry_episodes());
        self.connection.execute(
            "INSERT INTO pins (backend, show, first, last, entry, entry_first, entry_last) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (backend, show, first) DO UPDATE SET last = excluded.last, \
             entry = excluded.entry, entry_first = excluded.entry_first, \
             entry_last = excluded.entry_last",
            params![
                backend,
                pin.show().0,
                episodes.first(),
                episodes.last(),
                pin.entry().0,
                entry_episodes.first(),
                entry_episodes.last()
            ],
        )?;
        Ok(())
    }

    /// Take back the pin of `backend` of these episodes of a show, and
    /// answer whether there was one.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when the statement fails for any other reason.
    pub fn unpin(
        &mut self,
        backend: &str,
        show: ShowId,
        episodes: Episodes,
    ) -> Result<bool, Error> {
        let taken = self.connection.execute(
            "DELETE FROM pins WHERE backend = ?1 AND show = ?2 AND first = ?3 AND last = ?4",
            params![backend, show.0, episodes.first(), episodes.last()],
        )?;
        Ok(taken > 0)
    }

    /// Every pin of `backend`, in the order they were made.
    ///
    /// A row that holds no pin is passed over. The checks of the table hold
    /// the numbers of a row and not their types, and can be turned off.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when the database cannot be read because another
    /// connection has it, and [`Error::Write`] when it cannot be read for
    /// any other reason.
    pub fn pins(&self, backend: &str) -> Result<Vec<Pin>, Error> {
        let mut statement = self.connection.prepare(
            "SELECT show, first, last, entry, entry_first, entry_last FROM pins \
             WHERE backend = ?1 ORDER BY rowid",
        )?;
        let rows = statement
            .query_map([backend], |row| {
                (0..6)
                    .map(|column| row.get::<_, Value>(column))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.iter().filter_map(|row| pin_of(row)).collect())
    }
}

/// The pin the six values of a row hold, and none where they hold none.
fn pin_of(row: &[Value]) -> Option<Pin> {
    let numbers = row
        .iter()
        .map(|value| match value {
            Value::Integer(number) => Some(*number),
            _ => None,
        })
        .collect::<Option<Vec<i64>>>()?;
    let [show, first, last, entry, entry_first, entry_last] = numbers[..] else {
        return None;
    };
    Pin::new(
        ShowId(show),
        run(first, last)?,
        EntryId(entry),
        run(entry_first, entry_last)?,
    )
    .ok()
}

/// The run of two numbers of a row, and none where they make no run.
fn run(first: i64, last: i64) -> Option<Episodes> {
    Episodes::new(u32::try_from(first).ok()?, u32::try_from(last).ok()?)
}

#[cfg(test)]
mod tests {
    use benshi_core::ShowId;
    use benshi_core::list::EntryId;
    use benshi_core::pin::{Episodes, Pin};
    use tempfile::TempDir;

    use crate::{Error, Store};

    const ANILIST: &str = "anilist";
    /// The first show a fresh store files.
    const SHOW: ShowId = ShowId(1);
    /// The second.
    const OTHER: ShowId = ShowId(2);

    fn a_store() -> (TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        for (show, title) in [(SHOW, "Show Title"), (OTHER, "Other Title")] {
            assert_eq!(store.new_show(title).expect("the show is filed"), show);
        }
        (home, store)
    }

    fn run(first: u32, last: u32) -> Episodes {
        Episodes::new(first, last).expect("a run from its first episode to its last")
    }

    /// Episodes of `show` from `first` to `last` as the same count of an
    /// entry's, from `entry_first`.
    fn a_pin(show: ShowId, first: u32, last: u32, entry: i64, entry_first: u32) -> Pin {
        Pin::new(
            show,
            run(first, last),
            EntryId(entry),
            run(entry_first, entry_first + (last - first)),
        )
        .expect("runs of one length")
    }

    #[test]
    fn a_pin_kept_is_read_back() {
        let (_home, mut store) = a_store();
        let pin = a_pin(SHOW, 14, 26, 12345, 1);

        store.pin(ANILIST, &pin).expect("the pin is kept");

        assert_eq!(store.pins(ANILIST).expect("the store answers"), [pin]);
    }

    #[test]
    fn pins_are_read_in_the_order_they_were_made() {
        let (_home, mut store) = a_store();
        let pins = [
            a_pin(OTHER, 2, 2, 111_954, 1),
            a_pin(SHOW, 14, 26, 12345, 1),
            a_pin(SHOW, 1, 13, 678, 1),
        ];
        for pin in &pins {
            store.pin(ANILIST, pin).expect("the pin is kept");
        }

        assert_eq!(store.pins(ANILIST).expect("the store answers"), pins);
    }

    #[test]
    fn a_pin_from_the_episode_another_began_at_takes_its_place() {
        let (_home, mut store) = a_store();
        for pin in [
            a_pin(SHOW, 14, 26, 12345, 1),
            a_pin(OTHER, 2, 2, 111_954, 1),
        ] {
            store.pin(ANILIST, &pin).expect("the pin is kept");
        }
        let again = a_pin(SHOW, 14, 26, 678, 14);

        store.pin(ANILIST, &again).expect("the pin is kept");

        assert_eq!(
            store.pins(ANILIST).expect("the store answers"),
            [again, a_pin(OTHER, 2, 2, 111_954, 1)]
        );
    }

    #[test]
    fn a_pin_of_another_backend_is_not_read() {
        let (_home, mut store) = a_store();
        store
            .pin("another", &a_pin(SHOW, 14, 26, 12345, 1))
            .expect("the pin is kept");
        let pin = a_pin(SHOW, 14, 26, 678, 1);
        store.pin(ANILIST, &pin).expect("the pin is kept");

        assert_eq!(store.pins(ANILIST).expect("the store answers"), [pin]);
    }

    #[test]
    fn a_pin_taken_back_is_gone() {
        let (_home, mut store) = a_store();
        let other = a_pin(OTHER, 2, 2, 111_954, 1);
        for pin in [a_pin(SHOW, 14, 26, 12345, 1), other] {
            store.pin(ANILIST, &pin).expect("the pin is kept");
        }

        let taken = store
            .unpin(ANILIST, SHOW, run(14, 26))
            .expect("the store answers");
        let again = store
            .unpin(ANILIST, SHOW, run(14, 26))
            .expect("the store answers");

        assert!(taken);
        assert!(!again, "nothing was left to take back");
        assert_eq!(store.pins(ANILIST).expect("the store answers"), [other]);
    }

    #[test]
    fn a_pin_is_taken_back_by_its_own_episodes_of_its_own_backend_alone() {
        let (_home, mut store) = a_store();
        let pin = a_pin(SHOW, 14, 26, 12345, 1);
        store.pin(ANILIST, &pin).expect("the pin is kept");

        for (backend, show, episodes) in [
            (ANILIST, SHOW, run(14, 20)),
            (ANILIST, SHOW, run(14, 27)),
            (ANILIST, OTHER, run(14, 26)),
            ("another", SHOW, run(14, 26)),
        ] {
            let taken = store
                .unpin(backend, show, episodes)
                .expect("the store answers");
            assert!(!taken, "{backend} {show:?} {episodes:?}");
        }
        assert_eq!(store.pins(ANILIST).expect("the store answers"), [pin]);
    }

    #[test]
    fn a_pin_of_a_show_that_is_not_filed_is_refused_and_nothing_is_kept() {
        let (_home, mut store) = a_store();

        let refused = store.pin(ANILIST, &a_pin(ShowId(42), 14, 26, 12345, 1));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        assert_eq!(store.pins(ANILIST).expect("the store answers"), []);
    }

    #[test]
    fn a_row_that_is_not_a_pin_is_passed_over() {
        // The checks hold a row's numbers and not their types, and can be
        // turned off; a listing that stopped at such a row would stop for good.
        let (_home, mut store) = a_store();
        let pin = a_pin(SHOW, 14, 26, 12345, 1);
        store.pin(ANILIST, &pin).expect("the pin is kept");
        store
            .connection
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 INSERT INTO pins (backend, show, first, last, entry, entry_first, entry_last)
                 VALUES ('anilist', 2, 30, 20, 678, 1, 11),
                        ('anilist', 2, 4294967296, 4294967296, 678, 1, 1);
                 PRAGMA ignore_check_constraints = OFF;
                 INSERT INTO pins (backend, show, first, last, entry, entry_first, entry_last)
                 VALUES ('anilist', 1, 1.5, 1.5, 678, 1, 1),
                        ('anilist', 1, 40, 40, 'abc', 1, 1);",
            )
            .expect("the rows go in");

        assert_eq!(store.pins(ANILIST).expect("the store answers"), [pin]);
    }

    #[test]
    fn a_pin_is_still_there_after_the_store_is_opened_again() {
        let (home, mut store) = a_store();
        let pin = a_pin(SHOW, 14, 26, 12345, 1);
        store.pin(ANILIST, &pin).expect("the pin is kept");
        drop(store);

        let reopened =
            Store::open(&home.path().join("benshi.db")).expect("the database opens again");

        assert_eq!(reopened.pins(ANILIST).expect("the store answers"), [pin]);
    }
}
