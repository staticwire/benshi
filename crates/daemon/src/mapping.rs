//! The names and the pins `benshi map` gives entries of the list.
//!
//! A name the user gives means one entry from then on: it is kept in the
//! store, and a recognizer that reads it is put in place of the one the rounds
//! decide against. A call that changes a name reads what the store holds,
//! makes the change and puts the recognizer in place under the store's lock,
//! as the list's task does when it files a list, so neither puts in place what
//! the other has outdated.
//!
//! A pin says that episodes of a show are as many episodes of an entry. It is
//! kept in the store, under the same lock, and no recognizer reads it.

use std::sync::{Arc, Mutex};

use benshi_core::ShowId;
use benshi_core::list::{Entry, EntryId};
use benshi_core::pin::{Episodes, Pin, Unpinnable};
use benshi_core::recognize::altname::Unnameable;
use benshi_core::recognize::normalize::Key;
use benshi_store::Store;

use crate::detection::POISONED_STORE;
use crate::listing::{BACKEND, corpus_of, kept, named};
use crate::recognition::{Current, Recognizer};

/// Where the names the user gives are kept, and the recognizer they reach.
#[derive(Debug, Clone)]
pub struct Mapping {
    store: Arc<Mutex<Store>>,
    current: Current,
}

/// What a name was made to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    /// The name, as it was given.
    pub spelled: String,
    /// The entry it means.
    pub entry: EntryId,
    /// The title the list gives the entry.
    pub title: String,
    /// What a name of its key meant before, where it meant another entry.
    pub replaced: Option<Meant>,
}

/// An entry a name meant, as the list holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meant {
    /// The name, as it was given.
    pub spelled: String,
    /// The entry, and none where the list holds no entry of its show.
    pub entry: Option<EntryId>,
    /// The title the list gives the entry, and the show's where it holds none.
    pub title: String,
}

/// A name the user gave, as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Given {
    /// What the name means.
    pub meant: Meant,
    /// Why it is not in force, where it is not.
    pub not_in_force: Option<String>,
}

/// Episodes of a show and the episodes of an entry they are, as a listing
/// shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// The show, by the title the store files it under.
    pub show: String,
    /// The show's episodes.
    pub episodes: Episodes,
    /// The entry.
    pub entry: EntryId,
    /// The entry's episodes.
    pub entry_episodes: Episodes,
    /// The title the list gives the entry, and none where the list no longer
    /// holds it.
    pub title: Option<String>,
}

/// A pin made, and the pin of the same episodes it took the place of, where
/// that one named other episodes or another entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinning {
    /// The pin made.
    pub made: Pinned,
    /// The pin it took the place of.
    pub replaced: Option<Pinned>,
}

/// A show a title could name, as a refusal names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowOf {
    /// The title the store files the show under.
    pub title: String,
    /// The entry the list holds of the show, where it holds one.
    pub entry: Option<EntryId>,
}

/// Why a name or a pin was not made, or not taken back.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    /// The list holds no entry of that number.
    #[error("AniList {} is not on the list", .0.0)]
    NotOnTheList(EntryId),
    /// Recognition refused the name, as it refuses one as it is made.
    #[error("{}", refusal_of(.0))]
    Unnameable(Unnameable),
    /// No show is filed under the title, nor under a title of its key.
    #[error("no show is titled \"{0}\"")]
    NoShow(String),
    /// More shows than one are filed under the title, or under titles of its
    /// key where none is filed under the title.
    #[error("\"{named}\" names more than one show: {}", shows_named(.shows))]
    Shows {
        /// The title as it was given.
        named: String,
        /// The shows it names.
        shows: Vec<ShowOf>,
    },
    /// The pin's runs are not of one length, or the entry's begins at 0.
    #[error("{}", unpinnable_of(.0))]
    Unpinnable(Unpinnable),
    /// The episodes share an episode with a pin of the show made before.
    #[error("{}", overlap_of(*.asked, .pinned))]
    Overlaps {
        /// The episodes asked for.
        asked: Episodes,
        /// The pin they overlap.
        pinned: Pinned,
    },
    /// The store refused to read or write it.
    #[error("store refused: {0}")]
    Store(#[from] benshi_store::Error),
    /// The store could not be asked at all, as when the daemon is stopping.
    #[error("store could not be asked: {0}")]
    NotAsked(String),
}

