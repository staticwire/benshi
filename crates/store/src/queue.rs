//! The operations waiting to be delivered to a list.
//!
//! An operation is one row of `sync_queue`: the show it is about, its kind,
//! a payload whose shape the kind decides, and the instant it was queued. The
//! kind is kept by name and the payload as JSON, so `sqlite3` shows
//! `progress` beside `{"episode":3}`.
//!
//! **An operation names no backend.** It names the show by the local row and
//! carries no identifier of any service.
//!
//! **Every kind carries a value.** A progress carries the episode that was
//! seen, a rewatch the number of rewatches, an entry each of its fields. None
//! carries a step from what a list holds now, such as one more episode.
//! Writing what an operation carries leaves the same entry however many times
//! it is written, which is what allows a delivery to be repeated when nobody
//! can tell whether the first one arrived.

use rusqlite::{Connection, params};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::{Error, Store};

/// What an operation tells a list.
///
/// Kept in two columns: the name of the variant in `kind` and its fields as
/// JSON in `payload`. The name decides how the payload is read, and a payload
/// is read whole or refused. A name this build does not know, a field the
/// kind does not have, a field it has that is missing and a field spelt twice
/// are each an [`Error::Operation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Kind {
    /// An episode was seen.
    Progress {
        /// The episode in the release's numbering, and none for a film.
        ///
        /// A film's payload spells `null`. A payload with no `episode` in it
        /// is refused rather than read as a film, because a film's progress
        /// says a whole work was seen.
        #[serde(deserialize_with = "Option::deserialize")]
        episode: Option<u32>,
    },
    /// The show was seen again.
    Rewatch {
        /// How many times it has been seen again, this time included.
        count: u32,
    },
    /// A whole entry: its status, progress, score and rewatches at once.
    Entry {
        /// Where the show stands.
        status: Status,
        /// How many episodes the entry counts as seen.
        progress: u32,
        /// The score, and none where the entry has none.
        ///
        /// An entry with no score spells `null`, and a payload with no
        /// `score` in it is refused, as a progress with no episode is.
        #[serde(deserialize_with = "Option::deserialize")]
        score: Option<Rating>,
        /// How many times the show has been seen again.
        rewatches: u32,
    },
}

/// Where a show stands on a list.
///
/// `Rewatching` is a status here because it is one on AniList and on
/// Shikimori. MyAnimeList and Kitsu keep it as a flag beside the status. The
/// other five are on all four lists, which name them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Meant to be watched and not begun.
    Planned,
    /// Being watched.
    Watching,
    /// Being watched again.
    Rewatching,
    /// Watched to the end.
    Completed,
    /// Set aside, to be taken up again.
    OnHold,
    /// Given up.
    Dropped,
}

/// A score in whole points, from 1 to 100.
///
/// On AniList each user chooses the format a score is shown in, and the list
/// takes and answers a score in a hundred points whichever was chosen.
/// MyAnimeList and Shikimori score out of ten and Kitsu out of twenty, and
/// both multiply into a hundred exactly. Nought is not a rating: an entry
/// with no score carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rating(u8);

impl Rating {
    /// A rating, or nothing where the points are not from 1 to 100.
    #[must_use]
    pub fn new(points: u8) -> Option<Self> {
        (1..=100).contains(&points).then_some(Self(points))
    }

    /// The points, from 1 to 100.
    #[must_use]
    pub const fn points(self) -> u8 {
        self.0
    }
}

/// Read through [`Rating::new`], so that a score out of range in a payload is
/// refused where a derived implementation would let it in.
impl<'de> Deserialize<'de> for Rating {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let points = u8::deserialize(deserializer)?;
        Self::new(points).ok_or_else(|| {
            D::Error::custom(format_args!(
                "a rating is from 1 to 100, and this is {points}"
            ))
        })
    }
}

/// One operation as it waits in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    /// The row's `id` in `sync_queue`.
    pub id: i64,
    /// The show it is about, by the title the corpus spells.
    pub title: String,
    /// What a list is to be told.
    pub kind: Kind,
}

