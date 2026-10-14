//! Recording that an episode was seen.
//!
//! **One transaction across two tables.** The row in `episodes_seen` and the
//! operation in `sync_queue` that will carry it to a list are written
//! together, and a failure anywhere between them leaves neither. A row
//! without its operation would be an episode no list ever hears of, and an
//! operation without its row would be a list told of an episode that was
//! never recorded. The transaction rules out both.
//!
//! The mark on the episode's total in `watching` is made in the same
//! transaction. Made after it, a process killed between the two would leave
//! an episode recorded and a total that does not say so, and the episode
//! would register a second time.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use benshi_core::ShowId;
use benshi_core::path::RawPath;
use humantime::format_rfc3339_seconds;
use rusqlite::params;

use crate::queue::{Kind, enqueue};
use crate::watching::registered;
use crate::{Error, Store};

/// One viewing, as it reaches [`Store::record`].
///
/// Named for what it is rather than for the table it lands in, because the
/// daemon already has a `Seen` of its own, the sources its last round saw,
/// and the two meet in one module there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewing {
    /// The show that was seen, one the store has filed.
    pub show: ShowId,
    /// The episode in the release's numbering, and none for a film.
    ///
    /// A film spells no episode and is a work this program marks watched, so
    /// it is recorded with NULL in the column. A file of several episodes and
    /// a half episode name no episode to record, and neither has anything to
    /// put here.
    pub episode: Option<u32>,
    /// The file that was open.
    pub media: RawPath,
    /// When the episode registered as watched.
    pub at: SystemTime,
}

impl Store {
    /// Record that an episode was seen, and queue the operation that will
    /// tell a list.
    ///
    /// Instants are written as RFC 3339 text with whole seconds, and the
    /// media as the path's escaped form, so that `sqlite3` shows rows a
    /// person can read. Where a total is kept for the episode, it is marked
    /// registered.
    ///
    /// # Errors
    ///
    /// [`Error::Instant`] for a clock reading before 1970 or past the year
    /// 9999, before anything is written. [`Error::Busy`] when another
    /// connection has the database, and [`Error::Write`] when any statement
    /// fails for any other reason, a show the store has not filed among
    /// them. Nothing is left behind: the transaction is rolled back with the
    /// error.
    pub fn record(&mut self, viewing: &Viewing) -> Result<(), Error> {
        let at = rfc3339(viewing.at).ok_or(Error::Instant { at: viewing.at })?;
        let seen = Kind::Progress {
            episode: viewing.episode,
        };

        let show = viewing.show.0;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO episodes_seen (show, episode, seen_at, media) \
             VALUES (?1, ?2, ?3, ?4)",
            params![show, viewing.episode, at, viewing.media.escaped()],
        )?;
        enqueue(&transaction, show, &seen, &at)?;
        registered(&transaction, show, viewing.episode)?;
        transaction.commit()?;

        Ok(())
    }
}