/// What a person reads for a name recognition refuses.
fn refusal_of(refused: &Unnameable) -> String {
    match refused {
        Unnameable::Unspellable { spelled } => {
            format!("\"{spelled}\" spells no letters to look anything up by")
        }
        Unnameable::NotOnTheList { .. } => "its entry is not on the list".to_owned(),
        Unnameable::AlreadyMeans { spelled, entry } => {
            format!("\"{spelled}\" already means \"{entry}\", whose files it would take")
        }
    }
}

/// A run of episodes in words: `episode 2`, `episodes 14-26`.
fn run_words(episodes: Episodes) -> String {
    if episodes.count() == 1 {
        format!("episode {}", episodes.first())
    } else {
        format!("episodes {}-{}", episodes.first(), episodes.last())
    }
}

/// The shows a title names, each with the entry the list holds of it.
fn shows_named(shows: &[ShowOf]) -> String {
    let named: Vec<String> = shows
        .iter()
        .map(|show| match show.entry {
            Some(entry) => format!("\"{}\" (AniList {})", show.title, entry.0),
            None => format!("\"{}\" (not on the list)", show.title),
        })
        .collect();
    named.join(", ")
}

/// What a person reads for a pin that is no pin.
fn unpinnable_of(refused: &Unpinnable) -> String {
    match refused {
        Unpinnable::Lengths {
            episodes,
            entry_episodes,
        } => format!(
            "{} and {} are not of one length",
            run_words(*episodes),
            run_words(*entry_episodes)
        ),
        Unpinnable::EpisodeZero => "AniList numbers the episodes of an entry from 1".to_owned(),
    }
}

/// What a person reads for episodes that overlap a pin of their show.
fn overlap_of(asked: Episodes, pinned: &Pinned) -> String {
    let overlap = if asked.count() == 1 {
        "overlaps"
    } else {
        "overlap"
    };
    format!(
        "\"{}\" {} {overlap} {}, mapped to AniList {} {}",
        pinned.show,
        run_words(asked),
        run_words(pinned.episodes),
        pinned.entry.0,
        run_words(pinned.entry_episodes)
    )
}

/// Run `call` with the store locked, on a thread of its own.
async fn locked<T, F>(store: &Arc<Mutex<Store>>, call: F) -> Result<T, Refused>
where
    T: Send + 'static,
    F: FnOnce(&mut Store) -> Result<T, Refused> + Send + 'static,
{
    let store = Arc::clone(store);
    let asked = tokio::task::spawn_blocking(move || call(&mut store.lock().expect(POISONED_STORE)));

    match asked.await {
        Ok(answer) => answer,
        // Handed on as it was raised.
        Err(panicked) if panicked.is_panic() => std::panic::resume_unwind(panicked.into_panic()),
        Err(given_up) => Err(Refused::NotAsked(given_up.to_string())),
    }
}

/// What a name meant, from the entry the list holds of its show, and from
/// the show's own title where the list holds none.
fn meant(
    store: &Store,
    listed: &[(ShowId, Entry)],
    spelled: &str,
    show: ShowId,
) -> Result<Meant, Refused> {
    let (entry, title) = match listed.iter().find(|(listed_show, _)| *listed_show == show) {
        Some((_, entry)) => (Some(entry.id), entry.title.clone()),
        None => (None, store.title_of(show)?.unwrap_or_default()),
    };
    Ok(Meant {
        spelled: spelled.to_owned(),
        entry,
        title,
    })
}

/// The show `named` names, with the title it is filed under: the one filed
/// under that title, and where none is, the one under a title of its key.
fn show_named(
    store: &Store,
    listed: &[(ShowId, Entry)],
    named: &str,
) -> Result<(ShowId, String), Refused> {
    let shows = store.shows()?;
    let mut found: Vec<&(ShowId, String)> =
        shows.iter().filter(|(_, title)| title == named).collect();
    if found.is_empty()
        && let Some(key) = Key::from_title(named)
    {
        found = shows
            .iter()
            .filter(|(_, title)| Key::from_title(title).as_ref() == Some(&key))
            .collect();
    }

    match found.as_slice() {
        [] => Err(Refused::NoShow(named.to_owned())),
        [(show, title)] => Ok((*show, title.clone())),
        several => Err(Refused::Shows {
            named: named.to_owned(),
            shows: several
                .iter()
                .map(|(show, title)| ShowOf {
                    title: title.clone(),
                    entry: listed
                        .iter()
                        .find(|(listed_show, _)| listed_show == show)
                        .map(|(_, entry)| entry.id),
                })
                .collect(),
        }),
    }
}

