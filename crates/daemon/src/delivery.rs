//! What delivery would do with each operation of the log, as a dry run says
//! it and sends nothing.
//!
//! An operation past the cursor goes to an episode of an entry of the list,
//! as [`resolve`](fn@resolve) decides from the pins, the list and the rules of
//! `anime-relations`, and [`plan`] decides what seeing that episode writes to
//! the entry. The operations of one entry are planned in the order of the
//! log, each from what the one before would leave, as sending them one by one
//! would leave the entry.
//!
//! **The dry run reads the list as the store last filed it, and no
//! service.** It sends nothing and needs no token. The store keeps no dates
//! of an entry, so a start date planned from what it holds is one the list
//! may hold already, and [`Action::Write`] says so.
//!
//! An operation that cannot be sent says why: it goes to no entry, the entry
//! it goes to left the list, or its row holds no operation delivery reads. A
//! rule that names an entry no list holds adds the entry, and an entry that
//! left the list is not added to it again.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use benshi_core::ShowId;
use benshi_core::list::{Entry, EntryId};
use benshi_core::pin::Pin;
use benshi_store::Store;
use benshi_store::delivery::Pending;
use benshi_store::queue::Kind;
use benshi_sync::plan::{Day, Plan, Standing, Why, plan};
use benshi_sync::relations::{self, Relations, Service};
use benshi_sync::resolve::{self, By, Listed, Resolution, Target, Unresolved, resolve};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use crate::day::day_in;
use crate::detection::POISONED_STORE;
use crate::listing::BACKEND;

/// What delivery would do with every operation past the cursor, by the list
/// as the store last filed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRun {
    /// How many entries the list held when the store last filed it.
    pub entries: usize,
    /// Every operation past the cursor, lowest first.
    pub operations: Vec<Rehearsed>,
}

/// An operation of the log, and what delivery would do with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rehearsed {
    /// The operation's number in the log.
    pub id: i64,
    /// What it says was watched, and none where its row holds no operation
    /// delivery reads.
    pub watched: Option<Watched>,
    /// What delivery would do with it.
    pub action: Action,
}

/// What an operation says was watched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    /// The day it registered, in the time zone of the machine.
    pub day: Day,
    /// The show, by the title the store files it under.
    pub title: String,
    /// The episode in the release's numbering, and none for a name that
    /// spells none.
    pub episode: Option<u32>,
}

/// What delivery would do with one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Write the entry as `after` holds it.
    Write {
        /// The episode of the entry the operation goes to.
        to: Destination,
        /// The entry before the write, and none where no list holds it.
        before: Option<Standing>,
        /// The entry as the write leaves it.
        after: Standing,
        /// Whether `before` is what the store holds, which keeps no dates, so
        /// that the list may hold a start date the write keeps in place of the
        /// one in `after`.
        undated: bool,
    },
    /// Write nothing, and why.
    Nothing {
        /// The episode of the entry the operation goes to.
        to: Destination,
        /// Why nothing is written.
        why: Why,
    },
    /// Send nothing, and why.
    CannotSend {
        /// The episode of the entry the operation goes to, where it goes to
        /// one.
        to: Option<Destination>,
        /// Why it cannot be sent.
        why: Unsent,
    },
}

/// The episode of an entry an operation goes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    /// The entry, by the number the service gives the work.
    pub entry: EntryId,
    /// The episode, in the entry's numbering.
    pub episode: u32,
    /// What decided it.
    pub by: By,
    /// The title the list gives the entry, where the list holds it and it is
    /// another entry than the show's own.
    pub title: Option<String>,
    /// How many episodes the entry has, where the list holds a count.
    pub episodes: Option<u32>,
    /// Whether the list holds the entry.
    pub listed: bool,
}

/// Why an operation cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unsent {
    /// The episode goes to no entry.
    Unresolved(Unresolved),
    /// The entry it goes to left the list.
    Departed,
    /// The row holds no operation delivery reads, and why, as a person reads
    /// it.
    Unread(String),
}

/// Why a dry run was not answered.
#[derive(Debug, thiserror::Error)]
pub enum Unanswered {
    /// The store refused to read what it holds.
    #[error("store refused: {0}")]
    Store(#[from] benshi_store::Error),
    /// The store could not be asked at all, as when the daemon is stopping.
    #[error("store could not be asked: {0}")]
    NotAsked(String),
}

/// Delivery over the store, naming the day of a viewing in a time zone.
#[derive(Debug, Clone)]
pub struct Delivery {
    store: Arc<Mutex<Store>>,
    zone: TimeZone,
}

impl Delivery {
    /// Delivery over `store`, naming the day of a viewing in `zone`.
    #[must_use]
    pub fn new(store: Arc<Mutex<Store>>, zone: TimeZone) -> Self {
        Self { store, zone }
    }

