//! Where each backend is in the log of operations.
//!
//! Every backend reads the one log, `sync_queue`, and keeps its own place in
//! it: a cursor at the last operation it is done with, the write it decided
//! for an operation and has not had confirmed, and the operations it set
//! aside, each with the reason. Nothing deletes from the log, so the number of
//! an operation is never given to another.
//!
//! A write is kept so that it can be sent again as it was decided. Deciding it
//! reads the list: a film seen again at 3 rewatches is written as 4, and
//! deciding again after a write whose answer was lost would write 5.
//!
//! The cursor moves past an operation that is delivered or set aside, and
//! never back. [`Store::pending`] answers the operations past it, lowest
//! first, so a backend that takes them from there does them in the order of
//! the log.

use benshi_core::list::Write;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::queue::{LOG, OPERATION, Operation, read_operation, text_bytes};
use crate::{Error, Store};

/// An operation of the log past the cursor of a backend.
#[derive(Debug)]
pub struct Pending {
    /// The operation's `id` in `sync_queue`.
    pub id: i64,
    /// The operation, or why its row holds none: [`Error::Orphan`] or
    /// [`Error::Operation`].
    pub operation: Result<Operation, Error>,
}

/// An operation a backend set aside, and why.
#[derive(Debug)]
pub struct Aside {
    /// The operation's `id` in `sync_queue`.
    pub id: i64,
    /// The operation, or why its row holds none, as
    /// [`Pending::operation`].
    pub operation: Result<Operation, Error>,
    /// Why it was set aside, as a person reads it.
    pub reason: String,
}

impl Store {
    /// The operations of the log past the cursor of `backend`, lowest `id`
    /// first.
    ///
    /// A row that holds no operation is answered in its place, with why, so
    /// that a backend can set it aside and go on to the next.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when the log cannot be read for any other reason.
    pub fn pending(&self, backend: &str) -> Result<Vec<Pending>, Error> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {OPERATION} FROM {LOG} \
             WHERE sync_queue.id > IFNULL((SELECT operation FROM cursors WHERE backend = ?1), 0) \
             ORDER BY sync_queue.id"
        ))?;
        statement
            .query_map([backend], read_operation)?
            .map(|row| {
                let (id, operation) = row?;
                Ok(Pending { id, operation })
            })
            .collect()
    }

    /// Every operation `backend` set aside, lowest `id` first, each with its
    /// reason, and a row that holds no operation answered as
    /// [`Store::pending`] answers one.
    ///
    /// # Errors
    ///
    /// As [`Store::pending`].
    pub fn aside(&self, backend: &str) -> Result<Vec<Aside>, Error> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {OPERATION}, set_aside.reason \
             FROM {LOG} JOIN set_aside ON set_aside.operation = sync_queue.id \
             WHERE set_aside.backend = ?1 ORDER BY sync_queue.id"
        ))?;
        statement
            .query_map([backend], |row| {
                let reason = String::from_utf8_lossy(text_bytes(row.get_ref(6)?)).into_owned();
                Ok((read_operation(row)?, reason))
            })?
            .map(|row| {
                let ((id, operation), reason) = row?;
                Ok(Aside {
                    id,
                    operation,
                    reason,
                })
            })
            .collect()
    }

    /// Keep the write `backend` decided for `operation`, in place of one
    /// kept for it before.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when the statement fails for any other reason, as it
    /// does for a number that is not in the log.
    pub fn decide(&mut self, backend: &str, operation: i64, write: &Write) -> Result<(), Error> {
        let write = serde_json::to_string(write)
            .map_err(|cause| Error::Write(rusqlite::Error::ToSqlConversionFailure(cause.into())))?;
        self.connection.execute(
            "INSERT INTO decided (backend, operation, write) VALUES (?1, ?2, ?3) \
             ON CONFLICT (backend, operation) DO UPDATE SET write = excluded.write",
            params![backend, operation, write],
        )?;
        Ok(())
    }

    /// The write `backend` decided for `operation` and kept, where it kept
    /// one.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] and [`Error::Write`] where the database cannot be
    /// read, and [`Error::Decided`] where the write kept cannot be read as
    /// one.
    pub fn decided(&self, backend: &str, operation: i64) -> Result<Option<Write>, Error> {
        let kept = self
            .connection
            .query_row(
                "SELECT write FROM decided WHERE backend = ?1 AND operation = ?2",
                params![backend, operation],
                |row| Ok(text_bytes(row.get_ref(0)?).to_vec()),
            )
            .optional()?;
        kept.map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|cause| Error::Decided {
                backend: backend.to_owned(),
                operation,
                cause,
            })
        })
        .transpose()
    }

    /// `backend` delivered `operation`: the write kept for it and its place
    /// aside go, and the cursor moves past it, as one.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when a statement fails for any other reason, as it
    /// does for a number past the cursor that is not in the log. Nothing
    /// changes with either.
    pub fn delivered(&mut self, backend: &str, operation: i64) -> Result<(), Error> {
        let transaction = self.connection.transaction()?;
        forget_decided(&transaction, backend, operation)?;
        transaction.execute(
            "DELETE FROM set_aside WHERE backend = ?1 AND operation = ?2",
            params![backend, operation],
        )?;
        advance(&transaction, backend, operation)?;
        transaction.commit()?;
        Ok(())
    }

    /// `backend` set `operation` aside for `reason`, in place of a reason
    /// given before: the write kept for it goes, and the cursor moves past
    /// it, as one.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when a statement fails for any other reason, as it
    /// does for a number that is not in the log. Nothing changes with
    /// either.
    pub fn set_aside(&mut self, backend: &str, operation: i64, reason: &str) -> Result<(), Error> {
        let transaction = self.connection.transaction()?;
        forget_decided(&transaction, backend, operation)?;
        transaction.execute(
            "INSERT INTO set_aside (backend, operation, reason) VALUES (?1, ?2, ?3) \
             ON CONFLICT (backend, operation) DO UPDATE SET reason = excluded.reason",
            params![backend, operation, reason],
        )?;
        advance(&transaction, backend, operation)?;
        transaction.commit()?;
        Ok(())
    }
}