/// A pin as a listing shows it.
fn pinned_of(store: &Store, listed: &[(ShowId, Entry)], pin: &Pin) -> Result<Pinned, Refused> {
    Ok(Pinned {
        show: store.title_of(pin.show())?.unwrap_or_default(),
        episodes: pin.episodes(),
        entry: pin.entry(),
        entry_episodes: pin.entry_episodes(),
        title: listed
            .iter()
            .find(|(_, entry)| entry.id == pin.entry())
            .map(|(_, entry)| entry.title.clone()),
    })
}

impl Mapping {
    /// The names kept in `store`, read by the recognizer `current` holds.
    #[must_use]
    pub const fn new(store: Arc<Mutex<Store>>, current: Current) -> Self {
        Self { store, current }
    }

    /// Make `spelled` mean the entry of the list numbered `entry`.
    ///
    /// Every name of its key that was kept goes, in force or not, so the
    /// store holds one name of a key.
    ///
    /// # Errors
    ///
    /// [`Refused`], saying which.
    pub async fn map(&self, spelled: String, entry: EntryId) -> Result<Mapped, Refused> {
        let current = self.current.clone();
        locked(&self.store, move |store| {
            let listed = store.listed(BACKEND)?;
            let (show, title) = listed
                .iter()
                .find(|(_, listed)| listed.id == entry)
                .map(|(show, listed)| (*show, listed.title.clone()))
                .ok_or(Refused::NotOnTheList(entry))?;
            let corpus = corpus_of(&listed);
            let assigned = store.assignments()?;
            let (mut altnames, _not_in_force) = named(&corpus, &assigned);
            altnames
                .name(&spelled, show, &corpus)
                .map_err(Refused::Unnameable)?;
            // Every name of its key is displaced: one not in force is not
            // among the names `altnames` was built from.
            let key = Key::from_title(&spelled);
            let displaced: Vec<&(String, ShowId)> = assigned
                .iter()
                .filter(|(given, _)| Key::from_title(given) == key)
                .collect();
            let texts: Vec<&str> = displaced.iter().map(|(given, _)| given.as_str()).collect();
            store.assign(&spelled, show, &texts)?;
            let replaced = displaced
                .last()
                .filter(|(_, before)| *before != show)
                .map(|(given, before)| meant(store, &listed, given, *before))
                .transpose()?;
            current.replace(Recognizer::of(corpus, altnames));

            Ok(Mapped {
                spelled,
                entry,
                title,
                replaced,
            })
        })
        .await
    }

    /// Take back what `spelled` was made to mean, and answer the name as it
    /// was given, or nothing where no name of that key was given.
    ///
    /// # Errors
    ///
    /// [`Refused::Store`] or [`Refused::NotAsked`].
    pub async fn unmap(&self, spelled: String) -> Result<Option<String>, Refused> {
        let current = self.current.clone();
        locked(&self.store, move |store| {
            let Some(key) = Key::from_title(&spelled) else {
                return Ok(None);
            };
            let Some((given, _)) = store
                .assignments()?
                .into_iter()
                .find(|(given, _)| Key::from_title(given).as_ref() == Some(&key))
            else {
                return Ok(None);
            };
            store.unassign(&given)?;
            current.replace(kept(store)?);

            Ok(Some(given))
        })
        .await
    }

    /// Every name given, in the order given.
    ///
    /// # Errors
    ///
    /// [`Refused::Store`] or [`Refused::NotAsked`].
    pub async fn given(&self) -> Result<Vec<Given>, Refused> {
        locked(&self.store, |store| {
            let listed = store.listed(BACKEND)?;
            let assigned = store.assignments()?;
            let (_in_force, not_in_force) = named(&corpus_of(&listed), &assigned);

            assigned
                .iter()
                .map(|(spelled, show)| {
                    let reason = not_in_force
                        .iter()
                        .find(|(refused, _, _)| refused == spelled)
                        .map(|(_, _, why)| refusal_of(why));
                    Ok(Given {
                        meant: meant(store, &listed, spelled, *show)?,
                        not_in_force: reason,
                    })
                })
                .collect()
        })
        .await
    }

