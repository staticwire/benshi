//! What has been watched of an episode, across sittings.
//!
//! A [`Timeline`](benshi_core::timeline::Timeline) counts watched time in
//! memory, and counts it for the one file that is open: a different file
//! starts the count again, and a process that stops takes the count with it.
//! Two fifths of an episode now and two fifths another day are four fifths of
//! it, so the total is kept here, one row of `watching` for each episode, and
//! [`Store::resume`] hands it back.
//!
//! **A total outlives the sitting until the episode registers, and the
//! sitting it registers in until that sitting closes.** Dropping it at the
//! moment of registration would put the rest of that sitting into a fresh
//! total with no mark on it, and that total would count towards registering
//! the episode a second time. A process that is killed closes nothing, so
//! what it kept is there for the next one.

use std::time::{Duration, SystemTime};

use benshi_core::path::RawPath;
use rusqlite::{Connection, OptionalExtension, params};

use crate::record::rfc3339;
use crate::{Error, Store, filed};

/// What has been watched of one episode, as it reaches [`Store::keep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Total {
    /// The title as the corpus spells it.
    pub title: String,
    /// The episode in the release's numbering, and none for a film.
    pub episode: Option<u32>,
    /// The file that is open.
    pub media: RawPath,
    /// Everything watched of the episode, this sitting and the ones before.
    pub watched: Duration,
    /// When the total was read.
    pub at: SystemTime,
}

/// What the store kept for one episode, as [`Store::resume`] answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// The file that was open when the total was last kept.
    pub media: RawPath,
    /// Everything watched of the episode, in whole milliseconds.
    pub watched: Duration,
    /// Whether the episode registered in the sitting the total belongs to.
    pub registered: bool,
}

impl Store {
    /// Keep the total watched of an episode.
    ///
    /// The total takes the place of the one kept before, with its media and
    /// its instant. Whether the episode registered stays as it was. The show
    /// is filed if it is not, because a total is kept before any episode of
    /// the show registers.
    ///
    /// The column holds whole milliseconds. What is under one is dropped,
    /// and a total longer than the column holds is kept as the longest it
    /// holds.
    ///
    /// # Errors
    ///
    /// [`Error::Instant`] for a clock reading before 1970 or past the year
    /// 9999, before anything is written. [`Error::Busy`] when another
    /// connection has the database, and [`Error::Write`] when a statement
    /// fails for any other reason. The transaction is rolled back with
    /// either.
    pub fn keep(&mut self, total: &Total) -> Result<(), Error> {
        let at = rfc3339(total.at).ok_or(Error::Instant { at: total.at })?;
        let media = total.media.escaped();
        let watched = i64::try_from(total.watched.as_millis()).unwrap_or(i64::MAX);

        let transaction = self.connection.transaction()?;
        let show = filed(&transaction, &total.title)?;
        let kept = transaction.execute(
            "UPDATE watching SET media = ?3, watched_ms = ?4, updated_at = ?5 \
             WHERE show = ?1 AND episode IS ?2",
            params![show, total.episode, media, watched, at],
        )?;
        if kept == 0 {
            transaction.execute(
                "INSERT INTO watching (show, episode, media, watched_ms, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![show, total.episode, media, watched, at],
            )?;
        }
        transaction.commit()?;

        Ok(())
    }

    /// What was kept for an episode, or nothing where none was.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when the database cannot be read because another
    /// connection has it, which in WAL a connection that writes to it does
    /// not cause. [`Error::Write`] when the row cannot be read for any other
    /// reason, a total below nought included. [`Error::Media`] for a row
    /// whose media is not a path as this store writes one.
    pub fn resume(&self, title: &str, episode: Option<u32>) -> Result<Option<Kept>, Error> {
        let row = self
            .connection
            .query_row(
                "SELECT watching.id, media, watched_ms, registered \
                 FROM watching JOIN shows ON shows.id = watching.show \
                 WHERE shows.title = ?1 AND episode IS ?2",
                params![title, episode],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, text, watched, registered)) = row else {
            return Ok(None);
        };
        let Some(media) = RawPath::from_escaped(&text) else {
            return Err(Error::Media { id, text });
        };
        let watched = u64::try_from(watched)
            .map_err(|_| Error::Write(rusqlite::Error::IntegralValueOutOfRange(2, watched)))?;