/// Take away the write `backend` kept for `operation`.
fn forget_decided(
    transaction: &Transaction<'_>,
    backend: &str,
    operation: i64,
) -> Result<(), Error> {
    transaction.execute(
        "DELETE FROM decided WHERE backend = ?1 AND operation = ?2",
        params![backend, operation],
    )?;
    Ok(())
}

/// Move the cursor of `backend` past `operation`, and never back.
fn advance(transaction: &Transaction<'_>, backend: &str, operation: i64) -> Result<(), Error> {
    transaction.execute(
        "INSERT INTO cursors (backend, operation) VALUES (?1, ?2) \
         ON CONFLICT (backend) DO UPDATE SET operation = MAX(operation, excluded.operation)",
        params![backend, operation],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use benshi_core::list::{EntryId, Status, Write};
    use tempfile::TempDir;

    use super::Aside;
    use crate::queue::{Kind, Operation, enqueue};
    use crate::{Error, Store};

    const ANILIST: &str = "anilist";
    const ANOTHER: &str = "another";

    /// A store whose log holds a progress of episodes 1 to `episodes` of
    /// one show, with the numbers of those operations.
    fn a_log_of(episodes: u32) -> (TempDir, Store, Vec<i64>) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        let show = store.new_show("Show Title").expect("the show is filed");
        for episode in 1..=episodes {
            enqueue(
                &store.connection,
                show.0,
                &Kind::Progress {
                    episode: Some(episode),
                },
                "2026-11-05T00:00:00Z",
            )
            .expect("the operation is queued");
        }
        let operations = numbers(&store.queued().expect("the log reads"));
        (home, store, operations)
    }

    fn numbers(operations: &[Operation]) -> Vec<i64> {
        operations.iter().map(|operation| operation.id).collect()
    }

    /// The numbers of the operations past the cursor of `backend`, each read.
    fn pending(store: &Store, backend: &str) -> Vec<i64> {
        store
            .pending(backend)
            .expect("the log reads")
            .into_iter()
            .map(|pending| pending.operation.expect("the operation reads").id)
            .collect()
    }

    /// What `aside` holds, its operation read.
    fn read_aside(aside: &Aside) -> (i64, &str) {
        let operation = aside.operation.as_ref().expect("the operation reads");
        (operation.id, aside.reason.as_str())
    }

    /// The payload of `operation` changed by hand, which is how a row this
    /// build did not write gets into the log.
    fn spoil(store: &Store, operation: i64) {
        store
            .connection
            .execute(
                "UPDATE sync_queue SET payload = 'episode three' WHERE id = ?1",
                [operation],
            )
            .expect("the row is changed");
    }

    /// The show of `operation` changed by hand to one that is not there,
    /// with the foreign keys off, as `sqlite3` leaves them.
    fn orphan(store: &Store, operation: i64) {
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .expect("the keys are off");
        store
            .connection
            .execute("UPDATE sync_queue SET show = 99 WHERE id = ?1", [operation])
            .expect("the row is changed");
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("the keys are on");
    }

    fn a_write(progress: u32) -> Write {
        Write {
            id: EntryId(12345),
            status: Status::Watching,
            progress,
            rewatches: 0,
        }
    }

    #[test]
    fn every_operation_is_pending_for_a_backend_that_has_done_none() {
        let (_home, store, operations) = a_log_of(3);

        assert_eq!(pending(&store, ANILIST), operations);
    }

    #[test]
    fn an_operation_delivered_is_pending_no_more_and_those_after_it_are() {
        let (_home, mut store, operations) = a_log_of(3);

        store
            .delivered(ANILIST, operations[0])
            .expect("the operation is done");

        assert_eq!(pending(&store, ANILIST), operations[1..]);
    }

    #[test]
    fn the_cursor_of_one_backend_moves_no_other() {
        let (_home, mut store, operations) = a_log_of(3);

        store
            .delivered(ANILIST, operations[1])
            .expect("the operation is done");
        let another = pending(&store, ANOTHER);
        store
            .delivered(ANOTHER, operations[0])
            .expect("the operation is done");

        assert_eq!(another, operations);
        assert_eq!(pending(&store, ANILIST), operations[2..]);
        assert_eq!(pending(&store, ANOTHER), operations[1..]);
    }

    #[test]
    fn a_cursor_does_not_move_back() {
        let (_home, mut store, operations) = a_log_of(4);

        for done in [operations[2], operations[0]] {
            store
                .delivered(ANILIST, done)
                .expect("the operation is done");
        }

        assert_eq!(pending(&store, ANILIST), operations[3..]);
    }

    #[test]
    fn a_write_decided_is_kept_until_its_operation_is_delivered() {
        let (_home, mut store, operations) = a_log_of(2);

        store
            .decide(ANILIST, operations[0], &a_write(1))
            .expect("the write is kept");
        let kept = store
            .decided(ANILIST, operations[0])
            .expect("the store answers");
        store
            .delivered(ANILIST, operations[0])
            .expect("the operation is done");

        assert_eq!(kept, Some(a_write(1)));
        assert_eq!(
            store
                .decided(ANILIST, operations[0])
                .expect("the store answers"),
            None
        );
    }

    #[test]
    fn a_write_decided_again_takes_the_place_of_the_first() {
        let (_home, mut store, operations) = a_log_of(1);

        for progress in [1, 2] {
            store
                .decide(ANILIST, operations[0], &a_write(progress))
                .expect("the write is kept");
        }

        assert_eq!(
            store
                .decided(ANILIST, operations[0])
                .expect("the store answers"),
            Some(a_write(2))
        );
    }

    #[test]
    fn a_write_decided_by_one_backend_is_no_other_s_and_of_no_other_operation() {
        let (_home, mut store, operations) = a_log_of(2);

        store
            .decide(ANILIST, operations[0], &a_write(1))
            .expect("the write is kept");

        for (backend, operation) in [(ANOTHER, operations[0]), (ANILIST, operations[1])] {
            assert_eq!(
                store
                    .decided(backend, operation)
                    .expect("the store answers"),
                None,
                "{backend} {operation}"
            );
        }
    }

    #[test]
    fn an_operation_set_aside_is_pending_no_more_and_is_kept_with_its_reason() {
        let (_home, mut store, operations) = a_log_of(3);

        store
            .set_aside(ANILIST, operations[0], "no entry of the show")
            .expect("the operation is set aside");

        assert_eq!(pending(&store, ANILIST), operations[1..]);
        let aside = store.aside(ANILIST).expect("the store answers");
        assert_eq!(
            aside.iter().map(read_aside).collect::<Vec<_>>(),
            [(operations[0], "no entry of the show")]
        );
        assert!(store.aside(ANOTHER).expect("the store answers").is_empty());
    }

    #[test]
    fn an_operation_set_aside_again_keeps_the_newer_reason() {
        let (_home, mut store, operations) = a_log_of(1);

        for reason in ["no entry of the show", "episode 14 past the count of 13"] {
            store
                .set_aside(ANILIST, operations[0], reason)
                .expect("the operation is set aside");
        }

        let aside = store.aside(ANILIST).expect("the store answers");
        assert_eq!(aside.len(), 1);
        assert_eq!(aside[0].reason, "episode 14 past the count of 13");
    }

    #[test]
    fn operations_set_aside_are_read_lowest_first() {
        let (_home, mut store, operations) = a_log_of(3);

        for operation in [operations[2], operations[0]] {
            store
                .set_aside(ANILIST, operation, "a reason")
                .expect("the operation is set aside");
        }

        let aside = store.aside(ANILIST).expect("the store answers");
        assert_eq!(
            aside
                .iter()
                .map(|aside| read_aside(aside).0)
                .collect::<Vec<_>>(),
            [operations[0], operations[2]]
        );
    }

    #[test]
    fn an_operation_set_aside_and_delivered_is_aside_no_more_and_moves_no_cursor_back() {
        let (_home, mut store, operations) = a_log_of(3);
        for operation in [operations[0], operations[1]] {
            store
                .set_aside(ANILIST, operation, "a reason")
                .expect("the operation is set aside");
        }

        store
            .delivered(ANILIST, operations[0])
            .expect("the operation is done");

        let aside = store.aside(ANILIST).expect("the store answers");
        assert_eq!(
            aside
                .iter()
                .map(|aside| read_aside(aside).0)
                .collect::<Vec<_>>(),
            [operations[1]]
        );
        assert_eq!(pending(&store, ANILIST), operations[2..]);
    }

    #[test]
    fn an_operation_set_aside_takes_its_decided_write_with_it() {
        let (_home, mut store, operations) = a_log_of(1);
        store
            .decide(ANILIST, operations[0], &a_write(1))
            .expect("the write is kept");

        store
            .set_aside(ANILIST, operations[0], "no work of the number")
            .expect("the operation is set aside");

        assert_eq!(
            store
                .decided(ANILIST, operations[0])
                .expect("the store answers"),
            None
        );
    }

    #[test]
    fn the_log_keeps_every_operation_whatever_was_done_with_it() {
        let (_home, mut store, operations) = a_log_of(3);

        store
            .delivered(ANILIST, operations[0])
            .expect("the operation is done");
        store
            .set_aside(ANILIST, operations[1], "a reason")
            .expect("the operation is set aside");

        assert_eq!(numbers(&store.queued().expect("the log reads")), operations);
    }

    #[test]
    fn a_number_past_the_end_of_the_log_is_refused_and_nothing_moves() {
        let (_home, mut store, operations) = a_log_of(2);
        let nowhere = operations[1] + 1;

        let refused = [
            store.delivered(ANILIST, nowhere),
            store.set_aside(ANILIST, nowhere, "a reason"),
            store.decide(ANILIST, nowhere, &a_write(1)),
        ];

        for refused in refused {
            assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        }
        assert_eq!(pending(&store, ANILIST), operations);
        assert!(store.aside(ANILIST).expect("the store answers").is_empty());
    }

    #[test]
    fn an_operation_past_the_cursor_that_cannot_be_read_is_named_in_its_place() {
        let (_home, store, operations) = a_log_of(3);
        spoil(&store, operations[1]);

        let pending = store.pending(ANILIST).expect("the log reads");

        assert_eq!(
            pending.iter().map(|pending| pending.id).collect::<Vec<_>>(),
            operations
        );
        assert!(
            matches!(
                &pending[1].operation,
                Err(Error::Operation { id, .. }) if *id == operations[1]
            ),
            "{pending:?}"
        );
        assert!(
            pending[0].operation.is_ok() && pending[2].operation.is_ok(),
            "{pending:?}"
        );
    }

    #[test]
    fn a_row_whose_kind_or_payload_is_not_text_is_answered_in_its_place() {
        let (_home, store, operations) = a_log_of(3);
        for (spoiled, operation) in [
            ("kind = CAST(x'ff' AS TEXT)", operations[0]),
            ("payload = x'ff'", operations[1]),
        ] {
            store
                .connection
                .execute(
                    &format!("UPDATE sync_queue SET {spoiled} WHERE id = ?1"),
                    [operation],
                )
                .expect("the row is changed");
        }

        let pending = store.pending(ANILIST).expect("the log reads");

        assert_eq!(
            pending.iter().map(|pending| pending.id).collect::<Vec<_>>(),
            operations
        );
        for unread in &pending[..2] {
            assert!(
                matches!(&unread.operation, Err(Error::Operation { id, .. }) if *id == unread.id),
                "{pending:?}"
            );
        }
        assert!(pending[2].operation.is_ok(), "{pending:?}");
    }

    #[test]
    fn a_row_queued_at_no_instant_is_answered_in_its_place() {
        let (_home, store, operations) = a_log_of(2);
        store
            .connection
            .execute(
                "UPDATE sync_queue SET created_at = 'yesterday' WHERE id = ?1",
                [operations[0]],
            )
            .expect("the row is changed");

        let pending = store.pending(ANILIST).expect("the log reads");

        assert!(
            matches!(
                &pending[0].operation,
                Err(Error::Operation { id, .. }) if *id == operations[0]
            ),
            "{pending:?}"
        );
        assert!(pending[1].operation.is_ok(), "{pending:?}");
    }

    #[test]
    fn text_kept_as_a_blob_is_read_as_the_text_it_holds() {
        let (_home, mut store, operations) = a_log_of(2);
        store
            .set_aside(ANOTHER, operations[1], "a reason")
            .expect("the operation is set aside");
        store
            .decide(ANILIST, operations[0], &a_write(1))
            .expect("the write is kept");
        store
            .connection
            .execute_batch(
                "UPDATE shows SET title = CAST(title AS BLOB);
                 UPDATE sync_queue SET payload = CAST(payload AS BLOB);
                 UPDATE set_aside SET reason = CAST(reason AS BLOB);
                 UPDATE decided SET write = CAST(write AS BLOB);",
            )
            .expect("the rows are changed");

        let pending = store.pending(ANILIST).expect("the log reads");

        let read = pending
            .into_iter()
            .map(|pending| pending.operation.expect("the operation reads"))
            .collect::<Vec<_>>();
        assert_eq!(
            read.iter()
                .map(|operation| (operation.id, operation.title.as_str(), &operation.kind))
                .collect::<Vec<_>>(),
            [
                (
                    operations[0],
                    "Show Title",
                    &Kind::Progress { episode: Some(1) }
                ),
                (
                    operations[1],
                    "Show Title",
                    &Kind::Progress { episode: Some(2) }
                ),
            ]
        );
        let aside = store.aside(ANOTHER).expect("the store answers");
        assert_eq!(
            aside.iter().map(read_aside).collect::<Vec<_>>(),
            [(operations[1], "a reason")]
        );
        assert_eq!(
            store
                .decided(ANILIST, operations[0])
                .expect("the store answers"),
            Some(a_write(1))
        );
    }

    #[test]
    fn an_operation_that_cannot_be_read_is_set_aside_and_read_back_with_its_reason() {
        let (_home, mut store, operations) = a_log_of(2);
        orphan(&store, operations[0]);

        store
            .set_aside(ANILIST, operations[0], "the row cannot be read")
            .expect("the operation is set aside");

        let aside = store.aside(ANILIST).expect("the store answers");
        assert_eq!(
            aside
                .iter()
                .map(|aside| (aside.id, aside.reason.as_str()))
                .collect::<Vec<_>>(),
            [(operations[0], "the row cannot be read")]
        );
        assert!(
            matches!(
                &aside[0].operation,
                Err(Error::Orphan { id }) if *id == operations[0]
            ),
            "{aside:?}"
        );
        assert_eq!(pending(&store, ANILIST), operations[1..]);
    }

    #[test]
    fn an_operation_whose_cursor_cannot_move_keeps_its_write_and_is_not_set_aside() {
        let (_home, mut store, operations) = a_log_of(1);
        store
            .decide(ANILIST, operations[0], &a_write(1))
            .expect("the write is kept");
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refused BEFORE INSERT ON cursors \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
            )
            .expect("the trigger is made");

        let refused = [
            store.delivered(ANILIST, operations[0]),
            store.set_aside(ANILIST, operations[0], "a reason"),
        ];

        for refused in refused {
            assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        }
        assert_eq!(
            store
                .decided(ANILIST, operations[0])
                .expect("the store answers"),
            Some(a_write(1))
        );
        assert!(store.aside(ANILIST).expect("the store answers").is_empty());
        assert_eq!(pending(&store, ANILIST), operations);
    }

    #[test]
    fn what_one_backend_does_with_an_operation_leaves_what_another_kept_of_it() {
        let (_home, mut store, operations) = a_log_of(2);
        store
            .decide(ANOTHER, operations[0], &a_write(1))
            .expect("the write is kept");
        store
            .set_aside(ANOTHER, operations[1], "a reason")
            .expect("the operation is set aside");

        store
            .delivered(ANILIST, operations[0])
            .expect("the operation is done");
        store
            .set_aside(ANILIST, operations[1], "another reason")
            .expect("the operation is set aside");
        store
            .delivered(ANILIST, operations[1])
            .expect("the operation is done");

        assert_eq!(
            store
                .decided(ANOTHER, operations[0])
                .expect("the store answers"),
            Some(a_write(1))
        );
        let aside = store.aside(ANOTHER).expect("the store answers");
        assert_eq!(
            aside.iter().map(read_aside).collect::<Vec<_>>(),
            [(operations[1], "a reason")]
        );
    }

    #[test]
    fn a_write_kept_that_cannot_be_read_is_refused_and_named() {
        let (_home, store, operations) = a_log_of(2);
        for (operation, kept) in [(operations[0], "'{}'"), (operations[1], "x'ff'")] {
            store
                .connection
                .execute(
                    &format!(
                        "INSERT INTO decided (backend, operation, write) VALUES (?1, ?2, {kept})"
                    ),
                    rusqlite::params![ANILIST, operation],
                )
                .expect("the row goes in");
        }

        for kept in operations {
            let refused = store.decided(ANILIST, kept);

            assert!(
                matches!(
                    &refused,
                    Err(Error::Decided { backend, operation, .. })
                        if backend == ANILIST && *operation == kept
                ),
                "got {refused:?}"
            );
        }
    }

    #[test]
    fn where_a_backend_is_in_the_log_is_still_there_after_the_store_is_opened_again() {
        let (home, mut store, operations) = a_log_of(3);
        store
            .decide(ANILIST, operations[2], &a_write(3))
            .expect("the write is kept");
        store
            .delivered(ANILIST, operations[0])
            .expect("the operation is done");
        store
            .set_aside(ANILIST, operations[1], "a reason")
            .expect("the operation is set aside");
        drop(store);

        let reopened =
            Store::open(&home.path().join("benshi.db")).expect("the database opens again");

        assert_eq!(pending(&reopened, ANILIST), operations[2..]);
        assert_eq!(reopened.aside(ANILIST).expect("the store answers").len(), 1);
        assert_eq!(
            reopened
                .decided(ANILIST, operations[2])
                .expect("the store answers"),
            Some(a_write(3))
        );
    }
}