    /// Make `episodes` of the show `show` names the episodes `entry_episodes`
    /// of the entry of the list numbered `entry`.
    ///
    /// A pin of the same episodes of the show takes the place of the one
    /// before. A pin that shares an episode with another of the show is
    /// refused.
    ///
    /// # Errors
    ///
    /// [`Refused`], saying which.
    pub async fn pin(
        &self,
        show: String,
        episodes: Episodes,
        entry: EntryId,
        entry_episodes: Episodes,
    ) -> Result<Pinning, Refused> {
        locked(&self.store, move |store| {
            let listed = store.listed(BACKEND)?;
            let (show, _title) = show_named(store, &listed, &show)?;
            let made =
                Pin::new(show, episodes, entry, entry_episodes).map_err(Refused::Unpinnable)?;
            if !listed.iter().any(|(_, listed)| listed.id == entry) {
                return Err(Refused::NotOnTheList(entry));
            }
            let before = store
                .pins(BACKEND)?
                .into_iter()
                .find(|pin| pin.show() == show && pin.episodes().overlaps(episodes));
            if let Some(before) = before
                && before.episodes() != episodes
            {
                return Err(Refused::Overlaps {
                    asked: episodes,
                    pinned: pinned_of(store, &listed, &before)?,
                });
            }
            store.pin(BACKEND, &made)?;
            let replaced = before
                .filter(|before| *before != made)
                .map(|before| pinned_of(store, &listed, &before))
                .transpose()?;

            Ok(Pinning {
                made: pinned_of(store, &listed, &made)?,
                replaced,
            })
        })
        .await
    }

    /// Take back the pin of `episodes` of the show `show` names, and answer
    /// the title the show is filed under, or nothing where those episodes
    /// were not pinned.
    ///
    /// # Errors
    ///
    /// [`Refused::NoShow`] and [`Refused::Shows`], as [`Mapping::pin`] has
    /// them, [`Refused::Store`] or [`Refused::NotAsked`].
    pub async fn unpin(&self, show: String, episodes: Episodes) -> Result<Option<String>, Refused> {
        locked(&self.store, move |store| {
            let listed = store.listed(BACKEND)?;
            let (show, title) = show_named(store, &listed, &show)?;
            Ok(store.unpin(BACKEND, show, episodes)?.then_some(title))
        })
        .await
    }

