//! The detection task: what the platform reports, filtered by policy, on the
//! bus and in the store.
//!
//! This is where the adapter, the policy table and the event bus meet, and the
//! only place the adapter meets either of the others. The adapter decides
//! nothing and does not know policy exists; the policy table is pure logic in
//! `benshi-core` and knows nothing about a bus; the filter between them is
//! here.
//!
//! Policy is applied to **readings** and never to discovery. A denied source is
//! published in the membership exactly as an admitted one is, because a source
//! nobody can see is a source nobody can diagnose, and "why is my player being
//! ignored" has to have somewhere to look.
//!
//! A round also leaves behind what it saw, in [`Seen`], which is what a client
//! asking for a listing is answered from. The same question again: a listing
//! shows what the daemon is acting on, and a second look at the platform would
//! be free to disagree with it.
//!
//! And a round decides what each admitted reading has open, leaving the last
//! decision in [`Decided`] for a client asking why. Recognition itself is pure
//! logic in `benshi-core`; what is here is the moment it is asked.
//!
//! **What was decided is what gets recorded.** Every admitted reading goes to
//! a [`Session`] with the decision about it, and what the session answers is
//! carried out in the store: what was watched of an episode is kept, an
//! episode watched far enough is recorded, and an episode is closed when the
//! last player that had it open has left it.
//!
//! **A player has left what it had open when a round hears nothing of it
//! that says otherwise.** A reading that policy admitted says otherwise. So
//! does a failure to answer, and so does a reading that came without a
//! description of its source: in neither can the round tell what the player
//! has open. That leaves a player no longer listed, one listed with nothing
//! open, and one policy stopped admitting.
//!
//! **A task that stops closes nothing.** Its session goes with it, and an
//! episode open in it that had registered keeps its marked total in the
//! store. The next sitting of that episode goes on from that total and
//! records nothing.
//!
//! **A store that refuses a call stops the task for good.** Detecting on
//! while nothing is recorded looks the same as working. The supervisor
//! records why the task stopped, and [`run`](crate::run) returns that record
//! only once every task has stopped.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use benshi_core::policy::PolicyTable;
use benshi_core::recognise::Recognition;
use benshi_core::session::{Effect, Resumed, Session};
use benshi_core::timeline::WatchedPolicy;
use benshi_core::{MediaRef, PlayerId, PlayerSnapshot};
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
use benshi_store::watching::Total;
use benshi_store::{Store, Viewing};
use tokio::time::MissedTickBehavior;

use crate::bus::{BusEvent, EventBus};
use crate::recognition::{Decided, Decision, Recogniser};
use crate::supervisor::TaskError;

/// What is said about a reading whose source the same round did not describe.
///
/// The adapter contract forbids this, and the daemon does not rely on that. A
/// reading it cannot attribute to an application is a reading it cannot apply
/// policy to, and publishing it would walk past a deny the user set.
const UNDESCRIBED: &str = "read but not described in the same round, so no policy applies to it";

/// What is said when the policy table cannot be trusted.
const POISONED: &str = "the policy table was poisoned by a panic in another task";

/// What is said when the record of what was seen cannot be trusted.
const POISONED_SEEN: &str = "what was last seen was poisoned by a panic in another task";

/// What is said when the store cannot be trusted.
const POISONED_STORE: &str = "the store was poisoned by a panic in a call into it";

/// Every source the last round was able to describe.
///
/// Written by the detection task at the end of each round that reached the
/// platform and had no call refused by the store. Read by a client asking for
/// a listing, so that `benshi sources` shows what the daemon is acting on.
/// Looking at the platform a second time would be fresher and could disagree
/// with what detection is publishing, and for a command whose whole job is to
/// explain the daemon, agreeing matters more than being current.
///
/// A source that did not answer is not here, because there is nothing to
/// describe. It stays in the membership the bus publishes, which is where a
/// player that has gone quiet shows up.
///
/// Cloning shares the record rather than copying it.
#[derive(Debug, Clone, Default)]
pub struct Seen(Arc<RwLock<Vec<SourceInfo>>>);

impl Seen {
    /// A record of nothing, which is what is true before the first round.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the record with what this round found.
    ///
    /// # Panics
    ///
    /// If a panic elsewhere poisoned the record. A listing assembled from what
    /// a panicking task left behind is one nobody can reason about, and the
    /// supervisor records the panic and restarts.
    pub fn record(&self, sources: Vec<SourceInfo>) {
        *self.0.write().expect(POISONED_SEEN) = sources;
    }

    /// The sources of the last round, and none before the first has run.
    ///
    /// # Panics
    ///
    /// If a panic elsewhere poisoned the record, for the same reason.
    #[must_use]
    pub fn sources(&self) -> Vec<SourceInfo> {
        self.0.read().expect(POISONED_SEEN).clone()
    }
}

/// Which class a failed round belongs to.
///
/// Every failure is transient, and the match is exhaustive so that a fourth
/// variant has to state its own answer here instead of inheriting one. Nothing
/// is permanent because no platform failure is improved by stopping: a daemon
/// that gave up watching looks exactly like one where nothing is playing, and
/// the user finds out hours later that nothing was recorded.
///
/// This is the class of a whole round, which is not the class of one source. A
/// source that has gone is permanent for that source and is reported per
/// source; the same fact reaching this function says the platform could not be
/// read at all, and that is worth another attempt.
fn classify(error: WatchError) -> TaskError {
    match error {
        WatchError::Timeout { .. } | WatchError::Transport(_) | WatchError::Unavailable(_) => {
            TaskError::Transient(error.into())
        }
    }
}

/// What a detection task is wired to: everything it reads and writes that
/// outlives it.
///
/// A task that is started again builds a watcher and a [`Detection`] of its
/// own and is handed this again, so what one attempt left is there for the
/// next. Cloning shares every part but `now` and `period`, which are copied.
#[derive(Debug, Clone)]
pub struct Wiring {
    /// Where readings and the membership are published.
    pub bus: Arc<EventBus>,
    /// Which sources a reading is admitted from. Shared rather than copied,
    /// so that setting a policy changes what a running loop publishes without
    /// restarting it.
    pub policy: Arc<RwLock<PolicyTable>>,
    /// Where each round leaves the sources it described.
    pub seen: Seen,
    /// What a reading's file is decided against.
    pub recogniser: Arc<Recogniser>,
    /// Where each round leaves its last decision.
    pub decided: Decided,
    /// Where what was watched is kept and an episode is recorded.
    pub store: Arc<Mutex<Store>>,
    /// The time of day, which the store writes beside what it keeps.
    pub now: fn() -> SystemTime,
    /// How long a round is from the one before it.
    pub period: Duration,
}

/// Make one call into the store, on a thread that may block.
///
/// The store is locked on that thread and for the one call, so the lock is
/// never held across an await.
///
/// # Errors
///
/// [`TaskError::Permanent`] when the store refuses the call, saying what the
/// store said.
///
/// # Panics
///
/// If the call panicked, or if a panic in an earlier call poisoned the store.
/// Either is a bug, and the supervisor records the panic and restarts the
/// task. A poisoned store stays poisoned, so a restarted task panics again at
/// its first call into it.
async fn ask<T, C>(store: Arc<Mutex<Store>>, call: C) -> Result<T, TaskError>
where
    T: Send + 'static,
    C: FnOnce(&mut Store) -> Result<T, benshi_store::Error> + Send + 'static,
{
    let called = tokio::task::spawn_blocking(move || {
        let mut store = store.lock().expect(POISONED_STORE);
        call(&mut store)
    });

    match called.await {
        Ok(answered) => answered.map_err(|refused| TaskError::Permanent(refused.into())),
        // Nothing aborts this handle, and a runtime cancels a blocking call
        // only by shutting down before the call starts. The daemon's runtime
        // shuts down after its last task has ended, so a call that did not
        // come back panicked, and the panic is handed on as it was raised.
        Err(panicked) => std::panic::resume_unwind(panicked.into_panic()),
    }
}