/// An instant as RFC 3339 text with whole seconds, or nothing where the
/// text cannot spell it.
///
/// The formatter panics on an instant before the epoch and answers a
/// formatting error for a year past 9999, which `to_string` would turn into
/// a panic as well. Both are checked here, so that a broken clock is an
/// error the caller reads rather than the end of the task.
pub(crate) fn rfc3339(at: SystemTime) -> Option<String> {
    at.duration_since(UNIX_EPOCH).ok()?;
    let mut text = String::new();
    write!(text, "{}", format_rfc3339_seconds(at)).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use benshi_core::ShowId;
    use benshi_core::path::RawPath;
    use tempfile::TempDir;

    use super::Viewing;
    use crate::{Error, Store};

    /// An instant with whole seconds, because the column keeps seconds.
    fn an_instant() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    /// A store with one show filed, and that show.
    fn a_store() -> (TempDir, Store, ShowId) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        let show = store.new_show("Show Title").expect("the show is filed");
        (home, store, show)
    }

    fn viewing(show: ShowId, episode: Option<u32>, media: &[u8]) -> Viewing {
        Viewing {
            show,
            episode,
            media: RawPath::from_bytes(media.to_vec()),
            at: an_instant(),
        }
    }

    fn count(store: &Store, table: &str) -> u32 {
        store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("the table is there")
    }

    #[test]
    fn recording_an_episode_leaves_an_episode_and_an_operation_of_its_show() {
        let (_home, mut store, filed) = a_store();

        store
            .record(&viewing(
                filed,
                Some(3),
                b"/anime/[Group] Show Title - 03.mkv",
            ))
            .expect("recording cannot fail");

        assert_eq!(count(&store, "shows"), 1, "no show is filed by a recording");
        let show = filed.0;

        let (episode_show, episode, seen_at, media): (i64, Option<u32>, String, String) = store
            .connection
            .query_row(
                "SELECT show, episode, seen_at, media FROM episodes_seen",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("one episode");
        assert_eq!(episode_show, show);
        assert_eq!(episode, Some(3));
        assert_eq!(
            humantime::parse_rfc3339(&seen_at).expect("RFC 3339, as sqlite3 shows it"),
            an_instant()
        );
        assert_eq!(media, "/anime/[Group] Show Title - 03.mkv");

        let (operation_show, kind, payload, created_at): (i64, String, String, String) = store
            .connection
            .query_row(
                "SELECT show, kind, payload, created_at FROM sync_queue",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("one operation");
        assert_eq!(operation_show, show);
        assert_eq!(kind, "progress");
        assert_eq!(payload, r#"{"episode":3}"#);
        assert_eq!(
            humantime::parse_rfc3339(&created_at).expect("RFC 3339"),
            an_instant()
        );
    }

    /// The show each row of a table points at, in the order of the rows.
    fn shows_of(store: &Store, table: &str) -> Vec<i64> {
        store
            .connection
            .prepare(&format!("SELECT show FROM {table} ORDER BY id"))
            .expect("the table is there")
            .query_map([], |row| row.get(0))
            .expect("the rows are read")
            .collect::<Result<_, _>>()
            .expect("every row is read")
    }

    #[test]
    fn two_shows_of_one_title_are_recorded_apart() {
        let (_home, mut store, one) = a_store();
        let other = store.new_show("Show Title").expect("the show is filed");

        for show in [one, other] {
            store
                .record(&viewing(show, Some(3), b"Show Title - 03.mkv"))
                .expect("recording cannot fail");
        }

        assert_eq!(shows_of(&store, "episodes_seen"), [one.0, other.0]);
        assert_eq!(shows_of(&store, "sync_queue"), [one.0, other.0]);
    }

    #[test]
    fn an_episode_of_a_show_that_is_not_filed_is_refused() {
        let (_home, mut store, _show) = a_store();

        let refused = store.record(&viewing(ShowId(42), Some(3), b"Show Title - 03.mkv"));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
    }

    #[test]
    fn a_film_is_recorded_with_no_episode() {
        // A film spells no episode and is a work this program marks watched.
        // NULL, and never a number standing in for one.
        let (_home, mut store, show) = a_store();

        store
            .record(&viewing(show, None, b"A Film (2019).mkv"))
            .expect("recording cannot fail");

        let episode: Option<u32> = store
            .connection
            .query_row("SELECT episode FROM episodes_seen", [], |row| row.get(0))
            .expect("one episode");
        assert_eq!(episode, None);
        let payload: String = store
            .connection
            .query_row("SELECT payload FROM sync_queue", [], |row| row.get(0))
            .expect("one operation");
        assert_eq!(payload, r#"{"episode":null}"#);
    }

    #[test]
    fn an_operation_that_cannot_be_written_takes_the_episode_with_it() {
        // The one transaction across two tables, from the inside: a TEMP
        // trigger, which lives on this connection alone, refuses the
        // operation. RAISE(ABORT) backs out the statement that fired it and
        // leaves the transaction open, so what takes the episode with it is
        // the rollback as the transaction is dropped. A row without its
        // operation is the thing this store exists to make impossible.
        let (_home, mut store, show) = a_store();
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_operations BEFORE INSERT ON sync_queue \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END",
            )
            .expect("a trigger on this connection");

        let refused = store.record(&viewing(show, Some(3), b"Show Title - 03.mkv"));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        assert_eq!(count(&store, "episodes_seen"), 0);
        assert_eq!(count(&store, "sync_queue"), 0);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_recorded_readably() {
        // The same escaping a trace uses: valid UTF-8 verbatim, every other
        // byte as %XX, so `sqlite3` shows a name a person can read and the
        // bytes can still be recovered.
        let (_home, mut store, show) = a_store();

        store
            .record(&viewing(show, Some(3), b"/anime/\xFF\xFE - 03.mkv"))
            .expect("recording cannot fail");

        let media: String = store
            .connection
            .query_row("SELECT media FROM episodes_seen", [], |row| row.get(0))
            .expect("one episode");
        assert_eq!(media, "/anime/%FF%FE - 03.mkv");
    }

    #[test]
    fn an_instant_the_column_cannot_hold_is_refused_rather_than_a_panic() {
        // RFC 3339 has no year before 1970 in this formatter and no year past
        // 9999 at all. A clock set to either is a broken clock, and a broken
        // clock is a reason to say so, not a reason to bring the daemon down
        // inside a write.
        let (_home, mut store, show) = a_store();
        let before_the_epoch = UNIX_EPOCH - Duration::from_secs(1);
        // The first second of the year 10000, in hours since the epoch.
        let the_year_ten_thousand = UNIX_EPOCH + Duration::from_hours(70_389_528);

        for (at, said) in [
            (
                before_the_epoch,
                "clock reads Unix time -1, which is before 1970",
            ),
            (
                the_year_ten_thousand,
                "clock reads Unix time 253402300800, which is after 9999",
            ),
        ] {
            let refused = store.record(&Viewing {
                at,
                ..viewing(show, Some(3), b"Show Title - 03.mkv")
            });

            assert!(
                matches!(refused, Err(Error::Instant { at: refused_at }) if refused_at == at),
                "got {refused:?}"
            );
            assert_eq!(refused.expect_err("refused").to_string(), said);
        }
        assert_eq!(count(&store, "episodes_seen"), 0);
        assert_eq!(count(&store, "sync_queue"), 0);
    }
}