impl Store {
    /// Every operation in the queue, lowest `id` first.
    ///
    /// Reading takes nothing out of the queue.
    ///
    /// # Errors
    ///
    /// [`Error::Write`] when the queue cannot be read. [`Error::Orphan`] for
    /// a row whose show is not there and [`Error::Operation`] for a row that
    /// cannot be read as an operation, each naming the row: the reading
    /// fails whole rather than answering without it.
    pub fn queued(&self) -> Result<Vec<Operation>, Error> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT sync_queue.id, shows.title, sync_queue.kind, sync_queue.payload \
                 FROM sync_queue LEFT JOIN shows ON shows.id = sync_queue.show \
                 ORDER BY sync_queue.id",
            )
            .map_err(Error::Write)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(Error::Write)?;

        rows.map(|row| {
            let (id, title, kind, payload) = row.map_err(Error::Write)?;
            let title = title.ok_or(Error::Orphan { id })?;
            let kind = Kind::from_columns(&kind, &payload)
                .map_err(|cause| Error::Operation { id, cause })?;
            Ok(Operation { id, title, kind })
        })
        .collect()
    }
}

/// A kind as its two columns hold it.
///
/// The payload is kept as the text it was written as and never as a map of
/// its fields. A map keeps the last of two fields with one name, and the
/// kind reading the text itself is what refuses the second.
#[derive(Serialize, Deserialize)]
struct Columns<'a> {
    kind: String,
    #[serde(borrow)]
    payload: &'a RawValue,
}

impl Kind {
    /// The name and the payload this kind is written as.
    fn to_columns(&self) -> serde_json::Result<(String, String)> {
        let written = serde_json::to_string(self)?;
        let Columns { kind, payload } = serde_json::from_str(&written)?;
        Ok((kind, payload.get().to_owned()))
    }

    /// The kind that two columns hold.
    fn from_columns(kind: &str, payload: &str) -> serde_json::Result<Self> {
        let columns = Columns {
            kind: kind.to_owned(),
            payload: serde_json::from_str(payload)?,
        };
        serde_json::from_str(&serde_json::to_string(&columns)?)
    }
}