/// Readings from a platform, filtered by policy, published to the bus and
/// recorded in the store.
#[derive(Debug)]
pub struct Detection<W> {
    watcher: W,
    bus: Arc<EventBus>,
    policy: Arc<RwLock<PolicyTable>>,
    seen: Seen,
    recogniser: Arc<Recogniser>,
    decided: Decided,
    store: Arc<Mutex<Store>>,
    now: fn() -> SystemTime,
    period: Duration,
    listed: BTreeSet<PlayerId>,
    session: Session,
    /// The players the session was handed a reading of and has not been
    /// told have left.
    heard: BTreeSet<PlayerId>,
}

impl<W: PlayerWatcher> Detection<W> {
    /// Detection over one platform, wired to what outlives it.
    ///
    /// Nothing is open in the session it begins with. What was watched
    /// before it began comes back from the store, at the first reading of
    /// each episode.
    #[must_use]
    pub fn new(watcher: W, wiring: Wiring) -> Self {
        let Wiring {
            bus,
            policy,
            seen,
            recogniser,
            decided,
            store,
            now,
            period,
        } = wiring;

        Self {
            watcher,
            bus,
            policy,
            seen,
            recogniser,
            decided,
            store,
            now,
            period,
            listed: BTreeSet::new(),
            session: Session::new(WatchedPolicy::default()),
            heard: BTreeSet::new(),
        }
    }

    /// A round every `period`, for ever.
    ///
    /// The first round happens at once rather than after a wait, so a daemon
    /// that has just started publishes what is playing instead of nothing.
    ///
    /// A round that overruns delays the next one rather than being followed
    /// immediately by another: rounds that ran back to back to catch up would
    /// spend a struggling platform's remaining capacity on the backlog.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError::Transient`] when a round cannot reach the
    /// platform. The supervisor retries with backoff, which for a session bus
    /// that went away is the right answer and the only one. Returns
    /// [`TaskError::Permanent`] when the store refuses a call, and the
    /// supervisor leaves the task stopped.
    pub async fn run(&mut self) -> Result<(), TaskError> {
        let mut ticker = tokio::time::interval(self.period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            self.round().await?;
        }
    }

    /// One round: read the platform, apply policy, record and publish what
    /// was admitted, and let go of whoever has left.
    async fn round(&mut self) -> Result<(), TaskError> {
        let PollOutcome {
            sources,
            snapshots,
            failures,
        } = self.watcher.poll().await.map_err(classify)?;

        let described: BTreeSet<&PlayerId> = sources.iter().map(|source| &source.player).collect();

        // A source that did not answer is still on the platform: it failed to
        // speak, and it did not close. Leaving it out of the membership would
        // report a player that timed out as one that had gone, and report it
        // back the round after.
        let listed: BTreeSet<PlayerId> = described
            .iter()
            .copied()
            .chain(failures.iter().map(|(player, _reason)| player))
            .cloned()
            .collect();
        self.publish_membership(&listed);

        // Whoever the session was handed a reading of may have an episode
        // open, and has left it unless this round hears otherwise. A player
        // that did not answer has not left: it failed to speak.
        let mut quiet = self.heard.clone();

        for (player, reason) in failures {
            quiet.remove(&player);
            self.bus.publish(BusEvent::SourceFailed {
                player,
                reason: reason.to_string(),
            });
        }

        // Three answers to a reading: publish it when policy admits its source,
        // drop it without a word when policy denies it, and report it when this
        // round described no source it could have come from.
        let admitted = self.admitted(&sources);

        for snapshot in snapshots {
            if admitted.contains(&snapshot.player) {
                quiet.remove(&snapshot.player);
                if !self.heard.contains(&snapshot.player) {
                    self.heard.insert(snapshot.player.clone());
                }
                let decision = self.decide(&snapshot);
                let effects = self.session.advance(&snapshot, decision.as_ref());
                self.carry_out(effects).await?;
                self.bus.publish(BusEvent::Snapshot(snapshot));
            } else if !described.contains(&snapshot.player) {
                // The round cannot tell what this player has open, so the
                // player is kept in.
                quiet.remove(&snapshot.player);
                self.bus.publish(BusEvent::SourceFailed {
                    player: snapshot.player,
                    reason: UNDESCRIBED.to_owned(),
                });
            }
        }

        // After the readings, so that a player let go here does not close an
        // episode that another player opened in this round.
        self.heard.retain(|player| !quiet.contains(player));
        let quiet: Vec<PlayerId> = quiet.into_iter().collect();
        let effects = self.session.gone(&quiet);
        self.carry_out(effects).await?;

        self.seen.record(sources);

        Ok(())
    }

    /// Publish this round's membership when it differs from the last round's.
    ///
    /// Only when it differs. The bus is the log a person reads, and a line
    /// every second saying what the last one said is how a log stops being
    /// read.
    fn publish_membership(&mut self, listed: &BTreeSet<PlayerId>) {
        if *listed != self.listed {
            self.listed.clone_from(listed);
            self.bus.publish(BusEvent::SourcesChanged {
                sources: listed.iter().cloned().collect(),
            });
        }
    }

    /// Decide what an admitted reading has open, leave the decision where a
    /// client can read it, and answer with it.
    ///
    /// A file and nothing else. An address names no file on this machine, and
    /// a title with nothing underneath it is what a browser publishes;
    /// recognition reads filenames, and neither is one, so neither is answered
    /// for. Every admitted reading of a file is decided, every round, so that
    /// what a client reads is the decision about the reading the daemon last
    /// acted on.
    fn decide(&self, snapshot: &PlayerSnapshot) -> Option<Recognition> {
        let MediaRef::LocalFile(path) = &snapshot.media else {
            return None;
        };
        let (parsed, answer) = self.recogniser.decide(path);

        self.decided.record(Decision {
            player: snapshot.player.clone(),
            media: snapshot.media.clone(),
            parsed,
            answer: answer.clone(),
        });

        Some(answer)
    }

    /// Carry out what the session answered, in the order it answered, and
    /// bring the answer to a question back to it before anything else.
    ///
    /// The time of day is read for each call that writes one, as the call is
    /// made.
    async fn carry_out(&mut self, effects: Vec<Effect>) -> Result<(), TaskError> {
        for effect in effects {
            match effect {
                Effect::Resume {
                    question,
                    title,
                    episode,
                } => {
                    let kept = ask(Arc::clone(&self.store), move |store| {
                        store.resume(&title, episode)
                    })
                    .await?;
                    self.session.resumed(
                        question,
                        kept.map(|kept| Resumed {
                            watched: kept.watched,
                            registered: kept.registered,
                        }),
                    );
                }
                Effect::Keep {
                    title,
                    episode,
                    media,
                    watched,
                } => {
                    let total = Total {
                        title,
                        episode,
                        media,
                        watched,
                        at: (self.now)(),
                    };
                    ask(Arc::clone(&self.store), move |store| store.keep(&total)).await?;
                }
                Effect::Record {
                    title,
                    episode,
                    media,
                } => {
                    let viewing = Viewing {
                        title,
                        episode,
                        media,
                        at: (self.now)(),
                    };
                    ask(Arc::clone(&self.store), move |store| store.record(&viewing)).await?;
                }
                Effect::Close { title, episode } => {
                    ask(Arc::clone(&self.store), move |store| {
                        store.close(&title, episode)
                    })
                    .await?;
                }
            }
        }

        Ok(())
    }