    /// Every pin made, in the order made.
    ///
    /// # Errors
    ///
    /// [`Refused::Store`] or [`Refused::NotAsked`].
    pub async fn pins(&self) -> Result<Vec<Pinned>, Refused> {
        locked(&self.store, |store| {
            let listed = store.listed(BACKEND)?;
            store
                .pins(BACKEND)?
                .iter()
                .map(|pin| pinned_of(store, &listed, pin))
                .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use benshi_core::ShowId;
    use benshi_core::list::{Entry, EntryId, Status};
    use benshi_core::path::RawPath;
    use benshi_core::pin::{Episodes, Pin, Unpinnable};
    use benshi_core::recognize::altname::Unnameable;
    use benshi_core::recognize::{Recognition, Stage};
    use benshi_store::Store;
    use tempfile::TempDir;

    use super::{Given, Mapped, Mapping, Meant, Pinned, Pinning, Refused, ShowOf};
    use crate::listing;
    use crate::recognition::Current;

    const FIRST: EntryId = EntryId(900_001);
    const SEQUEL: EntryId = EntryId(900_002);

    fn an_entry(id: EntryId, title: &str) -> Entry {
        Entry {
            id,
            title: title.to_owned(),
            spellings: Vec::new(),
            format: None,
            episodes: Some(12),
            episode_length: None,
            year: None,
            released: true,
            status: Status::Watching,
            progress: 0,
            rewatches: 0,
            score: None,
        }
    }

    /// A store holding two entries a name cannot tell apart, and a mapping
    /// over it with a recognizer of them in place.
    fn a_mapping() -> (TempDir, Arc<Mutex<Store>>, Current, Mapping) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let mut store = Store::open(&home.path().join("benshi.db")).expect("the database opens");
        store
            .file(
                listing::BACKEND,
                &[an_entry(FIRST, "Nisekoi"), an_entry(SEQUEL, "Nisekoi:")],
            )
            .expect("the list is filed");
        let current = Current::new(listing::kept(&store).expect("the list reads"));
        let store = Arc::new(Mutex::new(store));
        let mapping = Mapping::new(Arc::clone(&store), current.clone());
        (home, store, current, mapping)
    }

    /// The show the store filed the entry under.
    fn show_of(store: &Arc<Mutex<Store>>, entry: EntryId) -> ShowId {
        store
            .lock()
            .expect("the store")
            .listed(listing::BACKEND)
            .expect("the list reads")
            .into_iter()
            .find(|(_, listed)| listed.id == entry)
            .map(|(show, _)| show)
            .expect("the entry is filed")
    }

    /// What the recognizer in place answers for a file of the shared name.
    fn answered(current: &Current) -> Recognition {
        current
            .now()
            .decide(&RawPath::from_bytes(b"[Group] Nisekoi - 03.mkv".to_vec()))
            .1
    }

    fn kept(store: &Arc<Mutex<Store>>) -> Vec<(String, ShowId)> {
        store
            .lock()
            .expect("the store")
            .assignments()
            .expect("the store answers")
    }

    #[tokio::test]
    async fn a_name_mapped_means_the_entry_from_then_on() {
        let (_home, store, current, mapping) = a_mapping();
        assert!(matches!(answered(&current), Recognition::Ambiguous(_)));

        let mapped = mapping.map("Nisekoi".to_owned(), SEQUEL).await;

        assert_eq!(
            mapped.expect("the name is mapped"),
            Mapped {
                spelled: "Nisekoi".to_owned(),
                entry: SEQUEL,
                title: "Nisekoi:".to_owned(),
                replaced: None,
            }
        );
        let sequel = show_of(&store, SEQUEL);
        assert!(
            matches!(
                answered(&current),
                Recognition::Recognized(found)
                    if found.show == sequel && matches!(found.stage, Stage::Altname(_))
            ),
            "{:?}",
            answered(&current)
        );
        assert_eq!(kept(&store), [("Nisekoi".to_owned(), sequel)]);
    }

    #[tokio::test]
    async fn a_name_mapped_to_an_entry_the_list_does_not_hold_is_refused() {
        let (_home, store, current, mapping) = a_mapping();

        let refused = mapping.map("Nisekoi".to_owned(), EntryId(999)).await;

        assert!(
            matches!(refused, Err(Refused::NotOnTheList(EntryId(999)))),
            "{refused:?}"
        );
        assert_eq!(kept(&store), []);
        assert!(matches!(answered(&current), Recognition::Ambiguous(_)));
    }

    #[tokio::test]
    async fn a_name_that_already_means_another_entry_is_refused() {
        // The mark tells the sequel apart, so its name means it and no other.
        let (_home, store, _current, mapping) = a_mapping();

        let refused = mapping.map("Nisekoi:".to_owned(), FIRST).await;

        assert!(
            matches!(
                &refused,
                Err(Refused::Unnameable(Unnameable::AlreadyMeans { entry, .. })) if entry == "Nisekoi:"
            ),
            "{refused:?}"
        );
        assert_eq!(kept(&store), []);
    }

    #[tokio::test]
    async fn a_name_mapped_again_says_what_it_meant_before() {
        let (_home, store, _current, mapping) = a_mapping();
        mapping
            .map("Nisekoi".to_owned(), SEQUEL)
            .await
            .expect("the name is mapped");

        let again = mapping
            .map("Nisekoi".to_owned(), FIRST)
            .await
            .expect("the name is mapped");

        assert_eq!(
            again.replaced,
            Some(Meant {
                spelled: "Nisekoi".to_owned(),
                entry: Some(SEQUEL),
                title: "Nisekoi:".to_owned(),
            })
        );
        assert_eq!(
            kept(&store),
            [("Nisekoi".to_owned(), show_of(&store, FIRST))]
        );
    }

    #[tokio::test]
    async fn a_name_mapped_again_to_the_entry_it_means_replaces_nothing() {
        // It means what it meant, so nothing is left to put back.
        let (_home, store, _current, mapping) = a_mapping();
        mapping
            .map("Nisekoi".to_owned(), SEQUEL)
            .await
            .expect("the name is mapped");

        let again = mapping
            .map("nisekoi".to_owned(), SEQUEL)
            .await
            .expect("the name is mapped");

        assert_eq!(again.replaced, None);
        assert_eq!(
            kept(&store),
            [("nisekoi".to_owned(), show_of(&store, SEQUEL))]
        );
    }

    #[tokio::test]
    async fn a_name_of_its_key_whose_entry_left_the_list_is_displaced_all_the_same() {
        // Not in force, it is not among the names a recognizer is built from.
        let (_home, store, _current, mapping) = a_mapping();
        mapping
            .map("Nisekoi".to_owned(), SEQUEL)
            .await
            .expect("the name is mapped");
        store
            .lock()
            .expect("the store")
            .file(listing::BACKEND, &[an_entry(FIRST, "Nisekoi")])
            .expect("the list is filed");

        let again = mapping
            .map("nisekoi".to_owned(), FIRST)
            .await
            .expect("the name is mapped");

        assert_eq!(
            again.replaced,
            Some(Meant {
                spelled: "Nisekoi".to_owned(),
                entry: None,
                title: "Nisekoi:".to_owned(),
            })
        );
        assert_eq!(
            kept(&store),
            [("nisekoi".to_owned(), show_of(&store, FIRST))]
        );
    }

    #[tokio::test]
    async fn a_name_taken_back_means_what_the_list_says_again() {
        let (_home, store, current, mapping) = a_mapping();
        mapping
            .map("Nisekoi".to_owned(), SEQUEL)
            .await
            .expect("the name is mapped");

        let taken = mapping.unmap("nisekoi".to_owned()).await;

        assert_eq!(
            taken.expect("the store answers"),
            Some("Nisekoi".to_owned())
        );
        assert_eq!(kept(&store), []);
        assert!(matches!(answered(&current), Recognition::Ambiguous(_)));
    }

    #[tokio::test]
    async fn taking_back_a_name_that_was_never_given_is_nothing() {
        let (_home, _store, _current, mapping) = a_mapping();

        let taken = mapping.unmap("Nisekoi".to_owned()).await;

        assert_eq!(taken.expect("the store answers"), None);
    }

    #[tokio::test]
    async fn the_names_given_are_listed_with_what_they_mean_and_why_one_is_not_in_force() {
        let (_home, store, _current, mapping) = a_mapping();
        {
            let mut store = store.lock().expect("the store");
            let gone = store
                .new_show("A Show That Left")
                .expect("the show is filed");
            store
                .assign("Old Name", gone, &[])
                .expect("the name is kept");
        }
        mapping
            .map("Nisekoi".to_owned(), SEQUEL)
            .await
            .expect("the name is mapped");

        let given = mapping.given().await.expect("the store answers");

        assert_eq!(
            given,
            [
                Given {
                    meant: Meant {
                        spelled: "Old Name".to_owned(),
                        entry: None,
                        title: "A Show That Left".to_owned(),
                    },
                    not_in_force: Some("its entry is not on the list".to_owned()),
                },
                Given {
                    meant: Meant {
                        spelled: "Nisekoi".to_owned(),
                        entry: Some(SEQUEL),
                        title: "Nisekoi:".to_owned(),
                    },
                    not_in_force: None,
                },
            ]
        );
    }

    fn run(first: u32, last: u32) -> Episodes {
        Episodes::new(first, last).expect("a run from its first episode to its last")
    }

    /// Every pin the store keeps.
    fn pinned_in(store: &Arc<Mutex<Store>>) -> Vec<Pin> {
        store
            .lock()
            .expect("the store")
            .pins(listing::BACKEND)
            .expect("the store answers")
    }

    /// Episodes 13 to 24 of the first show, which are the sequel's 1 to 12.
    fn the_sequel_pinned(store: &Arc<Mutex<Store>>) -> Pin {
        Pin::new(show_of(store, FIRST), run(13, 24), SEQUEL, run(1, 12))
            .expect("runs of one length")
    }

    /// [`the_sequel_pinned`], as a listing shows it.
    fn the_sequel_listed() -> Pinned {
        Pinned {
            show: "Nisekoi".to_owned(),
            episodes: run(13, 24),
            entry: SEQUEL,
            entry_episodes: run(1, 12),
            title: Some("Nisekoi:".to_owned()),
        }
    }

    #[tokio::test]
    async fn episodes_pinned_are_kept_as_episodes_of_the_entry() {
        let (_home, store, _current, mapping) = a_mapping();

        let made = mapping
            .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
            .await
            .expect("the episodes are pinned");

        assert_eq!(
            made,
            Pinning {
                made: the_sequel_listed(),
                replaced: None,
            }
        );
        assert_eq!(pinned_in(&store), [the_sequel_pinned(&store)]);
    }

    #[tokio::test]
    async fn a_show_is_found_by_the_key_of_its_title_where_no_show_has_the_title() {
        let (_home, store, _current, mapping) = a_mapping();

        let made = mapping
            .pin("nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
            .await
            .expect("the episodes are pinned");

        assert_eq!(made.made, the_sequel_listed());
        assert_eq!(pinned_in(&store), [the_sequel_pinned(&store)]);
    }

    #[tokio::test]
    async fn the_show_filed_under_the_title_is_found_though_another_shares_its_key() {
        let (_home, store, _current, mapping) = a_mapping();
        let filed = {
            let store = store.lock().expect("the store");
            store.new_show("SHOW TITLE").expect("the show is filed");
            store.new_show("Show Title").expect("the show is filed")
        };

        mapping
            .pin("Show Title".to_owned(), run(1, 12), SEQUEL, run(1, 12))
            .await
            .expect("the episodes are pinned");

        assert_eq!(
            pinned_in(&store),
            [Pin::new(filed, run(1, 12), SEQUEL, run(1, 12)).expect("runs of one length")]
        );
    }

    #[tokio::test]
    async fn a_title_that_spells_no_key_finds_no_show_by_one() {
        // Neither has a key, and no key must stand for another.
        let (_home, store, _current, mapping) = a_mapping();
        store
            .lock()
            .expect("the store")
            .new_show("!!!")
            .expect("the show is filed");

        let refused = mapping
            .pin("???".to_owned(), run(1, 12), SEQUEL, run(1, 12))
            .await;

        assert!(
            matches!(&refused, Err(Refused::NoShow(named)) if named == "???"),
            "{refused:?}"
        );
        assert_eq!(pinned_in(&store), []);
    }

    #[tokio::test]
    async fn episodes_of_a_show_no_title_names_are_refused() {
        let (_home, store, _current, mapping) = a_mapping();

        let refused = mapping
            .pin("Another Show".to_owned(), run(1, 12), SEQUEL, run(1, 12))
            .await;

        assert!(
            matches!(&refused, Err(Refused::NoShow(named)) if named == "Another Show"),
            "{refused:?}"
        );
        assert_eq!(pinned_in(&store), []);
    }

    #[tokio::test]
    async fn two_shows_of_one_title_are_refused_with_both_named() {
        let (_home, store, _current, mapping) = a_mapping();
        store
            .lock()
            .expect("the store")
            .new_show("Nisekoi")
            .expect("the show is filed");

        let refused = mapping
            .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
            .await;

        let both = [
            ShowOf {
                title: "Nisekoi".to_owned(),
                entry: Some(FIRST),
            },
            ShowOf {
                title: "Nisekoi".to_owned(),
                entry: None,
            },
        ];
        assert!(
            matches!(&refused, Err(Refused::Shows { named, shows }) if named == "Nisekoi" && *shows == both),
            "{refused:?}"
        );
        assert_eq!(pinned_in(&store), []);
    }

    #[tokio::test]
    async fn two_shows_of_one_key_are_refused_with_both_named_where_no_show_has_the_title() {
        let (_home, store, _current, mapping) = a_mapping();
        {
            let store = store.lock().expect("the store");
            for title in ["Show Title", "SHOW TITLE"] {
                store.new_show(title).expect("the show is filed");
            }
        }

        let refused = mapping
            .pin("show title".to_owned(), run(1, 12), SEQUEL, run(1, 12))
            .await;

        let both = ["Show Title", "SHOW TITLE"].map(|title| ShowOf {
            title: title.to_owned(),
            entry: None,
        });
        assert!(
            matches!(&refused, Err(Refused::Shows { named, shows }) if named == "show title" && *shows == both),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn a_pin_of_runs_of_two_lengths_is_refused() {
        let (_home, store, _current, mapping) = a_mapping();

        let refused = mapping
            .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 11))
            .await;

        assert!(
            matches!(
                refused,
                Err(Refused::Unpinnable(Unpinnable::Lengths { .. }))
            ),
            "{refused:?}"
        );
        assert_eq!(pinned_in(&store), []);
    }

    #[tokio::test]
    async fn a_pin_to_an_entry_the_list_does_not_hold_is_refused() {
        let (_home, store, _current, mapping) = a_mapping();

        let refused = mapping
            .pin("Nisekoi".to_owned(), run(13, 24), EntryId(999), run(1, 12))
            .await;

        assert!(
            matches!(refused, Err(Refused::NotOnTheList(EntryId(999)))),
            "{refused:?}"
        );
        assert_eq!(pinned_in(&store), []);
    }

    #[tokio::test]
    async fn a_pin_overlapping_another_of_its_show_is_refused_with_that_one_named() {
        let (_home, store, _current, mapping) = a_mapping();
        mapping
            .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
            .await
            .expect("the episodes are pinned");

        for overlapping in [run(20, 30), run(1, 13), run(24, 24), run(14, 23)] {
            let entry_episodes = Episodes::new(1, 1 + overlapping.last() - overlapping.first())
                .expect("a run of one length");
            let refused = mapping
                .pin("Nisekoi".to_owned(), overlapping, FIRST, entry_episodes)
                .await;

            assert!(
                matches!(
                    &refused,
                    Err(Refused::Overlaps { asked, pinned }) if *asked == overlapping && *pinned == the_sequel_listed()
                ),
                "{overlapping:?}: {refused:?}"
            );
        }
        assert_eq!(pinned_in(&store), [the_sequel_pinned(&store)]);
    }

    #[tokio::test]
    async fn a_pin_of_other_episodes_or_of_another_show_stands_beside_it() {
        let (_home, store, _current, mapping) = a_mapping();
        let asked = [
            ("Nisekoi", run(13, 24), SEQUEL, run(1, 12)),
            ("Nisekoi", run(1, 12), FIRST, run(1, 12)),
            ("Nisekoi:", run(13, 24), FIRST, run(1, 12)),
            ("Nisekoi:", run(25, 30), FIRST, run(1, 6)),
            ("Nisekoi:", run(5, 10), FIRST, run(1, 6)),
        ];

        for (show, episodes, entry, entry_episodes) in asked {
            let made = mapping
                .pin(show.to_owned(), episodes, entry, entry_episodes)
                .await;

            assert!(
                matches!(&made, Ok(made) if made.replaced.is_none()),
                "{show} {episodes:?}: {made:?}"
            );
        }
        assert_eq!(pinned_in(&store).len(), 5);
    }

    #[tokio::test]
    async fn a_pin_of_the_same_episodes_takes_the_place_of_the_one_before_and_says_so() {
        let (_home, store, _current, mapping) = a_mapping();
        mapping
            .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
            .await
            .expect("the episodes are pinned");

        let again = mapping
            .pin("Nisekoi".to_owned(), run(13, 24), FIRST, run(1, 12))
            .await
            .expect("the episodes are pinned");

        assert_eq!(again.replaced, Some(the_sequel_listed()));
        assert_eq!(
            pinned_in(&store),
            [
                Pin::new(show_of(&store, FIRST), run(13, 24), FIRST, run(1, 12))
                    .expect("runs of one length")
            ]
        );
    }

    #[tokio::test]
    async fn a_pin_made_again_as_it_was_replaces_nothing() {
        let (_home, store, _current, mapping) = a_mapping();
        for _twice in 0..2 {
            let made = mapping
                .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
                .await
                .expect("the episodes are pinned");

            assert_eq!(made.replaced, None);
        }
        assert_eq!(pinned_in(&store), [the_sequel_pinned(&store)]);
    }

    #[tokio::test]
    async fn pinned_episodes_taken_back_are_gone() {
        let (_home, store, _current, mapping) = a_mapping();
        mapping
            .pin("Nisekoi".to_owned(), run(13, 24), SEQUEL, run(1, 12))
            .await
            .expect("the episodes are pinned");

        let taken = mapping.unpin("nisekoi".to_owned(), run(13, 24)).await;
        let again = mapping.unpin("Nisekoi".to_owned(), run(13, 24)).await;

        assert_eq!(
            taken.expect("the store answers"),
            Some("Nisekoi".to_owned())
        );
        assert_eq!(again.expect("the store answers"), None);
        assert_eq!(pinned_in(&store), []);
    }

    #[tokio::test]
    async fn episodes_taken_back_of_a_show_no_title_names_are_refused() {
        let (_home, _store, _current, mapping) = a_mapping();

        let refused = mapping.unpin("Another Show".to_owned(), run(1, 12)).await;

        assert!(
            matches!(&refused, Err(Refused::NoShow(named)) if named == "Another Show"),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn the_pins_are_listed_with_their_entry_s_title_and_none_once_it_left_the_list() {
        let (_home, store, _current, mapping) = a_mapping();
        for (show, entry) in [("Nisekoi", SEQUEL), ("Nisekoi:", FIRST)] {
            mapping
                .pin(show.to_owned(), run(13, 24), entry, run(1, 12))
                .await
                .expect("the episodes are pinned");
        }
        store
            .lock()
            .expect("the store")
            .file(listing::BACKEND, &[an_entry(FIRST, "Nisekoi")])
            .expect("the list is filed");

        let pins = mapping.pins().await.expect("the store answers");

        assert_eq!(
            pins,
            [
                Pinned {
                    title: None,
                    ..the_sequel_listed()
                },
                Pinned {
                    show: "Nisekoi:".to_owned(),
                    episodes: run(13, 24),
                    entry: FIRST,
                    entry_episodes: run(1, 12),
                    title: Some("Nisekoi".to_owned()),
                },
            ]
        );
    }
}