/// Queue one operation for a show, on the connection or the transaction the
/// caller holds.
///
/// The instant arrives as the text the caller wrote beside it, so that an
/// episode and its operation carry one instant.
///
/// # Errors
///
/// [`Error::Write`] when the row cannot be written.
pub(crate) fn enqueue(
    connection: &Connection,
    show: i64,
    kind: &Kind,
    at: &str,
) -> Result<(), Error> {
    // A kind comes apart into a name and a payload as long as it has
    // fields, and each one here has. One added without any would have no
    // payload, and would fail this write with SQLite never asked.
    let (kind, payload) = kind
        .to_columns()
        .map_err(|cause| Error::Write(rusqlite::Error::ToSqlConversionFailure(cause.into())))?;
    connection
        .execute(
            "INSERT INTO sync_queue (show, kind, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![show, kind, payload, at],
        )
        .map_err(Error::Write)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, UNIX_EPOCH};

    use benshi_core::path::RawPath;
    use serde_json::json;
    use tempfile::TempDir;

    use super::{Kind, Operation, Rating, Status, enqueue};
    use crate::{Error, Store, Viewing};

    fn a_store() -> (TempDir, Store) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("a fresh database opens");
        (home, store)
    }

    fn a_viewing() -> Viewing {
        Viewing {
            title: "Show Title".to_owned(),
            episode: Some(3),
            media: RawPath::from_bytes(b"Show Title - 03.mkv".to_vec()),
            at: UNIX_EPOCH + Duration::from_secs(1_790_000_000),
        }
    }

    fn a_rating(points: u8) -> Rating {
        Rating::new(points).expect("a rating from 1 to 100")
    }

    /// One of each kind, and a film's progress beside an episode's.
    ///
    /// A kind added to the enum stops the build here until the list has one.
    fn every_kind() -> Vec<Kind> {
        let every = vec![
            Kind::Progress { episode: Some(3) },
            Kind::Progress { episode: None },
            Kind::Rewatch { count: 2 },
            Kind::Entry {
                status: Status::Completed,
                progress: 12,
                score: Some(a_rating(80)),
                rewatches: 1,
            },
        ];
        for kind in &every {
            match kind {
                Kind::Progress { .. } | Kind::Rewatch { .. } | Kind::Entry { .. } => {}
            }
        }
        every
    }

    /// File the show if it is new and queue one operation for it.
    fn queue(store: &Store, title: &str, kind: &Kind) {
        store
            .connection
            .execute("INSERT OR IGNORE INTO shows (title) VALUES (?1)", [title])
            .expect("the show is filed");
        let show: i64 = store
            .connection
            .query_row("SELECT id FROM shows WHERE title = ?1", [title], |row| {
                row.get(0)
            })
            .expect("the show is there");
        enqueue(&store.connection, show, kind, "2026-09-27T00:00:00Z")
            .expect("the operation is queued");
    }

    /// A row put into the queue by hand, which is how a row this build did
    /// not write gets there.
    fn a_row(store: &Store, kind: &str, payload: &str) {
        store
            .connection
            .execute(
                "INSERT OR IGNORE INTO shows (title) VALUES ('Show Title')",
                [],
            )
            .expect("the show is filed");
        store
            .connection
            .execute(
                "INSERT INTO sync_queue (show, kind, payload, created_at) \
                 VALUES (1, ?1, ?2, '2026-09-27T00:00:00Z')",
                [kind, payload],
            )
            .expect("the row goes in");
    }

    /// What a list holds for one show, as far as an operation can change it.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct Listed {
        status: Option<Status>,
        episode: Option<u32>,
        whole: bool,
        score: Option<Rating>,
        rewatches: u32,
    }

    /// A list kept in memory, one entry per show.
    ///
    /// It takes every operation by assignment: what the operation carries is
    /// what the entry holds afterwards, and nothing is read from the entry to
    /// work that out. That is the meaning the documentation of each kind
    /// gives it, written as code.
    #[derive(Debug, Default)]
    struct Ledger(BTreeMap<String, Listed>);

    impl Ledger {
        fn apply(&mut self, operation: &Operation) {
            let listed = self.0.entry(operation.title.clone()).or_default();
            match operation.kind {
                Kind::Progress {
                    episode: Some(episode),
                } => listed.episode = Some(episode),
                Kind::Progress { episode: None } => listed.whole = true,
                Kind::Rewatch { count } => listed.rewatches = count,
                Kind::Entry {
                    status,
                    progress,
                    score,
                    rewatches,
                } => {
                    listed.status = Some(status);
                    listed.episode = Some(progress);
                    listed.score = score;
                    listed.rewatches = rewatches;
                }
            }
        }
    }

    #[test]
    fn recording_an_episode_queues_the_operation_for_it() {
        let (_home, mut store) = a_store();

        store.record(&a_viewing()).expect("recording cannot fail");

        assert_eq!(
            store.queued().expect("the queue is readable"),
            [Operation {
                id: 1,
                title: "Show Title".to_owned(),
                kind: Kind::Progress { episode: Some(3) },
            }]
        );
    }

    #[test]
    fn an_operation_stays_in_the_queue_after_it_was_read() {
        // Reading is not taking. The operation is there for the next reading
        // and for the next process that opens the file.
        let (home, mut store) = a_store();
        store.record(&a_viewing()).expect("recording cannot fail");

        let first = store.queued().expect("the queue is readable");
        let again = store.queued().expect("the queue is readable");
        drop(store);
        let reopened = Store::open(&home.path().join("benshi.db"))
            .expect("the database opens again")
            .queued()
            .expect("the queue is readable");

        assert_eq!(first.len(), 1);
        assert_eq!(again, first);
        assert_eq!(reopened, first);
    }

    #[test]
    fn every_kind_is_read_back_as_the_kind_it_was_written_as() {
        let (_home, store) = a_store();
        let written = every_kind();
        for kind in &written {
            queue(&store, "Show Title", kind);
        }

        let read: Vec<Kind> = store
            .queued()
            .expect("the queue is readable")
            .into_iter()
            .map(|operation| operation.kind)
            .collect();

        assert_eq!(read, written);
    }

    #[test]
    fn the_queue_is_read_in_the_order_it_was_written() {
        // `id` is an INTEGER PRIMARY KEY without AUTOINCREMENT, so SQLite
        // hands out one past the highest row there is, and nothing deletes
        // from the queue: lowest `id` first is the order they went in.
        // Taking the highest row out would hand its number to the next one.
        let (_home, store) = a_store();
        queue(&store, "Show Title", &Kind::Progress { episode: Some(3) });
        queue(&store, "Another Show", &Kind::Progress { episode: Some(1) });
        queue(&store, "Show Title", &Kind::Progress { episode: Some(4) });

        let read = store.queued().expect("the queue is readable");

        let order: Vec<(i64, &str, &Kind)> = read
            .iter()
            .map(|operation| (operation.id, operation.title.as_str(), &operation.kind))
            .collect();
        assert_eq!(
            order,
            [
                (1, "Show Title", &Kind::Progress { episode: Some(3) }),
                (2, "Another Show", &Kind::Progress { episode: Some(1) }),
                (3, "Show Title", &Kind::Progress { episode: Some(4) }),
            ]
        );
    }

    #[test]
    fn a_kind_is_stored_by_name_beside_its_payload() {
        // What `sqlite3` shows. The payload is compared as JSON and not as
        // text, because the order of its keys is not part of the format.
        let (_home, store) = a_store();
        for kind in every_kind() {
            queue(&store, "Show Title", &kind);
        }

        let mut statement = store
            .connection
            .prepare("SELECT kind, payload FROM sync_queue ORDER BY id")
            .expect("the statement prepares");
        let stored: Vec<(String, serde_json::Value)> = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("the statement runs")
            .map(|row| {
                let (kind, payload) = row.expect("a row of text");
                (kind, serde_json::from_str(&payload).expect("JSON"))
            })
            .collect();

        assert_eq!(
            stored,
            [
                ("progress".to_owned(), json!({ "episode": 3 })),
                ("progress".to_owned(), json!({ "episode": null })),
                ("rewatch".to_owned(), json!({ "count": 2 })),
                (
                    "entry".to_owned(),
                    json!({ "status": "completed", "progress": 12, "score": 80, "rewatches": 1 })
                ),
            ]
        );
    }

    #[test]
    fn every_status_is_stored_under_a_name_of_its_own() {
        // The name is what the file holds, so a variant renamed in the code
        // must not rename it in the file.
        let (_home, store) = a_store();
        let every = [
            (Status::Planned, "planned"),
            (Status::Watching, "watching"),
            (Status::Rewatching, "rewatching"),
            (Status::Completed, "completed"),
            (Status::OnHold, "on_hold"),
            (Status::Dropped, "dropped"),
        ];
        for (status, _) in every {
            match status {
                Status::Planned
                | Status::Watching
                | Status::Rewatching
                | Status::Completed
                | Status::OnHold
                | Status::Dropped => {}
            }
            let entry = Kind::Entry {
                status,
                progress: 0,
                score: None,
                rewatches: 0,
            };
            queue(&store, "Show Title", &entry);
        }

        let mut statement = store
            .connection
            .prepare("SELECT payload ->> 'status' FROM sync_queue ORDER BY id")
            .expect("the statement prepares");
        let stored: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .expect("the statement runs")
            .collect::<Result<_, _>>()
            .expect("every row has a status");

        assert_eq!(stored, every.map(|(_, name)| name));
    }

    #[test]
    fn a_kind_this_build_does_not_know_is_refused_and_the_row_is_named() {
        // Refused whole rather than read around: an operation skipped is an
        // episode no list ever hears of, and nothing would say so. The
        // payload is one a progress could carry, so the name alone is what
        // is refused.
        let (_home, store) = a_store();
        queue(&store, "Show Title", &Kind::Progress { episode: Some(3) });
        a_row(&store, "bump", r#"{"episode":3}"#);

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 2, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn an_operation_whose_show_is_not_there_is_refused_and_the_row_is_named() {
        // Foreign keys keep such a row from being written through the store.
        // `sqlite3` leaves them off unless it is asked, so a row written by
        // hand or a show deleted by hand leaves one.
        let (_home, store) = a_store();
        queue(&store, "Show Title", &Kind::Progress { episode: Some(3) });
        store
            .connection
            .execute_batch(
                r#"PRAGMA foreign_keys = OFF;
                   INSERT INTO sync_queue (show, kind, payload, created_at)
                   VALUES (99, 'progress', '{"episode":3}', '2026-09-27T00:00:00Z');
                   PRAGMA foreign_keys = ON;"#,
            )
            .expect("the row goes in with nothing checking it");

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Orphan { id: 2 })),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_payload_that_spells_a_field_twice_is_refused() {
        // Which of the two was meant is a guess.
        let (_home, store) = a_store();
        a_row(&store, "progress", r#"{"episode":3,"episode":4}"#);

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 1, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_payload_that_is_not_json_is_refused() {
        let (_home, store) = a_store();
        a_row(&store, "progress", "episode three");

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 1, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_payload_is_read_as_its_own_kind_and_no_other() {
        let (_home, store) = a_store();
        a_row(&store, "rewatch", r#"{"episode":3}"#);

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 1, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_progress_that_spells_no_episode_is_refused_rather_than_read_as_a_film() {
        // A film's progress spells `null`. A payload with no episode in it
        // at all says nothing, and nothing is not a film: read as one, it
        // would say a whole work was seen.
        let (_home, store) = a_store();
        a_row(&store, "progress", "{}");

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 1, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_field_the_kind_does_not_have_is_refused() {
        let (_home, store) = a_store();
        a_row(&store, "progress", r#"{"episode":3,"count":1}"#);

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 1, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_rating_is_from_one_to_a_hundred() {
        // Nought is no rating, and no rating is `None`: two ways to say it
        // would be one too many.
        assert_eq!(Rating::new(0), None);
        assert_eq!(Rating::new(1).map(Rating::points), Some(1));
        assert_eq!(Rating::new(100).map(Rating::points), Some(100));
        assert_eq!(Rating::new(101), None);
    }

    #[test]
    fn a_score_that_is_not_a_rating_is_refused() {
        for score in [0, 101] {
            let (_home, store) = a_store();
            a_row(
                &store,
                "entry",
                &json!({ "status": "completed", "progress": 12, "score": score, "rewatches": 1 })
                    .to_string(),
            );

            let refused = store.queued();

            assert!(
                matches!(refused, Err(Error::Operation { id: 1, .. })),
                "a score of {score} got {refused:?}"
            );
        }
    }

    #[test]
    fn an_entry_that_spells_no_score_is_refused_rather_than_read_as_unscored() {
        // An entry with no score spells `null`. One with no score in it at
        // all is not a whole entry, and read as unscored it would say the
        // score was taken away.
        let (_home, store) = a_store();
        a_row(
            &store,
            "entry",
            r#"{"status":"completed","progress":12,"rewatches":1}"#,
        );

        let refused = store.queued();

        assert!(
            matches!(refused, Err(Error::Operation { id: 1, .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn an_operation_applied_twice_leaves_what_applying_it_once_left() {
        // Each kind is queued, read back, and applied to an empty ledger
        // once and then again. What one application leaves is written out
        // here by hand, so that a ledger which ignores an operation is not
        // mistaken for one the operation leaves unchanged.
        let nothing = Listed::default();
        let cases = [
            (
                Kind::Progress { episode: Some(3) },
                Listed {
                    episode: Some(3),
                    ..nothing.clone()
                },
            ),
            (
                Kind::Progress { episode: None },
                Listed {
                    whole: true,
                    ..nothing.clone()
                },
            ),
            (
                Kind::Rewatch { count: 2 },
                Listed {
                    rewatches: 2,
                    ..nothing.clone()
                },
            ),
            (
                Kind::Entry {
                    status: Status::Completed,
                    progress: 12,
                    score: Some(a_rating(80)),
                    rewatches: 1,
                },
                Listed {
                    status: Some(Status::Completed),
                    episode: Some(12),
                    whole: false,
                    score: Some(a_rating(80)),
                    rewatches: 1,
                },
            ),
        ];
        assert_eq!(
            cases.len(),
            every_kind().len(),
            "a case for each kind there is"
        );

        for (kind, left) in cases {
            let (_home, store) = a_store();
            queue(&store, "Show Title", &kind);
            let queued = store.queued().expect("the queue is readable");
            let [operation] = queued.as_slice() else {
                panic!("one operation was queued, and {queued:?} was read")
            };
            let mut ledger = Ledger::default();

            ledger.apply(operation);
            assert_eq!(
                ledger.0.get("Show Title"),
                Some(&left),
                "{kind:?} applied once"
            );

            ledger.apply(operation);
            assert_eq!(
                ledger.0.get("Show Title"),
                Some(&left),
                "{kind:?} applied twice"
            );
        }
    }
}