    /// Which of this round's sources policy admits readings from.
    ///
    /// The table is read once for the whole round rather than once per source,
    /// and the guard does not outlive the call, so nothing is published and
    /// nothing is awaited while the table is locked.
    ///
    /// # Panics
    ///
    /// If the table was poisoned by a panic elsewhere. What a poisoned table
    /// says about policy cannot be trusted, and carrying on would risk
    /// publishing a source the user denied. The supervisor records the panic
    /// and restarts the task.
    fn admitted<'round>(&self, sources: &'round [SourceInfo]) -> BTreeSet<&'round PlayerId> {
        let table = self.policy.read().expect(POISONED);

        sources
            .iter()
            .filter(|source| table.policy_for(&source.app).admits())
            .map(|source| &source.player)
            .collect()
    }
}

#[cfg(test)]
// A fake answers from memory, which is the whole point of a fake, and the trait
// asks for a future either way. The `async` therefore stays where there is
// nothing to await, rather than being spelled out as the future it desugars to.
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use super::{Detection, Seen, Wiring};
    use crate::bus::{BusEvent, EventBus};
    use crate::recognition::{Decided, Recogniser};
    use crate::supervisor::TaskError;
    use benshi_core::clock::{Clock, TestClock, Timestamp};
    use benshi_core::path::RawPath;
    use benshi_core::policy::{Policy, PolicyTable};
    use benshi_core::recognise::Recognition;
    use benshi_core::recognise::altname::Altnames;
    use benshi_core::recognise::parse::Episode;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };
    use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
    use benshi_store::queue::{Kind, Operation};
    use benshi_store::watching::{Kept, Total};
    use benshi_store::{Store, Viewing};
    use std::collections::{BTreeSet, VecDeque};
    use std::ops::RangeInclusive;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;
    use tokio::sync::broadcast::Receiver;

    /// The period a test drives the loop at.
    ///
    /// Every test that lets the loop tick pauses the clock, so this is virtual
    /// time and no test waits for it.
    const INTERVAL: Duration = Duration::from_millis(100);

    /// The length the readings of [`playing`] report.
    const LENGTH: Duration = Duration::from_mins(4);

    /// The interval between two readings that are a step apart.
    const STEP: Duration = Duration::from_secs(5);

    /// The reading at which half of [`LENGTH`] has been watched, counted
    /// from nought.
    ///
    /// The first reading of a sitting adds nothing and each one after it adds
    /// a [`STEP`], so two minutes are watched at the twenty-fourth after it.
    const HALF_AT: u32 = 24;

    /// The title the list holds for the series.
    const SERIES: &str = "Show Title";

    /// The title the list holds for the film.
    const FILM: &str = "A Film";

    const THIRD: &str = "/anime/[Group] Show Title - 03 [1080p].mkv";
    const NOT_LISTED: &str = "/anime/[Group] Some Other Show - 03 [1080p].mkv";

    /// What [`the_time`] reads, as a store writes it.
    const THE_TIME: &str = "2033-05-18T03:33:20Z";

    /// The time of day in every test here, which a store writes beside what
    /// it keeps.
    fn the_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    fn a_source(identity: &str, app: &str) -> SourceInfo {
        SourceInfo {
            player: PlayerId(identity.to_owned()),
            app: AppName(app.to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    fn a_reading(source: &SourceInfo) -> PlayerSnapshot {
        a_reading_of(
            source,
            MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
        )
    }

    fn a_reading_of(source: &SourceInfo, media: MediaRef) -> PlayerSnapshot {
        PlayerSnapshot {
            player: source.player.clone(),
            media,
            state: source.state,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::Value(Duration::from_mins(23)),
            observed_at: Timestamp::epoch(),
        }
    }

    /// A reading of a file whose name a release would give it.
    fn a_reading_of_a_file(source: &SourceInfo, name: &str) -> PlayerSnapshot {
        a_reading_of(
            source,
            MediaRef::LocalFile(RawPath::from_bytes(name.as_bytes().to_vec())),
        )
    }

    /// A database in a directory of its own, which goes when this does.
    struct Database {
        _home: TempDir,
        path: PathBuf,
        store: Arc<Mutex<Store>>,
    }

    /// One row of `episodes_seen`, as `sqlite3` shows it.
    #[derive(Debug, PartialEq, Eq)]
    struct Viewed {
        title: String,
        episode: Option<u32>,
        media: String,
        at: String,
    }

    impl Database {
        fn new() -> Self {
            let home = tempfile::tempdir().expect("a directory of our own");
            let path = home.path().join("data").join("benshi.db");
            let store = Store::open(&path).expect("the database opens");

            Self {
                _home: home,
                path,
                store: Arc::new(Mutex::new(store)),
            }
        }

        /// What the store holds of an episode.
        fn kept(&self, title: &str, episode: Option<u32>) -> Option<Kept> {
            self.store
                .lock()
                .expect("the store")
                .resume(title, episode)
                .expect("the total reads")
        }

        /// Every operation in the queue.
        fn queued(&self) -> Vec<Operation> {
            self.store
                .lock()
                .expect("the store")
                .queued()
                .expect("the queue reads")
        }

        /// The rows a query answers, read from the file as `sqlite3` reads
        /// it.
        fn rows<T>(
            &self,
            query: &str,
            read: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
        ) -> Vec<T> {
            let connection =
                rusqlite::Connection::open(&self.path).expect("the file opens for reading");
            let mut statement = connection.prepare(query).expect("a query SQLite takes");
            statement
                .query_map([], read)
                .expect("the query runs")
                .collect::<Result<_, _>>()
                .expect("every row reads")
        }

        /// Every episode recorded as seen, in the order they were recorded.
        fn seen(&self) -> Vec<Viewed> {
            self.rows(
                "SELECT shows.title, episode, media, seen_at \
                 FROM episodes_seen JOIN shows ON shows.id = episodes_seen.show \
                 ORDER BY episodes_seen.id",
                |row| {
                    Ok(Viewed {
                        title: row.get(0)?,
                        episode: row.get(1)?,
                        media: row.get(2)?,
                        at: row.get(3)?,
                    })
                },
            )
        }

        /// How many rows a table holds.
        fn count_of(&self, table: &str) -> Vec<u32> {
            self.rows(&format!("SELECT count(*) FROM {table}"), |row| row.get(0))
        }

        /// Change the file from outside the daemon, as a person with
        /// `sqlite3` can.
        fn by_hand(&self, statements: &str) {
            rusqlite::Connection::open(&self.path)
                .expect("the file opens for writing")
                .execute_batch(statements)
                .expect("the statements run");
        }

        /// Have the store refuse every statement of this kind on this table.
        fn refusing(&self, what: &str, table: &str) {
            self.by_hand(&format!(
                "CREATE TRIGGER refused BEFORE {what} ON {table} \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;"
            ));
        }
    }

    /// Two series and a film, each spelled one way, and nothing named by
    /// hand.
    fn a_list() -> Recogniser {
        Recogniser::of(
            [SERIES, "Second Series", FILM].into_iter().collect(),
            Altnames::new(),
        )
    }

    /// Wiring to this database and [`the_time`], with the table a user starts
    /// with, nothing to match a name against, and records of its own.
    fn wired_to(database: &Database) -> Wiring {
        Wiring {
            bus: Arc::new(EventBus::new()),
            policy: Arc::new(RwLock::new(PolicyTable::allowing_video_players())),
            seen: Seen::new(),
            recogniser: Arc::new(Recogniser::empty()),
            decided: Decided::new(),
            store: Arc::clone(&database.store),
            now: the_time,
            period: INTERVAL,
        }
    }

    /// Detection over these rounds, against this policy table, leaving what it
    /// saw in `seen`.
    ///
    /// Nothing to match a name against, so nothing reaches the database it is
    /// handed back with, and no decision read back: a test that reads one
    /// takes [`deciding`] instead.
    fn detecting(
        watcher: ScriptedWatcher,
        bus: Arc<EventBus>,
        policy: Arc<RwLock<PolicyTable>>,
        seen: Seen,
    ) -> (Database, Detection<ScriptedWatcher>) {
        let database = Database::new();
        let wiring = Wiring {
            bus,
            policy,
            seen,
            ..wired_to(&database)
        };

        (database, Detection::new(watcher, wiring))
    }

    /// Detection over these rounds, leaving its decisions in `decided`.
    fn deciding(
        watcher: ScriptedWatcher,
        bus: Arc<EventBus>,
        decided: Decided,
    ) -> (Database, Detection<ScriptedWatcher>) {
        let database = Database::new();
        let wiring = Wiring {
            bus,
            decided,
            ..wired_to(&database)
        };

        (database, Detection::new(watcher, wiring))
    }

    /// Detection over these rounds that decides against [`a_list`] and
    /// records in this database, with `policy` to say who is admitted.
    fn recording_under(
        watcher: ScriptedWatcher,
        database: &Database,
        policy: Arc<RwLock<PolicyTable>>,
    ) -> Detection<ScriptedWatcher> {
        let wiring = Wiring {
            policy,
            recogniser: Arc::new(a_list()),
            ..wired_to(database)
        };

        Detection::new(watcher, wiring)
    }

    /// Detection over these rounds that decides against [`a_list`] and
    /// records in this database.
    fn recording(watcher: ScriptedWatcher, database: &Database) -> Detection<ScriptedWatcher> {
        recording_under(
            watcher,
            database,
            Arc::new(RwLock::new(PolicyTable::allowing_video_players())),
        )
    }

    /// The instant this many steps after the first reading of a test.
    ///
    /// Every clock made here starts at one epoch and is moved once, so the
    /// instants compare as the instants of one clock do.
    fn after(steps: u32) -> Timestamp {
        let clock = TestClock::new();
        clock.advance(STEP * steps);

        clock.now()
    }

    /// A reading of this file being played, this many steps into a test.
    ///
    /// No position, because nothing here is about one: watched time is
    /// counted from the instants and the states.
    fn playing(source: &SourceInfo, name: &str, steps: u32) -> PlayerSnapshot {
        PlayerSnapshot {
            player: source.player.clone(),
            media: MediaRef::LocalFile(a_path(name)),
            state: PlayState::Playing,
            position: Known::NotReported,
            duration: Known::Value(LENGTH),
            observed_at: after(steps),
        }
    }

    /// A round that described these sources and took these readings.
    fn a_round_of(sources: &[&SourceInfo], snapshots: Vec<PlayerSnapshot>) -> PollOutcome {
        PollOutcome {
            sources: sources.iter().copied().cloned().collect(),
            snapshots,
            failures: Vec::new(),
        }
    }

    /// A round in which this source was listed and did not answer.
    fn a_round_without_an_answer_from(source: &SourceInfo) -> PollOutcome {
        PollOutcome {
            sources: Vec::new(),
            snapshots: Vec::new(),
            failures: vec![(
                source.player.clone(),
                WatchError::Timeout {
                    player: source.player.clone(),
                    deadline: Duration::from_millis(500),
                },
            )],
        }
    }

    /// Rounds a step apart, in each of which this source plays this file.
    fn watching(
        source: &SourceInfo,
        name: &str,
        steps: RangeInclusive<u32>,
    ) -> Vec<Result<PollOutcome, WatchError>> {
        steps
            .map(|step| Ok(a_round_of(&[source], vec![playing(source, name, step)])))
            .collect()
    }

    /// Take this many rounds, every one of which has to go well.
    async fn rounds(detection: &mut Detection<ScriptedWatcher>, count: u32) {
        for round in 0..count {
            detection
                .round()
                .await
                .unwrap_or_else(|failure| panic!("round {round} failed: {failure}"));
        }
    }

    /// What detection stops with in the round after this many went well.
    async fn stopped_after(detection: &mut Detection<ScriptedWatcher>, count: u32) -> TaskError {
        rounds(detection, count).await;

        detection
            .round()
            .await
            .expect_err("the round the store refused in")
    }

    /// The path a store answers with for a file named here.
    fn a_path(name: &str) -> RawPath {
        RawPath::from_bytes(name.as_bytes().to_vec())
    }

    /// A round in which every source answered and had something open.
    fn a_round(sources: Vec<SourceInfo>) -> PollOutcome {
        let snapshots = sources.iter().map(a_reading).collect();

        PollOutcome {
            sources,
            snapshots,
            failures: Vec::new(),
        }
    }

    /// A watcher answering from memory: the scripted rounds in order, and then
    /// a round of `then` for ever.
    ///
    /// `PollOutcome` cannot derive `Clone`, because a `WatchError` cannot, so
    /// the repeating answer is rebuilt from its sources rather than copied.
    struct ScriptedWatcher {
        scripted: VecDeque<Result<PollOutcome, WatchError>>,
        then: Vec<SourceInfo>,
    }

    impl ScriptedWatcher {
        /// Answers these rounds in order, then empty rounds for ever.
        fn of(rounds: Vec<Result<PollOutcome, WatchError>>) -> Self {
            Self {
                scripted: rounds.into(),
                then: Vec::new(),
            }
        }

        /// Answers a round describing and reading these sources, every time.
        fn repeating(sources: Vec<SourceInfo>) -> Self {
            Self {
                scripted: VecDeque::new(),
                then: sources,
            }
        }
    }

    impl PlayerWatcher for ScriptedWatcher {
        async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
            Ok(self.then.clone())
        }

        async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
            self.scripted
                .pop_front()
                .unwrap_or_else(|| Ok(a_round(self.then.clone())))
        }
    }

    /// Everything on the bus, drained without waiting.
    ///
    /// `try_recv` rather than `recv`, so a regression that stops an event being
    /// published fails the test in no time instead of hanging the run.
    fn drain(events: &mut Receiver<BusEvent>) -> Vec<BusEvent> {
        let mut seen = Vec::new();

        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }

        seen
    }

    /// The source of every reading among these events, in the order published.
    fn snapshots_in(published: &[BusEvent]) -> Vec<PlayerId> {
        published
            .iter()
            .filter_map(|event| match event {
                BusEvent::Snapshot(snapshot) => Some(snapshot.player.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every membership among these events, in the order published.
    fn listings_in(published: &[BusEvent]) -> Vec<Vec<PlayerId>> {
        published
            .iter()
            .filter_map(|event| match event {
                BusEvent::SourcesChanged { sources } => Some(sources.clone()),
                _ => None,
            })
            .collect()
    }

    fn id(identity: &str) -> PlayerId {
        PlayerId(identity.to_owned())
    }

    fn app(name: &str) -> AppName {
        AppName(name.to_owned())
    }

    #[tokio::test]
    async fn readings_from_admitted_sources_reach_the_bus() {
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![
            a_source("mpv.instance1701", "mpv"),
            a_source("vlc", "vlc"),
        ]))]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("a round");

        assert_eq!(
            snapshots_in(&drain(&mut events)),
            vec![id("mpv.instance1701"), id("vlc")]
        );
    }

    #[tokio::test]
    async fn a_denied_source_is_silenced_and_still_listed() {
        // Policy suppresses readings and never discovery. A source nobody can
        // see is a source nobody can diagnose, so the denied player stays in
        // the membership the bus publishes.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![
            a_source("firefox.instance30062", "firefox"),
            a_source("mpv", "mpv"),
        ]))]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("a round");
        let published = drain(&mut events);

        assert_eq!(
            snapshots_in(&published),
            vec![id("mpv")],
            "the denied source produced a reading"
        );
        assert!(
            !published
                .iter()
                .any(|event| matches!(event, BusEvent::SourceFailed { .. })),
            "the denied source was reported as a failure rather than silenced: {published:?}"
        );
        assert_eq!(
            listings_in(&published),
            vec![vec![id("firefox.instance30062"), id("mpv")]],
            "the denied source is missing from the membership"
        );
    }

    #[tokio::test]
    async fn policy_is_keyed_on_the_application_and_not_on_the_identity() {
        // A browser qualifies its bus name with a process id, so the identity
        // is not what the policy table is keyed on. The daemon reads the
        // application the adapter declared beside the reading.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![a_source(
            "chromium.instance16481",
            "chromium",
        )]))]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("a round");

        assert!(snapshots_in(&drain(&mut events)).is_empty());
    }

    #[tokio::test]
    async fn a_reading_the_round_did_not_describe_is_reported_and_not_published() {
        // The contract forbids this, and the daemon must not depend on that:
        // a reading it cannot attribute to an application is a reading it
        // cannot apply policy to, and publishing it would walk past a deny.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let stranger = a_source("firefox", "firefox");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: Vec::new(),
            snapshots: vec![a_reading(&stranger)],
            failures: Vec::new(),
        })]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("a round");
        let published = drain(&mut events);

        assert!(
            snapshots_in(&published).is_empty(),
            "an unattributable reading was published: {published:?}"
        );
        assert!(
            published
                .iter()
                .any(|event| matches!(event, BusEvent::SourceFailed { player, .. } if player == &id("firefox"))),
            "an unattributable reading was dropped without a word: {published:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_source_is_reported_and_the_others_continue() {
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let working = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: vec![working.clone()],
            snapshots: vec![a_reading(&working)],
            failures: vec![(
                id("vlc"),
                WatchError::Timeout {
                    player: id("vlc"),
                    deadline: Duration::from_millis(500),
                },
            )],
        })]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("a round");
        let published = drain(&mut events);

        assert_eq!(
            snapshots_in(&published),
            vec![id("mpv")],
            "one source failing blinded the daemon to the rest"
        );
        let reported = published
            .iter()
            .find_map(|event| match event {
                BusEvent::SourceFailed { player, reason } if player == &id("vlc") => Some(reason),
                _ => None,
            })
            .expect("the failure reached the bus");
        assert!(
            reported.contains("500ms"),
            "the reason does not say what happened: {reported:?}"
        );
    }

    #[tokio::test]
    async fn a_source_that_did_not_answer_stays_in_the_membership() {
        // Failing to answer is not closing. A player left out of the membership
        // for one round and put back the next reads as one that closed and
        // reopened, and a player that times out every other round would write
        // that pair of lines for as long as it ran.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let answering = a_source("mpv", "mpv");
        let silent = || {
            (
                id("vlc"),
                WatchError::Timeout {
                    player: id("vlc"),
                    deadline: Duration::from_millis(500),
                },
            )
        };
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![answering.clone(), a_source("vlc", "vlc")])),
            Ok(PollOutcome {
                sources: vec![answering.clone()],
                snapshots: vec![a_reading(&answering)],
                failures: vec![silent()],
            }),
        ]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("the first round");
        detection.round().await.expect("the round it went quiet in");

        assert_eq!(
            listings_in(&drain(&mut events)),
            vec![vec![id("mpv"), id("vlc")]],
            "a source that did not answer was reported as gone"
        );
    }

    #[tokio::test]
    async fn each_round_leaves_what_it_saw_where_a_client_can_read_it() {
        // A listing command shows what the daemon is acting on rather than a
        // second look at the platform, so the round leaves its sources behind
        // and the next round replaces them.
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let seen = Seen::new();
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![
                a_source("mpv", "mpv"),
                a_source("vlc", "vlc"),
            ])),
            Ok(a_round(vec![a_source("vlc", "vlc")])),
        ]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, seen.clone());

        assert!(
            seen.sources().is_empty(),
            "something was recorded before the first round ran"
        );

        detection.round().await.expect("the first round");
        assert_eq!(
            seen.sources(),
            vec![a_source("mpv", "mpv"), a_source("vlc", "vlc")]
        );

        detection.round().await.expect("the second round");
        assert_eq!(
            seen.sources(),
            vec![a_source("vlc", "vlc")],
            "the record grew instead of being replaced"
        );
    }

    #[tokio::test]
    async fn a_denied_source_is_recorded_so_a_listing_can_explain_it() {
        // The listing is where "why is my player being ignored" is answered, so
        // a denied source has to be in the record even though none of its
        // readings ever reach the bus.
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let seen = Seen::new();
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![a_source(
            "firefox.instance30062",
            "firefox",
        )]))]);
        let (_database, mut detection) = detecting(watcher, bus, policy, seen.clone());

        detection.round().await.expect("a round");

        assert_eq!(
            seen.sources(),
            vec![a_source("firefox.instance30062", "firefox")]
        );
    }

    #[tokio::test]
    async fn a_source_that_stopped_being_listed_leaves_the_membership() {
        // The other half of the same rule. A source that the platform no longer
        // reports at all has closed, and holding it in the membership would
        // mean nothing ever leaves.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![
                a_source("mpv", "mpv"),
                a_source("vlc", "vlc"),
            ])),
            Ok(a_round(vec![a_source("mpv", "mpv")])),
        ]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("the first round");
        detection.round().await.expect("the round after vlc closed");

        assert_eq!(
            listings_in(&drain(&mut events)),
            vec![vec![id("mpv"), id("vlc")], vec![id("mpv")]]
        );
    }

    #[tokio::test]
    async fn a_swap_is_visible_because_membership_is_published() {
        // One player closing while another opens leaves the count unchanged,
        // so a count cannot see it. The membership can.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![a_source("mpv", "mpv")])),
            Ok(a_round(vec![a_source("vlc", "vlc")])),
        ]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("the first round");
        detection.round().await.expect("the second round");

        assert_eq!(
            listings_in(&drain(&mut events)),
            vec![vec![id("mpv")], vec![id("vlc")]]
        );
    }

    #[tokio::test]
    async fn membership_that_did_not_change_is_not_republished() {
        // The bus is the log a person reads. A line every second saying the
        // same thing is how a log stops being read.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        detection.round().await.expect("the first round");
        detection.round().await.expect("the second round");
        detection.round().await.expect("the third round");

        assert_eq!(
            listings_in(&drain(&mut events)).len(),
            1,
            "membership was republished without changing"
        );
    }

    #[tokio::test]
    async fn a_round_decides_what_an_admitted_reading_has_open() {
        // `benshi why` reads a decision rather than making one, so the round
        // that admits a reading of a file is what decides it, and the
        // decision is left where a client can read it.
        let bus = Arc::new(EventBus::new());
        let decided = Decided::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: vec![mpv.clone()],
            snapshots: vec![a_reading_of_a_file(
                &mpv,
                "/anime/[Group] Show Title - 03.mkv",
            )],
            failures: Vec::new(),
        })]);
        let (_database, mut detection) = deciding(watcher, bus, decided.clone());

        detection.round().await.expect("a round");

        let decision = decided.latest().expect("the reading was decided");
        assert_eq!(decision.player, id("mpv"));
        assert_eq!(
            decision.media,
            MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/[Group] Show Title - 03.mkv".to_vec()
            ))
        );
        assert_eq!(decision.parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(decision.parsed.episode, Episode::Only(3));
        assert!(
            matches!(decision.answer, Recognition::Unrecognised(_)),
            "nothing to match against, got {:?}",
            decision.answer
        );
    }

    #[tokio::test]
    async fn the_decision_kept_is_the_most_recent_readings() {
        // One record, replaced each time, the way the listing is: an
        // explanation is of what the daemon last acted on.
        let bus = Arc::new(EventBus::new());
        let decided = Decided::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(vec![
            Ok(PollOutcome {
                sources: vec![mpv.clone()],
                snapshots: vec![a_reading_of_a_file(
                    &mpv,
                    "/anime/[Group] Show Title - 03.mkv",
                )],
                failures: Vec::new(),
            }),
            Ok(PollOutcome {
                sources: vec![mpv.clone()],
                snapshots: vec![a_reading_of_a_file(
                    &mpv,
                    "/anime/[Group] Show Title - 04.mkv",
                )],
                failures: Vec::new(),
            }),
        ]);
        let (_database, mut detection) = deciding(watcher, bus, decided.clone());

        detection.round().await.expect("the first round");
        detection.round().await.expect("the second round");

        assert_eq!(
            decided
                .latest()
                .expect("a decision was recorded")
                .parsed
                .episode,
            Episode::Only(4)
        );
    }

    #[tokio::test]
    async fn a_denied_reading_is_not_decided() {
        // Policy suppresses the reading, and a reading the daemon does not
        // act on is not one it should explain: a decision about a denied
        // player's file would send the user looking at recognition for a
        // player the listing says is ignored.
        let bus = Arc::new(EventBus::new());
        let decided = Decided::new();
        let firefox = a_source("firefox.instance30062", "firefox");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: vec![firefox.clone()],
            snapshots: vec![a_reading_of_a_file(
                &firefox,
                "/anime/[Group] Show Title - 03.mkv",
            )],
            failures: Vec::new(),
        })]);
        let (_database, mut detection) = deciding(watcher, bus, decided.clone());

        detection.round().await.expect("a round");

        assert!(decided.latest().is_none(), "a denied reading was decided");
    }

    #[tokio::test]
    async fn a_reading_that_names_no_file_is_not_decided() {
        // Recognition reads filenames. An address names no file on this
        // machine, and a title with nothing underneath it is what a browser
        // publishes; neither is a name the stages were written for.
        let bus = Arc::new(EventBus::new());
        let decided = Decided::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: vec![mpv.clone()],
            snapshots: vec![
                a_reading_of(
                    &mpv,
                    MediaRef::Remote("https://example.invalid/stream".to_owned()),
                ),
                a_reading_of(&mpv, MediaRef::Title("Show Title - 03".to_owned())),
            ],
            failures: Vec::new(),
        })]);
        let (_database, mut detection) = deciding(watcher, bus, decided.clone());

        detection.round().await.expect("a round");

        assert!(
            decided.latest().is_none(),
            "a reading naming no file was decided"
        );
    }

    #[tokio::test]
    async fn a_platform_that_cannot_be_reached_is_transient() {
        // There is no platform failure that stopping detection would improve.
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::of(vec![Err(WatchError::Transport(Box::new(
            std::io::Error::other("the session bus is gone"),
        )))]);
        let (_database, mut detection) = detecting(watcher, bus, policy, Seen::new());

        let failure = detection.round().await.expect_err("the round fails");

        assert!(
            matches!(failure, crate::supervisor::TaskError::Transient(_)),
            "a lost bus stopped detection instead of retrying it: {failure:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_deny_takes_effect_within_one_interval_and_without_a_restart() {
        // The policy is read each round rather than once at startup, so a
        // table that changes under a running loop changes what that loop
        // publishes, and the daemon is not restarted to make it take effect.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let (_database, mut detection) =
            detecting(watcher, Arc::clone(&bus), Arc::clone(&policy), Seen::new());

        let running = tokio::spawn(async move { detection.run().await });
        tokio::time::sleep(INTERVAL / 2).await;
        assert_eq!(
            snapshots_in(&drain(&mut events)),
            vec![id("mpv")],
            "the loop published nothing before the policy changed"
        );

        policy
            .write()
            .expect("the policy table")
            .set(&app("mpv"), Policy::Deny);
        tokio::time::sleep(INTERVAL).await;

        assert!(
            snapshots_in(&drain(&mut events)).is_empty(),
            "the deny did not take effect within one interval"
        );
        assert!(
            !running.is_finished(),
            "the loop stopped instead of going on"
        );
        running.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_round_happens_once_an_interval() {
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let (_database, mut detection) = detecting(watcher, Arc::clone(&bus), policy, Seen::new());

        let running = tokio::spawn(async move { detection.run().await });
        tokio::time::sleep(INTERVAL * 3 + INTERVAL / 2).await;
        running.abort();

        assert_eq!(
            snapshots_in(&drain(&mut events)).len(),
            4,
            "a round runs at the start and once an interval after"
        );
    }

    #[tokio::test]
    async fn a_lock_a_panic_left_poisoned_is_not_read() {
        // A poisoned table means another task panicked holding it, so what it
        // says about policy cannot be trusted. Reading it anyway could publish
        // a source the user denied, which is the one mistake policy exists to
        // prevent.
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let poisoner = Arc::clone(&policy);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.write().expect("the policy table");
            panic!("a bug in another task");
        })
        .join();
        assert!(panicked.is_err(), "the thread did not panic");

        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let (_database, mut detection) = detecting(watcher, bus, policy, Seen::new());

        let read = tokio::spawn(async move { detection.round().await });

        assert!(
            read.await.expect_err("the round panics").is_panic(),
            "a poisoned policy table was read as though it were trustworthy"
        );
    }

    #[test]
    fn every_watch_error_is_classified() {
        // Written as an exhaustive match in the module, so a fourth variant
        // has to state its own answer rather than inherit one.
        assert!(matches!(
            super::classify(WatchError::Unavailable(id("mpv"))),
            crate::supervisor::TaskError::Transient(_)
        ));
        assert!(matches!(
            super::classify(WatchError::Timeout {
                player: id("mpv"),
                deadline: Duration::from_millis(500),
            }),
            crate::supervisor::TaskError::Transient(_)
        ));
        assert!(matches!(
            super::classify(WatchError::Transport(Box::new(std::io::Error::other(
                "the session bus is gone"
            )))),
            crate::supervisor::TaskError::Transient(_)
        ));
    }

    /// What `episodes_seen` holds where nothing was recorded, named because
    /// an empty array otherwise needs its element type spelt out.
    const NOTHING_SEEN: [Viewed; 0] = [];

    /// The third episode as the store holds it once `watched` of it is kept.
    fn the_third(watched: Duration, registered: bool) -> Kept {
        Kept {
            media: a_path(THIRD),
            watched,
            registered,
        }
    }

    #[tokio::test]
    async fn an_episode_is_recorded_at_the_reading_that_has_half_of_it_watched() {
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=HALF_AT));
        let mut detection = recording(watcher, &database);

        rounds(&mut detection, HALF_AT).await;
        assert_eq!(
            database.seen(),
            NOTHING_SEEN,
            "recorded a reading short of half"
        );

        rounds(&mut detection, 1).await;
        assert_eq!(
            database.seen(),
            [Viewed {
                title: SERIES.to_owned(),
                episode: Some(3),
                media: THIRD.to_owned(),
                at: THE_TIME.to_owned(),
            }]
        );
        assert_eq!(
            database.queued(),
            [Operation {
                id: 1,
                title: SERIES.to_owned(),
                kind: Kind::Progress { episode: Some(3) },
            }]
        );
    }

    #[tokio::test]
    async fn what_was_watched_is_kept_with_its_file_and_the_time() {
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=3));
        let mut detection = recording(watcher, &database);

        rounds(&mut detection, 4).await;

        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * 3, false))
        );
        assert_eq!(
            database.rows("SELECT updated_at FROM watching", |row| row
                .get::<_, String>(0)),
            [THE_TIME]
        );
    }

    #[tokio::test]
    async fn the_time_written_is_the_time_the_write_is_made_at() {
        /// How many times the clock of this test has been read.
        static READ: AtomicU64 = AtomicU64::new(0);

        /// A clock a second further on at each reading of it.
        fn a_second_on() -> SystemTime {
            the_time() + Duration::from_secs(READ.fetch_add(1, Ordering::SeqCst))
        }

        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=HALF_AT));
        let wiring = Wiring {
            recogniser: Arc::new(a_list()),
            now: a_second_on,
            ..wired_to(&database)
        };
        let mut detection = Detection::new(watcher, wiring);

        rounds(&mut detection, HALF_AT + 1).await;

        // A total is kept at each of the twenty-four readings that add to
        // it, and the episode is recorded after the last of them.
        assert_eq!(
            database.rows("SELECT updated_at FROM watching", |row| row
                .get::<_, String>(0)),
            ["2033-05-18T03:33:43Z"]
        );
        assert_eq!(
            database.rows("SELECT seen_at FROM episodes_seen", |row| row
                .get::<_, String>(0)),
            ["2033-05-18T03:33:44Z"]
        );
    }

    #[tokio::test]
    async fn detection_started_again_counts_on_from_what_was_kept() {
        // A detection task that is started again knows nothing of the one
        // before it. What that one watched comes back from the store.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let half_way = HALF_AT / 2;

        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=half_way));
        let mut first = recording(watcher, &database);
        rounds(&mut first, half_way + 1).await;
        drop(first);
        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * half_way, false))
        );

        // The first reading of the second adds nothing, so `half_way` readings
        // of it leave the total a step short of half.
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, half_way + 1..=HALF_AT + 1));
        let mut second = recording(watcher, &database);
        rounds(&mut second, half_way).await;
        assert_eq!(
            database.seen(),
            NOTHING_SEEN,
            "recorded a reading short of half"
        );

        rounds(&mut second, 1).await;
        assert_eq!(database.seen().len(), 1, "{:?}", database.seen());
        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * HALF_AT, true))
        );
    }

    #[tokio::test]
    async fn an_episode_the_store_holds_as_registered_is_not_recorded_again() {
        let database = Database::new();
        {
            let mut store = database.store.lock().expect("the store");
            store
                .keep(&Total {
                    title: SERIES.to_owned(),
                    episode: Some(3),
                    media: a_path(THIRD),
                    watched: STEP * HALF_AT,
                    at: the_time(),
                })
                .expect("the total is kept");
            store
                .record(&Viewing {
                    title: SERIES.to_owned(),
                    episode: Some(3),
                    media: a_path(THIRD),
                    at: the_time(),
                })
                .expect("the episode is recorded");
        }
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=HALF_AT));
        let mut detection = recording(watcher, &database);

        rounds(&mut detection, HALF_AT + 1).await;

        assert_eq!(database.seen().len(), 1, "{:?}", database.seen());
    }

    /// Detection in which `source` has watched the third episode to where it
    /// registered, with `next` as the round after that.
    ///
    /// The total is looked at before the detection is handed back. A test of
    /// a close reads the row going, and a row that was never there is gone
    /// as well.
    async fn registered_under(
        database: &Database,
        policy: Arc<RwLock<PolicyTable>>,
        source: &SourceInfo,
        next: PollOutcome,
    ) -> Detection<ScriptedWatcher> {
        let mut script = watching(source, THIRD, 0..=HALF_AT);
        script.push(Ok(next));
        let mut detection = recording_under(ScriptedWatcher::of(script), database, policy);

        rounds(&mut detection, HALF_AT + 1).await;
        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * HALF_AT, true)),
            "the episode registered before the round under test"
        );

        detection
    }

    /// [`registered_under`] the policy table a user starts with.
    async fn registered(
        database: &Database,
        source: &SourceInfo,
        next: PollOutcome,
    ) -> Detection<ScriptedWatcher> {
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));

        registered_under(database, policy, source, next).await
    }

    #[tokio::test]
    async fn a_player_that_is_no_longer_listed_closes_its_episode() {
        // The store drops the total of an episode that registered when the
        // episode is closed, so the row going is the close arriving.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let mut detection = registered(&database, &mpv, a_round_of(&[], Vec::new())).await;

        rounds(&mut detection, 1).await;

        assert_eq!(database.kept(SERIES, Some(3)), None);
    }

    #[tokio::test]
    async fn an_episode_that_did_not_register_keeps_its_total_when_its_player_leaves() {
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let mut script = watching(&mpv, THIRD, 0..=3);
        script.push(Ok(a_round_of(&[], Vec::new())));
        let mut detection = recording(ScriptedWatcher::of(script), &database);

        rounds(&mut detection, 5).await;

        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * 3, false))
        );
    }

    #[tokio::test]
    async fn a_player_with_nothing_open_closes_its_episode() {
        // A player that closed its file is still listed and sends no reading
        // to say so.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let nothing_open = a_round_of(&[&mpv], Vec::new());
        let mut detection = registered(&database, &mpv, nothing_open).await;

        rounds(&mut detection, 1).await;

        assert_eq!(database.kept(SERIES, Some(3)), None);
    }

    #[tokio::test]
    async fn a_player_that_did_not_answer_keeps_its_episode_open() {
        // Failing to answer is not closing. A player that timed out once
        // would otherwise end a sitting it is still in.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let silent = a_round_without_an_answer_from(&mpv);
        let mut detection = registered(&database, &mpv, silent).await;

        rounds(&mut detection, 1).await;

        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * HALF_AT, true))
        );
    }

    #[tokio::test]
    async fn a_reading_without_a_description_keeps_its_player_in() {
        // The round cannot tell what that player has open. Closed here, an
        // episode that registered would lose its total and be recorded a
        // second time.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let undescribed = a_round_of(&[], vec![playing(&mpv, THIRD, HALF_AT + 1)]);
        let mut detection = registered(&database, &mpv, undescribed).await;

        rounds(&mut detection, 1).await;

        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * HALF_AT, true))
        );
    }

    #[tokio::test]
    async fn a_player_kept_in_by_a_round_is_let_go_by_the_first_that_hears_nothing_of_it() {
        // Neither round that kept the player in lists it, so a rule that let
        // go of whoever the round before listed would hold its episode open
        // for good.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        for kept_in_by in [
            a_round_of(&[], vec![playing(&mpv, THIRD, HALF_AT + 1)]),
            a_round_without_an_answer_from(&mpv),
        ] {
            let mut detection = registered(&database, &mpv, kept_in_by).await;

            rounds(&mut detection, 2).await;

            assert_eq!(database.kept(SERIES, Some(3)), None);
        }
    }

    #[tokio::test]
    async fn a_player_that_was_let_go_is_forgotten() {
        // Kept note of, it would be let go again in every round after the
        // one it left in.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let mut detection = registered(&database, &mpv, a_round_of(&[], Vec::new())).await;
        assert_eq!(detection.heard, BTreeSet::from([id("mpv")]));

        rounds(&mut detection, 1).await;

        assert_eq!(detection.heard, BTreeSet::new());
    }

    #[tokio::test]
    async fn a_player_policy_stopped_admitting_closes_its_episode() {
        // Its readings stop reaching the session, and a player that held an
        // episode open by being denied would hold it open for every other.
        let database = Database::new();
        let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
        let mpv = a_source("mpv", "mpv");
        let denied = a_round_of(&[&mpv], vec![playing(&mpv, THIRD, HALF_AT + 1)]);
        let mut detection = registered_under(&database, Arc::clone(&policy), &mpv, denied).await;

        policy
            .write()
            .expect("the policy table")
            .set(&app("mpv"), Policy::Deny);
        rounds(&mut detection, 1).await;

        assert_eq!(database.kept(SERIES, Some(3)), None);
    }

    #[tokio::test]
    async fn a_player_that_opens_a_file_nobody_recognised_closes_its_episode() {
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let moved_on = a_round_of(&[&mpv], vec![playing(&mpv, NOT_LISTED, HALF_AT + 1)]);
        let mut detection = registered(&database, &mpv, moved_on).await;

        rounds(&mut detection, 1).await;

        assert_eq!(database.kept(SERIES, Some(3)), None);
    }

    #[tokio::test]
    async fn a_player_that_opens_what_names_no_file_closes_its_episode() {
        // Recognition is not asked about an address, and the session is
        // still handed the reading: it is how the player says it moved on.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let moved_on = a_round_of(
            &[&mpv],
            vec![PlayerSnapshot {
                media: MediaRef::Remote("https://example.invalid/stream".to_owned()),
                ..playing(&mpv, THIRD, HALF_AT + 1)
            }],
        );
        let mut detection = registered(&database, &mpv, moved_on).await;

        rounds(&mut detection, 1).await;

        assert_eq!(database.kept(SERIES, Some(3)), None);
    }

    #[tokio::test]
    async fn an_episode_handed_to_another_player_in_one_round_stays_open() {
        // The readings of a round are taken before anybody is let go. Taken
        // after, the episode would be closed and its total dropped before the
        // player that has it open now was heard.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let vlc = a_source("vlc", "vlc");
        let handed_over = a_round_of(&[&vlc], vec![playing(&vlc, THIRD, HALF_AT + 1)]);
        let mut detection = registered(&database, &mpv, handed_over).await;

        rounds(&mut detection, 1).await;

        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * HALF_AT, true))
        );
    }

    #[tokio::test]
    async fn a_total_is_kept_before_its_episode_is_recorded() {
        // A file of ten seconds is watched at its second reading, so the
        // total kept there is the first the store holds of it. Recording
        // marks the total the store holds, and marks nothing where it holds
        // none.
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let short = |steps| PlayerSnapshot {
            duration: Known::Value(Duration::from_secs(10)),
            ..playing(&mpv, THIRD, steps)
        };
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round_of(&[&mpv], vec![short(0)])),
            Ok(a_round_of(&[&mpv], vec![short(2)])),
        ]);
        let mut detection = recording(watcher, &database);

        rounds(&mut detection, 2).await;

        assert_eq!(database.seen().len(), 1, "{:?}", database.seen());
        assert_eq!(
            database.kept(SERIES, Some(3)),
            Some(the_third(STEP * 2, true))
        );
    }

    #[tokio::test]
    async fn a_file_nobody_recognised_reaches_no_table() {
        let database = Database::new();
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, NOT_LISTED, 0..=HALF_AT));
        let mut detection = recording(watcher, &database);

        rounds(&mut detection, HALF_AT + 1).await;

        for table in ["shows", "watching", "episodes_seen", "sync_queue"] {
            assert_eq!(database.count_of(table), [0], "{table}");
        }
    }

    #[tokio::test]
    async fn a_total_the_store_refuses_stops_detection_for_good() {
        // Detecting on while nothing is recorded looks the same as working.
        let database = Database::new();
        database.refusing("INSERT", "watching");
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=1));
        let mut detection = recording(watcher, &database);

        let failure = stopped_after(&mut detection, 1).await;

        assert!(matches!(failure, TaskError::Permanent(_)), "{failure:?}");
        assert!(
            failure.to_string().contains("refused by the test"),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn an_episode_the_store_refuses_stops_detection_for_good() {
        let database = Database::new();
        database.refusing("INSERT", "episodes_seen");
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=HALF_AT));
        let mut detection = recording(watcher, &database);

        let failure = stopped_after(&mut detection, HALF_AT).await;

        assert!(matches!(failure, TaskError::Permanent(_)), "{failure:?}");
        assert!(
            failure.to_string().contains("refused by the test"),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn a_question_the_store_cannot_answer_stops_detection_for_good() {
        let database = Database::new();
        database
            .store
            .lock()
            .expect("the store")
            .keep(&Total {
                title: SERIES.to_owned(),
                episode: Some(3),
                media: a_path(THIRD),
                watched: STEP,
                at: the_time(),
            })
            .expect("the total is kept");
        database.by_hand("UPDATE watching SET media = '%zz'");
        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=0));
        let mut detection = recording(watcher, &database);

        let failure = stopped_after(&mut detection, 0).await;

        assert!(matches!(failure, TaskError::Permanent(_)), "{failure:?}");
        assert!(
            failure.to_string().contains("is not an escaped path"),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn a_close_the_store_refuses_stops_detection_for_good() {
        let database = Database::new();
        database.refusing("DELETE", "watching");
        let mpv = a_source("mpv", "mpv");
        let mut script = watching(&mpv, THIRD, 0..=HALF_AT);
        script.push(Ok(a_round_of(&[], Vec::new())));
        let mut detection = recording(ScriptedWatcher::of(script), &database);

        let failure = stopped_after(&mut detection, HALF_AT + 1).await;

        assert!(matches!(failure, TaskError::Permanent(_)), "{failure:?}");
        assert!(
            failure.to_string().contains("refused by the test"),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn a_store_a_panic_left_poisoned_is_not_written() {
        // A poisoned store means a call into it panicked, which is a bug, and
        // the supervisor is what records one.
        let database = Database::new();
        let poisoner = Arc::clone(&database.store);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.lock().expect("the store");
            panic!("a bug in another task");
        })
        .join();
        assert!(panicked.is_err(), "the thread did not panic");

        let mpv = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(watching(&mpv, THIRD, 0..=0));
        let mut detection = recording(watcher, &database);

        let asked = tokio::spawn(async move { detection.round().await });

        // The panic is what the supervisor records, so it has to arrive
        // saying what was wrong and not that a thread ended.
        let panic = asked
            .await
            .expect_err("a poisoned store was used as though it were trustworthy")
            .into_panic();
        let said = panic
            .downcast_ref::<String>()
            .expect("a panic that says something");
        assert!(said.starts_with("the store was poisoned"), "{said}");
    }
}