    /// What sending every operation past the cursor would do, by the list as
    /// the store last filed it. Nothing is sent, and nothing the store holds
    /// changes.
    ///
    /// # Errors
    ///
    /// [`Unanswered::Store`] where the store cannot read what it holds, and
    /// [`Unanswered::NotAsked`] where it cannot be asked.
    ///
    /// # Panics
    ///
    /// If a panic elsewhere poisoned the store, and with the panic of a call
    /// into the store, handed on as it was raised.
    pub async fn dry_run(&self) -> Result<DryRun, Unanswered> {
        let store = Arc::clone(&self.store);
        let read = tokio::task::spawn_blocking(move || {
            let store = store.lock().expect(POISONED_STORE);
            let kept = Kept {
                listed: store.listed(BACKEND)?,
                departed: store.departed(BACKEND)?,
                pins: store.pins(BACKEND)?,
            };
            Ok::<_, benshi_store::Error>((store.pending(BACKEND)?, kept))
        });

        let (pending, kept) = match read.await {
            Ok(read) => read?,
            // Handed on as it was raised.
            Err(panicked) if panicked.is_panic() => {
                std::panic::resume_unwind(panicked.into_panic())
            }
            Err(given_up) => return Err(Unanswered::NotAsked(given_up.to_string())),
        };
        Ok(rehearse(pending, &kept, relations::bundled(), &self.zone))
    }
}

/// What the store holds of one backend's list.
#[derive(Debug, Clone, Default)]
struct Kept {
    /// Every show of the list with its entry.
    listed: Vec<(ShowId, Entry)>,
    /// Every show whose entry left the list, with that entry.
    departed: Vec<(ShowId, EntryId)>,
    /// Every pin of the backend.
    pins: Vec<Pin>,
}

/// What delivery would do with each operation, by what the store holds of
/// the list and by the rules, a viewing on the day `zone` names.
fn rehearse(pending: Vec<Pending>, kept: &Kept, rules: &Relations, zone: &TimeZone) -> DryRun {
    let mut rehearsal = Rehearsal::new(kept, rules);
    DryRun {
        entries: kept.listed.len(),
        operations: pending
            .into_iter()
            .map(|pending| rehearsal.of(pending, zone))
            .collect(),
    }
}

/// What the store holds of a list, looked up by number, and each entry as
/// the operations planned so far leave it.
struct Rehearsal<'a> {
    entries: HashMap<EntryId, &'a Entry>,
    /// The entry of each show, listed or departed.
    own: HashMap<ShowId, EntryId>,
    departed: HashSet<EntryId>,
    pins: &'a [Pin],
    rules: &'a Relations,
    left: HashMap<EntryId, Standing>,
}

impl<'a> Rehearsal<'a> {
    fn new(kept: &'a Kept, rules: &'a Relations) -> Self {
        Self {
            entries: kept
                .listed
                .iter()
                .map(|(_, entry)| (entry.id, entry))
                .collect(),
            own: kept
                .listed
                .iter()
                .map(|(show, entry)| (*show, entry.id))
                .chain(kept.departed.iter().copied())
                .collect(),
            departed: kept.departed.iter().map(|&(_, entry)| entry).collect(),
            pins: &kept.pins,
            rules,
            left: HashMap::new(),
        }
    }

    /// What delivery would do with one operation.
    fn of(&mut self, pending: Pending, zone: &TimeZone) -> Rehearsed {
        let id = pending.id;
        let unread = |why: String| Rehearsed {
            id,
            watched: None,
            action: Action::CannotSend {
                to: None,
                why: Unsent::Unread(why),
            },
        };
        let operation = match pending.operation {
            Ok(operation) => operation,
            Err(unread_as) => return unread(unread_as.to_string()),
        };
        let episode = match operation.kind {
            Kind::Progress { episode } => episode,
            Kind::Rewatch { .. } => {
                return unread(format!(
                    "queued operation {id} is a rewatch, and only a progress is sent"
                ));
            }
            Kind::Entry { .. } => {
                return unread(format!(
                    "queued operation {id} is a whole entry, and only a progress is sent"
                ));
            }
        };
        let Some(day) = day_in(operation.at, zone) else {
            return unread(format!(
                "queued operation {id} was queued at an instant no day of this time zone holds"
            ));
        };

        Rehearsed {
            id,
            action: self.action(operation.show, episode, day),
            watched: Some(Watched {
                day,
                title: operation.title,
                episode,
            }),
        }
    }

