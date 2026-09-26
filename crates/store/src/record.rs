//! Recording that an episode was seen.
//!
//! **One transaction across two tables.** The row in `episodes_seen` and the
//! operation in `sync_queue` that will carry it to a list are written
//! together, and so is the row in `shows` that both point at; a failure
//! anywhere between them leaves none of the three. A row without its
//! operation would be an episode no list ever hears of, and an operation
//! without its row would be a list told of an episode that was never
//! recorded. The transaction rules out both.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use benshi_core::path::RawPath;
use humantime::format_rfc3339_seconds;
use rusqlite::params;
use serde_json::json;

use crate::{Error, Store};

/// One viewing, as it reaches [`Store::record`].
///
/// Named for what it is rather than for the table it lands in, because the
/// daemon already has a `Seen` of its own, the sources its last round saw,
/// and the two meet in one module there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewing {
    /// The title as the corpus spells it.
    pub title: String,
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
    /// The show is filed under its title the first time an episode of it is
    /// recorded. Instants are written as RFC 3339 text with whole seconds,
    /// and the media as the path's escaped form, so that `sqlite3` shows
    /// rows a person can read.
    ///
    /// # Errors
    ///
    /// [`Error::Instant`] for a clock reading before 1970 or past the year
    /// 9999, before anything is written. [`Error::Write`] when any statement
    /// fails. Nothing is left behind: the transaction is rolled back with
    /// the error.
    pub fn record(&mut self, viewing: &Viewing) -> Result<(), Error> {
        let at = rfc3339(viewing.at).ok_or(Error::Instant { at: viewing.at })?;
        let payload = json!({ "episode": viewing.episode }).to_string();

        let transaction = self.connection.transaction().map_err(Error::Write)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO shows (title) VALUES (?1)",
                [&viewing.title],
            )
            .map_err(Error::Write)?;
        let show: i64 = transaction
            .query_row(
                "SELECT id FROM shows WHERE title = ?1",
                [&viewing.title],
                |row| row.get(0),
            )
            .map_err(Error::Write)?;
        transaction
            .execute(
                "INSERT INTO episodes_seen (show, episode, seen_at, media) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![show, viewing.episode, at, viewing.media.escaped()],
            )
            .map_err(Error::Write)?;
        transaction
            .execute(
                "INSERT INTO sync_queue (show, kind, payload, created_at) \
                 VALUES (?1, 'progress', ?2, ?3)",
                params![show, payload, at],
            )
            .map_err(Error::Write)?;
        transaction.commit().map_err(Error::Write)
    }
}

/// An instant as RFC 3339 text with whole seconds, or nothing where the
/// text cannot spell it.
///
/// The formatter panics on an instant before the epoch and answers a
/// formatting error for a year past 9999, which `to_string` would turn into
/// a panic as well. Both are checked here, so that a broken clock is an
/// error the caller reads rather than the end of the task.
fn rfc3339(at: SystemTime) -> Option<String> {
    at.duration_since(UNIX_EPOCH).ok()?;
    let mut text = String::new();
    write!(text, "{}", format_rfc3339_seconds(at)).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use benshi_core::path::RawPath;
    use tempfile::TempDir;

    use super::Viewing;
    use crate::{Error, Store};

    /// An instant with whole seconds, because the column keeps seconds.
    fn an_instant() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    fn a_store() -> (TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        (home, store)
    }

    fn viewing(title: &str, episode: Option<u32>, media: &[u8]) -> Viewing {
        Viewing {
            title: title.to_owned(),
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
    fn recording_an_episode_leaves_a_show_an_episode_and_an_operation() {
        let (_home, mut store) = a_store();

        store
            .record(&viewing(
                "Show Title",
                Some(3),
                b"/anime/[Group] Show Title - 03.mkv",
            ))
            .expect("recording cannot fail");

        let (show, title): (i64, String) = store
            .connection
            .query_row("SELECT id, title FROM shows", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("one show");
        assert_eq!(title, "Show Title");

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

    #[test]
    fn a_second_episode_of_the_same_show_adds_no_second_show() {
        let (_home, mut store) = a_store();

        store
            .record(&viewing("Show Title", Some(3), b"Show Title - 03.mkv"))
            .expect("recording cannot fail");
        store
            .record(&viewing("Show Title", Some(4), b"Show Title - 04.mkv"))
            .expect("recording cannot fail");

        assert_eq!(count(&store, "shows"), 1);
        assert_eq!(count(&store, "episodes_seen"), 2);
        assert_eq!(count(&store, "sync_queue"), 2);
        let shows: u32 = store
            .connection
            .query_row(
                "SELECT count(DISTINCT show) FROM episodes_seen",
                [],
                |row| row.get(0),
            )
            .expect("the column is there");
        assert_eq!(shows, 1, "both episodes point at the one show");
    }

    #[test]
    fn a_film_is_recorded_with_no_episode() {
        // A film spells no episode and is a work this program marks watched.
        // NULL, and never a number standing in for one.
        let (_home, mut store) = a_store();

        store
            .record(&viewing("A Film", None, b"A Film (2019).mkv"))
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
        // leaves the transaction open, so what takes the show and the episode
        // with it is the rollback as the transaction is dropped. A row
        // without its operation is the thing this store exists to make
        // impossible.
        let (_home, mut store) = a_store();
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_operations BEFORE INSERT ON sync_queue \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END",
            )
            .expect("a trigger on this connection");

        let refused = store.record(&viewing("Show Title", Some(3), b"Show Title - 03.mkv"));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        assert_eq!(count(&store, "episodes_seen"), 0);
        assert_eq!(count(&store, "shows"), 0);
        assert_eq!(count(&store, "sync_queue"), 0);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_recorded_readably() {
        // The same escaping a trace uses: valid UTF-8 verbatim, every other
        // byte as %XX, so `sqlite3` shows a name a person can read and the
        // bytes can still be recovered.
        let (_home, mut store) = a_store();

        store
            .record(&viewing("Show Title", Some(3), b"/anime/\xFF\xFE - 03.mkv"))
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
        let (_home, mut store) = a_store();
        let before_the_epoch = UNIX_EPOCH - Duration::from_secs(1);
        // The first second of the year 10000, in hours since the epoch.
        let the_year_ten_thousand = UNIX_EPOCH + Duration::from_hours(70_389_528);

        for at in [before_the_epoch, the_year_ten_thousand] {
            let refused = store.record(&Viewing {
                at,
                ..viewing("Show Title", Some(3), b"Show Title - 03.mkv")
            });

            assert!(
                matches!(refused, Err(Error::Instant { at: refused_at }) if refused_at == at),
                "got {refused:?}"
            );
        }
        assert_eq!(count(&store, "episodes_seen"), 0);
        assert_eq!(count(&store, "sync_queue"), 0);
    }
}