        Ok(Some(Kept {
            media,
            watched: Duration::from_millis(watched),
            registered,
        }))
    }

    /// Close an episode: drop its total where the episode registered, and
    /// keep it where it did not.
    ///
    /// An episode with no total kept is closed by doing nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] when another connection has the database, and
    /// [`Error::Write`] when the statement fails for any other reason.
    pub fn close(&self, title: &str, episode: Option<u32>) -> Result<(), Error> {
        self.connection.execute(
            "DELETE FROM watching \
             WHERE registered = 1 AND episode IS ?2 \
             AND show = (SELECT id FROM shows WHERE title = ?1)",
            params![title, episode],
        )?;
        Ok(())
    }
}

/// Mark the total of an episode registered, where one is kept.
///
/// An episode with no total is left without one. A row of `watching` is a
/// sitting that somebody closes, and an episode recorded outside a sitting
/// has nobody to close it.
pub(crate) fn registered(
    connection: &Connection,
    show: i64,
    episode: Option<u32>,
) -> Result<(), Error> {
    connection.execute(
        "UPDATE watching SET registered = 1 WHERE show = ?1 AND episode IS ?2",
        params![show, episode],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use benshi_core::path::RawPath;
    use tempfile::TempDir;

    use super::{Kept, Total};
    use crate::{Error, Store, Viewing};

    /// Two fifths of an episode of twenty-four minutes, less a little.
    const A_SITTING: Duration = Duration::from_millis(561_600);
    /// Both sittings together.
    const TWO_SITTINGS: Duration = Duration::from_millis(1_123_200);

    const MEDIA: &[u8] = b"/anime/[Group] Show Title - 03.mkv";

    fn an_instant() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    fn a_store() -> (TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        (home, store)
    }

    fn total(title: &str, episode: Option<u32>, watched: Duration) -> Total {
        Total {
            title: title.to_owned(),
            episode,
            media: RawPath::from_bytes(MEDIA.to_vec()),
            watched,
            at: an_instant(),
        }
    }

    fn viewing(title: &str, episode: Option<u32>) -> Viewing {
        Viewing {
            title: title.to_owned(),
            episode,
            media: RawPath::from_bytes(MEDIA.to_vec()),
            at: an_instant(),
        }
    }

    fn kept(watched: Duration, registered: bool) -> Kept {
        Kept {
            media: RawPath::from_bytes(MEDIA.to_vec()),
            watched,
            registered,
        }
    }

    fn rows(store: &Store) -> u32 {
        store
            .connection
            .query_row("SELECT count(*) FROM watching", [], |row| row.get(0))
            .expect("the table is there")
    }

    #[test]
    fn an_episode_never_opened_has_nothing_to_resume() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");

        let another_episode = store.resume("Show Title", Some(4));
        let another_show = store.resume("Another Show", Some(3));
        let the_film = store.resume("Show Title", None);

        assert_eq!(rows(&store), 1, "the one that was opened is there");
        assert_eq!(another_episode.expect("the store answers"), None);
        assert_eq!(another_show.expect("the store answers"), None);
        assert_eq!(the_film.expect("the store answers"), None);
    }

    #[test]
    fn a_total_kept_is_the_total_resumed() {
        let (_home, mut store) = a_store();

        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(A_SITTING, false))
        );
    }

    #[test]
    fn a_total_is_a_row_a_person_can_read() {
        let (_home, mut store) = a_store();

        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");

        let (title, episode, media, watched_ms, registered, updated_at): (
            String,
            Option<u32>,
            String,
            i64,
            i64,
            String,
        ) = store
            .connection
            .query_row(
                "SELECT shows.title, episode, media, watched_ms, registered, updated_at \
                 FROM watching JOIN shows ON shows.id = watching.show",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("one row");
        assert_eq!(title, "Show Title");
        assert_eq!(episode, Some(3));
        assert_eq!(media, "/anime/[Group] Show Title - 03.mkv");
        assert_eq!(watched_ms, 561_600);
        assert_eq!(registered, 0);
        assert_eq!(
            humantime::parse_rfc3339(&updated_at).expect("RFC 3339, as sqlite3 shows it"),
            an_instant()
        );
    }

    #[test]
    fn a_later_total_takes_the_place_of_the_earlier_one() {
        // The sittings are added up by whoever runs the timeline. What
        // arrives here is the sum, and it is kept as it arrives.
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");

        store
            .keep(&Total {
                media: RawPath::from_bytes(b"/anime/[Other] Show Title - 03.mkv".to_vec()),
                at: an_instant() + Duration::from_secs(3600),
                ..total("Show Title", Some(3), TWO_SITTINGS)
            })
            .expect("the total is kept");

        assert_eq!(rows(&store), 1);
        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(Kept {
                media: RawPath::from_bytes(b"/anime/[Other] Show Title - 03.mkv".to_vec()),
                watched: TWO_SITTINGS,
                registered: false,
            })
        );
        let updated_at: String = store
            .connection
            .query_row("SELECT updated_at FROM watching", [], |row| row.get(0))
            .expect("one row");
        assert_eq!(
            humantime::parse_rfc3339(&updated_at).expect("RFC 3339"),
            an_instant() + Duration::from_secs(3600)
        );
    }

    #[test]
    fn a_film_keeps_one_row_however_often_its_total_is_kept() {
        // A film has no episode, and NULL is equal to nothing in SQL, itself
        // included. A row looked for with `=` is never found, and every
        // total kept would be one more row.
        let (_home, mut store) = a_store();

        store
            .keep(&total("A Film", None, A_SITTING))
            .expect("the total is kept");
        store
            .keep(&total("A Film", None, TWO_SITTINGS))
            .expect("the total is kept");

        assert_eq!(rows(&store), 1);
        assert_eq!(
            store.resume("A Film", None).expect("the store answers"),
            Some(kept(TWO_SITTINGS, false))
        );
    }

    #[test]
    fn every_episode_has_a_total_of_its_own() {
        let (_home, mut store) = a_store();

        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");
        store
            .keep(&total("Show Title", Some(4), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .keep(&total(
                "Another Show",
                Some(3),
                Duration::from_millis(5_000),
            ))
            .expect("the total is kept");

        assert_eq!(rows(&store), 3);
        let shows: u32 = store
            .connection
            .query_row("SELECT count(*) FROM shows", [], |row| row.get(0))
            .expect("the table is there");
        assert_eq!(shows, 2, "two episodes of one show are one show");
        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(A_SITTING, false))
        );
        assert_eq!(
            store
                .resume("Show Title", Some(4))
                .expect("the store answers"),
            Some(kept(TWO_SITTINGS, false))
        );
        assert_eq!(
            store
                .resume("Another Show", Some(3))
                .expect("the store answers"),
            Some(kept(Duration::from_millis(5_000), false))
        );
    }

    #[test]
    fn recording_an_episode_marks_its_total_registered() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .keep(&total("Show Title", Some(4), A_SITTING))
            .expect("the total is kept");
        store
            .keep(&total("Another Show", Some(3), A_SITTING))
            .expect("the total is kept");

        store
            .record(&viewing("Show Title", Some(3)))
            .expect("recording cannot fail");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(TWO_SITTINGS, true))
        );
        assert_eq!(
            store
                .resume("Show Title", Some(4))
                .expect("the store answers"),
            Some(kept(A_SITTING, false)),
            "another episode of the show did not register"
        );
        assert_eq!(
            store
                .resume("Another Show", Some(3))
                .expect("the store answers"),
            Some(kept(A_SITTING, false)),
            "the same episode of another show did not register"
        );
    }

    #[test]
    fn recording_a_film_marks_its_total_registered() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("A Film", None, TWO_SITTINGS))
            .expect("the total is kept");

        store
            .record(&viewing("A Film", None))
            .expect("recording cannot fail");

        assert_eq!(
            store.resume("A Film", None).expect("the store answers"),
            Some(kept(TWO_SITTINGS, true))
        );
    }

    #[test]
    fn recording_an_episode_nobody_is_watching_leaves_no_total() {
        // A row here is a sitting somebody will close. An episode recorded
        // outside one has none, and a row made for it would stay for ever.
        let (_home, mut store) = a_store();

        store
            .record(&viewing("Show Title", Some(3)))
            .expect("recording cannot fail");

        assert_eq!(rows(&store), 0);
        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            None
        );
    }

    #[test]
    fn an_operation_that_cannot_be_written_leaves_the_total_unregistered() {
        // The mark is part of the recording's transaction: a recording that
        // did not happen must not leave an episode that looks registered.
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_operations BEFORE INSERT ON sync_queue \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END",
            )
            .expect("a trigger on this connection");

        let refused = store.record(&viewing("Show Title", Some(3)));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(TWO_SITTINGS, false))
        );
    }

    #[test]
    fn a_mark_that_cannot_be_made_takes_the_recording_with_it() {
        // The other direction. An episode recorded beside a total that does
        // not say so would register a second time, so the recording does
        // not outlive the mark.
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_marks BEFORE UPDATE OF registered ON watching \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END",
            )
            .expect("a trigger on this connection");

        let refused = store.record(&viewing("Show Title", Some(3)));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        let (episodes, operations): (u32, u32) = store
            .connection
            .query_row(
                "SELECT (SELECT count(*) FROM episodes_seen), \
                        (SELECT count(*) FROM sync_queue)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("the tables are there");
        assert_eq!(episodes, 0);
        assert_eq!(operations, 0);
    }

    #[test]
    fn a_total_kept_after_the_episode_registered_stays_registered() {
        // The viewer goes on watching after the episode registers, and the
        // total goes on growing.
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");
        store
            .record(&viewing("Show Title", Some(3)))
            .expect("recording cannot fail");

        store
            .keep(&total("Show Title", Some(3), TWO_SITTINGS))
            .expect("the total is kept");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(TWO_SITTINGS, true))
        );
    }

    #[test]
    fn closing_an_episode_that_registered_drops_its_total() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .record(&viewing("Show Title", Some(3)))
            .expect("recording cannot fail");
        assert_eq!(rows(&store), 1, "there is a total to drop");

        store
            .close("Show Title", Some(3))
            .expect("the store closes");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            None
        );
        assert_eq!(rows(&store), 0);
    }

    #[test]
    fn closing_an_episode_that_did_not_register_keeps_its_total() {
        // Two fifths now and two fifths another day are four fifths, and
        // they are only if the first two survive the file being closed.
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");

        store
            .close("Show Title", Some(3))
            .expect("the store closes");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(A_SITTING, false))
        );
    }

    #[test]
    fn closing_an_episode_closes_that_episode_alone() {
        let (_home, mut store) = a_store();
        for (title, episode) in [
            ("Show Title", Some(3)),
            ("Show Title", Some(4)),
            ("Show Title", None),
            ("Another Show", Some(3)),
        ] {
            store
                .keep(&total(title, episode, TWO_SITTINGS))
                .expect("the total is kept");
            store
                .record(&viewing(title, episode))
                .expect("recording cannot fail");
        }

        store
            .close("Show Title", Some(3))
            .expect("the store closes");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            None
        );
        for (title, episode) in [
            ("Show Title", Some(4)),
            ("Show Title", None),
            ("Another Show", Some(3)),
        ] {
            assert_eq!(
                store.resume(title, episode).expect("the store answers"),
                Some(kept(TWO_SITTINGS, true)),
                "{title} {episode:?} was closed with it"
            );
        }
    }

    #[test]
    fn closing_a_film_that_registered_drops_its_total() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("A Film", None, TWO_SITTINGS))
            .expect("the total is kept");
        store
            .record(&viewing("A Film", None))
            .expect("recording cannot fail");
        assert_eq!(rows(&store), 1, "there is a total to drop");

        store.close("A Film", None).expect("the store closes");

        assert_eq!(
            store.resume("A Film", None).expect("the store answers"),
            None
        );
    }

    #[test]
    fn an_episode_opened_again_after_it_closed_begins_from_nothing() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .record(&viewing("Show Title", Some(3)))
            .expect("recording cannot fail");
        store
            .close("Show Title", Some(3))
            .expect("the store closes");

        store
            .keep(&total("Show Title", Some(3), Duration::from_millis(5_000)))
            .expect("the total is kept");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(Duration::from_millis(5_000), false))
        );
    }

    #[test]
    fn closing_an_episode_never_opened_is_nothing() {
        let (_home, store) = a_store();

        store
            .close("Show Title", Some(3))
            .expect("the store closes");

        assert_eq!(rows(&store), 0);
    }

    #[test]
    fn a_total_is_kept_in_whole_milliseconds() {
        let (_home, mut store) = a_store();

        store
            .keep(&total(
                "Show Title",
                Some(3),
                Duration::from_nanos(1_999_999_999),
            ))
            .expect("the total is kept");

        assert_eq!(
            store
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(Duration::from_millis(1_999), false))
        );
    }

    #[test]
    fn a_total_longer_than_the_column_holds_is_kept_as_the_longest_it_holds() {
        let (_home, mut store) = a_store();

        store
            .keep(&total("Show Title", Some(3), Duration::MAX))
            .expect("the total is kept");

        let watched_ms: i64 = store
            .connection
            .query_row("SELECT watched_ms FROM watching", [], |row| row.get(0))
            .expect("one row");
        assert_eq!(watched_ms, i64::MAX);
    }

    #[test]
    fn a_name_that_is_not_utf8_comes_back_as_the_bytes_it_was() {
        let (_home, mut store) = a_store();
        let shift_jis = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e 100% - 03.mkv";

        store
            .keep(&Total {
                media: RawPath::from_bytes(shift_jis.to_vec()),
                ..total("Show Title", Some(3), A_SITTING)
            })
            .expect("the total is kept");

        let resumed = store
            .resume("Show Title", Some(3))
            .expect("the store answers")
            .expect("the total is there");
        assert_eq!(resumed.media.as_bytes(), shift_jis);
    }

    #[test]
    fn media_that_is_not_a_path_as_the_store_writes_one_is_refused_and_the_row_is_named() {
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");
        store
            .connection
            .execute("UPDATE watching SET media = '/anime/100% - 03.mkv'", [])
            .expect("the row is edited by hand");

        let refused = store.resume("Show Title", Some(3));

        assert!(
            matches!(
                &refused,
                Err(Error::Media { id: 1, text }) if text == "/anime/100% - 03.mkv"
            ),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_total_below_nought_is_refused() {
        // Only a hand writes one, and neither nought nor its size is a
        // total anybody watched.
        let (_home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");
        store
            .connection
            .execute("UPDATE watching SET watched_ms = -5", [])
            .expect("the row is edited by hand");

        let refused = store.resume("Show Title", Some(3));

        assert!(
            matches!(
                refused,
                Err(Error::Write(rusqlite::Error::IntegralValueOutOfRange(
                    _,
                    -5
                )))
            ),
            "got {refused:?}"
        );
    }

    #[test]
    fn an_instant_the_column_cannot_hold_is_refused_and_nothing_is_kept() {
        let (_home, mut store) = a_store();

        let refused = store.keep(&Total {
            at: UNIX_EPOCH - Duration::from_secs(1),
            ..total("Show Title", Some(3), A_SITTING)
        });

        assert!(
            matches!(refused, Err(Error::Instant { at }) if at == UNIX_EPOCH - Duration::from_secs(1)),
            "got {refused:?}"
        );
        assert_eq!(rows(&store), 0);
        let shows: u32 = store
            .connection
            .query_row("SELECT count(*) FROM shows", [], |row| row.get(0))
            .expect("the table is there");
        assert_eq!(shows, 0);
    }

    #[test]
    fn a_total_is_still_there_after_the_store_is_opened_again() {
        let (home, mut store) = a_store();
        store
            .keep(&total("Show Title", Some(3), A_SITTING))
            .expect("the total is kept");
        store
            .keep(&total("Show Title", Some(4), TWO_SITTINGS))
            .expect("the total is kept");
        store
            .record(&viewing("Show Title", Some(4)))
            .expect("recording cannot fail");
        drop(store);

        let reopened =
            Store::open(&home.path().join("benshi.db")).expect("the database opens again");

        assert_eq!(
            reopened
                .resume("Show Title", Some(3))
                .expect("the store answers"),
            Some(kept(A_SITTING, false))
        );
        assert_eq!(
            reopened
                .resume("Show Title", Some(4))
                .expect("the store answers"),
            Some(kept(TWO_SITTINGS, true)),
            "and so is the mark on the one that registered"
        );
    }

    #[test]
    fn a_total_that_cannot_be_kept_leaves_no_show_behind() {
        // The show is filed before the total is written, so the two are one
        // transaction: a show with nothing pointing at it is a title the
        // store never heard anything about.
        let (_home, mut store) = a_store();
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_totals BEFORE INSERT ON watching \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END",
            )
            .expect("a trigger on this connection");

        let refused = store.keep(&total("Show Title", Some(3), A_SITTING));

        assert!(matches!(refused, Err(Error::Write(_))), "got {refused:?}");
        let shows: u32 = store
            .connection
            .query_row("SELECT count(*) FROM shows", [], |row| row.get(0))
            .expect("the table is there");
        assert_eq!(shows, 0);
        assert_eq!(rows(&store), 0);
    }
}