    /// What delivery would do with this episode of `show`, seen on `day`.
    fn action(&mut self, show: ShowId, episode: Option<u32>, day: Day) -> Action {
        let own = self.own.get(&show).copied();
        let listed = own.map(|entry| Listed {
            entry,
            episodes: self.entries.get(&entry).and_then(|listed| listed.episodes),
        });
        let pinned = episode.and_then(|episode| self.pinned(show, episode));

        match resolve(
            show,
            episode,
            listed,
            pinned.as_slice(),
            self.rules,
            Service::AniList,
        ) {
            Resolution::Resolved { to, by } => self.write(to, by, own, day),
            Resolution::Unresolved(why) => Action::CannotSend {
                to: None,
                why: if own.is_some_and(|entry| self.departed.contains(&entry)) {
                    Unsent::Departed
                } else {
                    Unsent::Unresolved(why)
                },
            },
        }
    }

    /// The pin of `show` that takes `episode`, as one episode of its entry.
    fn pinned(&self, show: ShowId, episode: u32) -> Option<resolve::Pin> {
        self.pins.iter().find_map(|pin| {
            let run = pin.episodes();
            (pin.show() == show && (run.first()..=run.last()).contains(&episode)).then(|| {
                resolve::Pin {
                    show,
                    episode: Some(episode),
                    to: Target {
                        entry: pin.entry(),
                        episode: pin.entry_episodes().first() + (episode - run.first()),
                    },
                }
            })
        })
    }

    /// What seeing episode `to` on `day` writes, from what the operations
    /// planned so far leave of the entry, or what the store holds of it.
    fn write(&mut self, to: Target, by: By, own: Option<EntryId>, day: Day) -> Action {
        let listed = self.entries.get(&to.entry).copied();
        let destination = Destination {
            entry: to.entry,
            episode: to.episode,
            by,
            title: listed
                .filter(|_| own != Some(to.entry))
                .map(|entry| entry.title.clone()),
            episodes: listed.and_then(|entry| entry.episodes),
            listed: listed.is_some(),
        };
        if listed.is_none() && self.departed.contains(&to.entry) {
            return Action::CannotSend {
                to: Some(destination),
                why: Unsent::Departed,
            };
        }

        let (before, undated) = match self.left.get(&to.entry) {
            Some(left) => (Some(*left), false),
            None => (listed.map(held), listed.is_some()),
        };
        match plan(before.as_ref(), destination.episodes, to.episode, day) {
            Plan::Write(after) => {
                self.left.insert(to.entry, after);
                Action::Write {
                    to: destination,
                    before,
                    after,
                    undated,
                }
            }
            Plan::Nothing(why) => Action::Nothing {
                to: destination,
                why,
            },
        }
    }
}

/// Where the account stands on an entry as the store holds it, which keeps
/// no dates.
fn held(entry: &Entry) -> Standing {
    Standing {
        status: entry.status,
        progress: entry.progress,
        rewatches: entry.rewatches,
        started: None,
        completed: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    use benshi_core::ShowId;
    use benshi_core::list::{Entry, EntryId, Format, Status};
    use benshi_core::path::RawPath;
    use benshi_core::pin::{Episodes, Pin};
    use benshi_store::delivery::Pending;
    use benshi_store::queue::{Kind, Operation};
    use benshi_store::{Error, Store, Viewing};
    use benshi_sync::plan::{Day, Standing, Why};
    use benshi_sync::relations::Relations;
    use benshi_sync::resolve::{By, Unresolved};
    use jiff::tz::{Offset, TimeZone};
    use tempfile::TempDir;

    use super::{
        Action, Delivery, Destination, DryRun, Kept, Rehearsed, Unsent, Watched, rehearse,
    };
    use crate::listing::BACKEND;

    /// The show every operation here is of.
    const SHOW: ShowId = ShowId(1);

    /// The entry the list holds of [`SHOW`].
    const ENTRY: EntryId = EntryId(12345);

    /// Another entry, which a rule or a pin sends an episode to.
    const SECOND: EntryId = EntryId(900_001);

    /// The instant this RFC 3339 text names.
    fn at(text: &str) -> SystemTime {
        SystemTime::from(
            text.parse::<jiff::Timestamp>()
                .unwrap_or_else(|failed| panic!("{text}: {failed}")),
        )
    }

    /// Noon, UTC, on this day of November 2026.
    fn noon(day: u8) -> SystemTime {
        at(&format!("2026-11-{day:02}T12:00:00Z"))
    }

    const fn november(day: u8) -> Day {
        Day {
            year: 2026,
            month: 11,
            day,
        }
    }

    /// A progress of this episode of [`SHOW`], queued at `at`.
    fn seen_at(id: i64, episode: Option<u32>, at: SystemTime) -> Pending {
        Pending {
            id,
            operation: Ok(Operation {
                id,
                show: SHOW,
                title: "Show Title".to_owned(),
                kind: Kind::Progress { episode },
                at,
            }),
        }
    }

    /// A progress of this episode of [`SHOW`], queued at noon on this day of
    /// November.
    fn seen(id: i64, episode: Option<u32>, day: u8) -> Pending {
        seen_at(id, episode, noon(day))
    }

    /// What [`seen`] says was watched.
    fn watched(episode: Option<u32>, day: u8) -> Watched {
        Watched {
            day: november(day),
            title: "Show Title".to_owned(),
            episode,
        }
    }

    /// An entry of `episodes` episodes, where the account stands at `status`
    /// and `progress`.
    fn an_entry(
        id: EntryId,
        title: &str,
        episodes: Option<u32>,
        status: Status,
        progress: u32,
    ) -> Entry {
        Entry {
            id,
            title: title.to_owned(),
            spellings: Vec::new(),
            format: Some(Format::Tv),
            episodes,
            episode_length: None,
            year: None,
            released: true,
            status,
            progress,
            rewatches: 0,
            score: None,
        }
    }

    /// The list holding [`SHOW`] as [`ENTRY`], twelve episodes, where the
    /// account stands at `status` and `progress`.
    fn listed(status: Status, progress: u32) -> Kept {
        Kept {
            listed: vec![(
                SHOW,
                an_entry(ENTRY, "Show Title", Some(12), status, progress),
            )],
            ..Kept::default()
        }
    }

    /// [`SECOND`] on the list as a show of its own, twelve episodes, planned.
    fn the_second() -> (ShowId, Entry) {
        (
            ShowId(2),
            an_entry(SECOND, "Show Title Season 2", Some(12), Status::Planned, 0),
        )
    }

    /// A file of these rule lines.
    fn rules(lines: &str) -> Relations {
        let text = format!("::meta\n- version: 1.3.0\n::rules\n{lines}\n");
        Relations::read(&text).unwrap_or_else(|refused| panic!("{refused}: {text}"))
    }

    /// The rule that sends episodes 13 to 24 of [`ENTRY`] to episodes 1 to
    /// 12 of [`SECOND`].
    fn a_second_season() -> Relations {
        rules("- ?|?|12345:13-24 -> ?|?|900001:1-12")
    }

    fn run(first: u32, last: u32) -> Episodes {
        Episodes::new(first, last).expect("a run")
    }

    /// What delivery would do with each operation, a viewing on the day UTC
    /// names.
    fn rehearsed(pending: Vec<Pending>, kept: &Kept, rules: &Relations) -> Vec<Rehearsed> {
        rehearse(pending, kept, rules, &TimeZone::UTC).operations
    }

    /// This episode of [`ENTRY`], as its own show reaches it on the list.
    fn of_the_entry(episode: u32) -> Destination {
        Destination {
            entry: ENTRY,
            episode,
            by: By::Entry,
            title: None,
            episodes: Some(12),
            listed: true,
        }
    }

    const fn standing(
        status: Status,
        progress: u32,
        rewatches: u32,
        started: Option<Day>,
        completed: Option<Day>,
    ) -> Standing {
        Standing {
            status,
            progress,
            rewatches,
            started,
            completed,
        }
    }

    #[test]
    fn an_episode_of_an_entry_planned_writes_it_watching_from_that_day() {
        let operations = rehearsed(
            vec![seen(41, Some(3), 1)],
            &listed(Status::Planned, 0),
            &rules(""),
        );

        assert_eq!(
            operations,
            [Rehearsed {
                id: 41,
                watched: Some(watched(Some(3), 1)),
                action: Action::Write {
                    to: of_the_entry(3),
                    before: Some(standing(Status::Planned, 0, 0, None, None)),
                    after: standing(Status::Watching, 3, 0, Some(november(1)), None),
                    undated: true,
                },
            }]
        );
    }

    #[test]
    fn the_operations_of_one_entry_are_planned_each_from_what_the_one_before_leaves() {
        let operations = rehearsed(
            vec![
                seen(41, Some(3), 1),
                seen(42, Some(3), 2),
                seen(43, Some(12), 2),
            ],
            &listed(Status::Planned, 0),
            &rules(""),
        );

        assert_eq!(
            operations[1].action,
            Action::Nothing {
                to: of_the_entry(3),
                why: Why::Conflict { progress: 3 },
            }
        );
        assert_eq!(
            operations[2].action,
            Action::Write {
                to: of_the_entry(12),
                before: Some(standing(Status::Watching, 3, 0, Some(november(1)), None)),
                after: standing(
                    Status::Completed,
                    12,
                    0,
                    Some(november(1)),
                    Some(november(2))
                ),
                undated: false,
            }
        );
    }

    #[test]
    fn a_rewatch_counts_on_from_the_rewatches_the_list_holds() {
        let mut kept = listed(Status::Completed, 12);
        kept.listed[0].1.rewatches = 1;

        let operations = rehearsed(
            vec![seen(41, Some(1), 1), seen(42, Some(12), 2)],
            &kept,
            &rules(""),
        );

        assert_eq!(
            operations[0].action,
            Action::Write {
                to: of_the_entry(1),
                before: Some(standing(Status::Completed, 12, 1, None, None)),
                after: standing(Status::Rewatching, 1, 1, None, None),
                undated: true,
            }
        );
        assert_eq!(
            operations[1].action,
            Action::Write {
                to: of_the_entry(12),
                before: Some(standing(Status::Rewatching, 1, 1, None, None)),
                after: standing(Status::Completed, 12, 2, None, None),
                undated: false,
            }
        );
    }

    #[test]
    fn a_film_seen_again_counts_one_more_rewatch() {
        let kept = Kept {
            listed: vec![(
                SHOW,
                an_entry(ENTRY, "Show Title", Some(1), Status::Completed, 1),
            )],
            ..Kept::default()
        };

        let operations = rehearsed(vec![seen(45, None, 3)], &kept, &rules(""));

        assert_eq!(
            operations[0].action,
            Action::Write {
                to: Destination {
                    episodes: Some(1),
                    ..of_the_entry(1)
                },
                before: Some(standing(Status::Completed, 1, 0, None, None)),
                after: standing(Status::Completed, 1, 1, None, None),
                undated: true,
            }
        );
    }

    #[test]
    fn a_rule_that_sends_an_episode_to_an_entry_no_list_holds_adds_it() {
        let operations = rehearsed(
            vec![seen(44, Some(14), 3), seen(45, Some(15), 4)],
            &listed(Status::Completed, 12),
            &a_second_season(),
        );

        let to = |episode| Destination {
            entry: SECOND,
            episode,
            by: By::Rule,
            title: None,
            episodes: None,
            listed: false,
        };
        assert_eq!(
            operations[0].action,
            Action::Write {
                to: to(2),
                before: None,
                after: standing(Status::Watching, 2, 0, Some(november(3)), None),
                undated: false,
            }
        );
        assert_eq!(
            operations[1].action,
            Action::Write {
                to: to(3),
                before: Some(standing(Status::Watching, 2, 0, Some(november(3)), None)),
                after: standing(Status::Watching, 3, 0, Some(november(3)), None),
                undated: false,
            }
        );
    }

    #[test]
    fn a_rule_that_sends_an_episode_to_another_entry_of_the_list_names_it() {
        let mut kept = listed(Status::Completed, 12);
        kept.listed.push(the_second());

        let operations = rehearsed(vec![seen(44, Some(14), 3)], &kept, &a_second_season());

        assert_eq!(
            operations[0].action,
            Action::Write {
                to: Destination {
                    entry: SECOND,
                    episode: 2,
                    by: By::Rule,
                    title: Some("Show Title Season 2".to_owned()),
                    episodes: Some(12),
                    listed: true,
                },
                before: Some(standing(Status::Planned, 0, 0, None, None)),
                after: standing(Status::Watching, 2, 0, Some(november(3)), None),
                undated: true,
            }
        );
    }

    #[test]
    fn a_pin_sends_an_episode_where_it_says() {
        let mut kept = listed(Status::Completed, 12);
        kept.listed.push(the_second());
        kept.pins
            .push(Pin::new(ShowId(2), run(201, 212), ENTRY, run(1, 12)).expect("a pin"));
        kept.pins
            .push(Pin::new(SHOW, run(201, 212), SECOND, run(1, 12)).expect("a pin"));

        let operations = rehearsed(
            vec![seen(46, Some(203), 4), seen(47, Some(213), 4)],
            &kept,
            &rules(""),
        );

        assert_eq!(
            operations[0].action,
            Action::Write {
                to: Destination {
                    entry: SECOND,
                    episode: 3,
                    by: By::Pin,
                    title: Some("Show Title Season 2".to_owned()),
                    episodes: Some(12),
                    listed: true,
                },
                before: Some(standing(Status::Planned, 0, 0, None, None)),
                after: standing(Status::Watching, 3, 0, Some(november(4)), None),
                undated: true,
            }
        );
        assert_eq!(
            operations[1].action,
            Action::CannotSend {
                to: None,
                why: Unsent::Unresolved(Unresolved::LaterSeason {
                    season: 2,
                    episode: 13
                }),
            }
        );
    }

    #[test]
    fn an_episode_of_a_show_no_list_holds_cannot_be_sent() {
        let operations = rehearsed(vec![seen(45, Some(2), 4)], &Kept::default(), &rules(""));

        assert_eq!(
            operations,
            [Rehearsed {
                id: 45,
                watched: Some(watched(Some(2), 4)),
                action: Action::CannotSend {
                    to: None,
                    why: Unsent::Unresolved(Unresolved::NotListed),
                },
            }]
        );
    }

    #[test]
    fn an_episode_that_goes_to_no_entry_cannot_be_sent_and_says_why() {
        let operations = rehearsed(
            vec![
                seen(47, Some(14), 5),
                seen(48, Some(201), 5),
                seen(49, None, 5),
            ],
            &listed(Status::Watching, 3),
            &rules(""),
        );

        let unsent: Vec<&Action> = operations
            .iter()
            .map(|operation| &operation.action)
            .collect();
        assert_eq!(
            unsent,
            [
                &Action::CannotSend {
                    to: None,
                    why: Unsent::Unresolved(Unresolved::Outside { episodes: Some(12) }),
                },
                &Action::CannotSend {
                    to: None,
                    why: Unsent::Unresolved(Unresolved::LaterSeason {
                        season: 2,
                        episode: 1
                    }),
                },
                &Action::CannotSend {
                    to: None,
                    why: Unsent::Unresolved(Unresolved::NoEpisode { episodes: Some(12) }),
                },
            ]
        );
    }

    #[test]
    fn an_episode_of_a_show_whose_entry_left_the_list_cannot_be_sent() {
        let kept = Kept {
            departed: vec![(SHOW, ENTRY)],
            ..Kept::default()
        };

        let operations = rehearsed(
            vec![seen(51, Some(4), 5), seen(52, None, 5)],
            &kept,
            &rules(""),
        );

        assert_eq!(
            operations[0].action,
            Action::CannotSend {
                to: Some(Destination {
                    episodes: None,
                    listed: false,
                    ..of_the_entry(4)
                }),
                why: Unsent::Departed,
            }
        );
        assert_eq!(
            operations[1].action,
            Action::CannotSend {
                to: None,
                why: Unsent::Departed,
            }
        );
    }

    #[test]
    fn a_rule_that_sends_an_episode_to_an_entry_that_left_the_list_cannot_send_it() {
        let mut kept = listed(Status::Completed, 12);
        kept.departed.push((ShowId(2), SECOND));

        let operations = rehearsed(vec![seen(44, Some(14), 3)], &kept, &a_second_season());

        assert_eq!(
            operations[0].action,
            Action::CannotSend {
                to: Some(Destination {
                    entry: SECOND,
                    episode: 2,
                    by: By::Rule,
                    title: None,
                    episodes: None,
                    listed: false,
                }),
                why: Unsent::Departed,
            }
        );
    }

    #[test]
    fn a_row_that_holds_no_operation_cannot_be_sent_and_says_what_the_store_said() {
        let operations = rehearsed(
            vec![Pending {
                id: 48,
                operation: Err(Error::Orphan { id: 48 }),
            }],
            &Kept::default(),
            &rules(""),
        );

        assert_eq!(
            operations,
            [Rehearsed {
                id: 48,
                watched: None,
                action: Action::CannotSend {
                    to: None,
                    why: Unsent::Unread(
                        "queued operation 48 is for a show that is not there".to_owned()
                    ),
                },
            }]
        );
    }

    #[test]
    fn an_operation_of_a_kind_other_than_progress_cannot_be_sent() {
        let mut rewatch = seen(49, None, 5);
        let mut entry = seen(50, None, 5);
        for (pending, kind) in [
            (&mut rewatch, Kind::Rewatch { count: 2 }),
            (
                &mut entry,
                Kind::Entry {
                    status: Status::Completed,
                    progress: 12,
                    score: None,
                    rewatches: 1,
                },
            ),
        ] {
            pending.operation.as_mut().expect("an operation").kind = kind;
        }

        let operations = rehearsed(
            vec![rewatch, entry],
            &listed(Status::Watching, 3),
            &rules(""),
        );

        let unread: Vec<(Option<&Watched>, &Action)> = operations
            .iter()
            .map(|operation| (operation.watched.as_ref(), &operation.action))
            .collect();
        assert_eq!(
            unread,
            [
                (
                    None,
                    &Action::CannotSend {
                        to: None,
                        why: Unsent::Unread(
                            "queued operation 49 is a rewatch, and only a progress is sent"
                                .to_owned()
                        ),
                    }
                ),
                (
                    None,
                    &Action::CannotSend {
                        to: None,
                        why: Unsent::Unread(
                            "queued operation 50 is a whole entry, and only a progress is sent"
                                .to_owned()
                        ),
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_viewing_is_on_the_day_the_time_zone_given_names() {
        let late = seen_at(41, Some(3), at("2026-11-01T22:30:00Z"));
        let east = TimeZone::fixed(Offset::constant(3));

        let operations =
            rehearse(vec![late], &listed(Status::Planned, 0), &rules(""), &east).operations;

        assert_eq!(operations[0].watched, Some(watched(Some(3), 2)));
        assert!(
            matches!(
                operations[0].action,
                Action::Write { after, .. } if after.started == Some(november(2))
            ),
            "{operations:?}"
        );
    }

    #[test]
    fn a_dry_run_counts_the_entries_the_store_filed() {
        let mut kept = listed(Status::Planned, 0);
        kept.listed.push(the_second());

        assert_eq!(
            rehearse(Vec::new(), &kept, &rules(""), &TimeZone::UTC),
            DryRun {
                entries: 2,
                operations: Vec::new(),
            }
        );
    }

    /// A store of its own, shared as the daemon shares one.
    fn a_store() -> (TempDir, Arc<Mutex<Store>>) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("the database opens");
        (home, Arc::new(Mutex::new(store)))
    }

    /// Record a viewing of this episode of `show` at noon on November 1.
    fn record(store: &mut Store, show: ShowId, episode: u32) {
        record_at(store, show, episode, noon(1));
    }

    /// Record a viewing of this episode of `show` at `at`.
    fn record_at(store: &mut Store, show: ShowId, episode: u32, at: SystemTime) {
        store
            .record(&Viewing {
                show,
                episode: Some(episode),
                media: RawPath::from_bytes(format!("Show Title - {episode:02}.mkv").into_bytes()),
                at,
            })
            .expect("the viewing is recorded");
    }

    /// The show the store files an entry under.
    fn show_of(store: &Store, entry: EntryId) -> ShowId {
        store
            .listed(BACKEND)
            .expect("the list reads")
            .into_iter()
            .find(|(_, listed)| listed.id == entry)
            .map(|(show, _)| show)
            .expect("the entry is filed")
    }

    #[tokio::test]
    async fn a_dry_run_answers_by_what_the_store_holds_past_the_cursor() {
        let (_home, store) = a_store();
        {
            let mut store = store.lock().expect("the store");
            store
                .file(
                    BACKEND,
                    &[an_entry(ENTRY, "Show Title", Some(12), Status::Planned, 0)],
                )
                .expect("the list is filed");
            let show = show_of(&store, ENTRY);
            record(&mut store, show, 3);
            record(&mut store, show, 4);
            store.delivered(BACKEND, 1).expect("the first is delivered");
        }

        let dry_run = Delivery::new(Arc::clone(&store), TimeZone::UTC)
            .dry_run()
            .await
            .expect("the store reads");

        assert_eq!(
            dry_run,
            DryRun {
                entries: 1,
                operations: vec![Rehearsed {
                    id: 2,
                    watched: Some(watched(Some(4), 1)),
                    action: Action::Write {
                        to: of_the_entry(4),
                        before: Some(standing(Status::Planned, 0, 0, None, None)),
                        after: standing(Status::Watching, 4, 0, Some(november(1)), None),
                        undated: true,
                    },
                }],
            }
        );
    }

    #[tokio::test]
    async fn a_dry_run_reads_the_pins_and_the_entries_that_left_the_list() {
        let (_home, store) = a_store();
        {
            let mut store = store.lock().expect("the store");
            let (_, second) = the_second();
            store
                .file(
                    BACKEND,
                    &[
                        an_entry(ENTRY, "Show Title", Some(12), Status::Watching, 2),
                        second.clone(),
                    ],
                )
                .expect("the list is filed");
            let show = show_of(&store, ENTRY);
            store
                .pin(
                    BACKEND,
                    &Pin::new(show, run(13, 24), SECOND, run(1, 12)).expect("a pin"),
                )
                .expect("the pin is kept");
            store
                .file(BACKEND, &[second])
                .expect("the list is filed without the first entry");
            record(&mut store, show, 3);
            record(&mut store, show, 14);
        }

        let dry_run = Delivery::new(Arc::clone(&store), TimeZone::UTC)
            .dry_run()
            .await
            .expect("the store reads");

        let actions: Vec<&Action> = dry_run
            .operations
            .iter()
            .map(|operation| &operation.action)
            .collect();
        assert_eq!(
            actions,
            [
                &Action::CannotSend {
                    to: Some(Destination {
                        episodes: None,
                        listed: false,
                        ..of_the_entry(3)
                    }),
                    why: Unsent::Departed,
                },
                &Action::Write {
                    to: Destination {
                        entry: SECOND,
                        episode: 2,
                        by: By::Pin,
                        title: Some("Show Title Season 2".to_owned()),
                        episodes: Some(12),
                        listed: true,
                    },
                    before: Some(standing(Status::Planned, 0, 0, None, None)),
                    after: standing(Status::Watching, 2, 0, Some(november(1)), None),
                    undated: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_dry_run_asks_the_rules_of_anime_relations_this_build_carries() {
        // A rule of the bundled file sends episodes 13 to 24 of the first
        // work to episodes 1 to 12 of the second.
        let (_home, store) = a_store();
        {
            let mut store = store.lock().expect("the store");
            store
                .file(
                    BACKEND,
                    &[an_entry(
                        EntryId(20_632),
                        "Show Title",
                        Some(12),
                        Status::Completed,
                        12,
                    )],
                )
                .expect("the list is filed");
            let show = show_of(&store, EntryId(20_632));
            record(&mut store, show, 14);
        }

        let dry_run = Delivery::new(Arc::clone(&store), TimeZone::UTC)
            .dry_run()
            .await
            .expect("the store reads");

        assert_eq!(
            dry_run.operations[0].action,
            Action::Write {
                to: Destination {
                    entry: EntryId(20_853),
                    episode: 2,
                    by: By::Rule,
                    title: None,
                    episodes: None,
                    listed: false,
                },
                before: None,
                after: standing(Status::Watching, 2, 0, Some(november(1)), None),
                undated: false,
            }
        );
    }

    #[tokio::test]
    async fn a_dry_run_names_the_day_in_the_time_zone_it_was_given() {
        let (_home, store) = a_store();
        {
            let mut store = store.lock().expect("the store");
            store
                .file(
                    BACKEND,
                    &[an_entry(ENTRY, "Show Title", Some(12), Status::Planned, 0)],
                )
                .expect("the list is filed");
            let show = show_of(&store, ENTRY);
            record_at(&mut store, show, 3, at("2026-11-01T22:30:00Z"));
        }

        let dry_run = Delivery::new(Arc::clone(&store), TimeZone::fixed(Offset::constant(3)))
            .dry_run()
            .await
            .expect("the store reads");

        assert_eq!(dry_run.operations[0].watched, Some(watched(Some(3), 2)));
    }

    #[tokio::test]
    async fn a_dry_run_changes_nothing_the_store_holds() {
        let (_home, store) = a_store();
        {
            let mut store = store.lock().expect("the store");
            store
                .file(
                    BACKEND,
                    &[an_entry(ENTRY, "Show Title", Some(12), Status::Planned, 0)],
                )
                .expect("the list is filed");
            let show = show_of(&store, ENTRY);
            record(&mut store, show, 3);
            record(&mut store, show, 20);
        }

        Delivery::new(Arc::clone(&store), TimeZone::UTC)
            .dry_run()
            .await
            .expect("the store reads");

        let store = store.lock().expect("the store");
        let pending: Vec<i64> = store
            .pending(BACKEND)
            .expect("the log reads")
            .iter()
            .map(|pending| pending.id)
            .collect();
        assert_eq!(pending, [1, 2]);
        assert_eq!(store.aside(BACKEND).expect("the log reads").len(), 0);
        for operation in [1, 2] {
            assert_eq!(
                store.decided(BACKEND, operation).expect("the store reads"),
                None
            );
        }
    }
}
