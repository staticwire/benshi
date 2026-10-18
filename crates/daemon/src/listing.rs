//! Reading the list, and what a round decides against.
//!
//! The list's task reads the account's list from AniList, files it in the
//! store, and puts a recognizer of it in place of the one the rounds decide
//! against. It reads at the times `[list]` names: as the daemon starts, every
//! `refresh`, and where a round decided that a name spelling a title is not
//! on the list, at most once in `on_miss`. A login wakes it at once.
//!
//! A reading that fails is tried again after [`FIRST_RETRY`], and after
//! twice as long each time it fails again, up to [`LAST_RETRY`]. A token
//! AniList refuses is not tried again until a login. A token that is not
//! found, or that the secret store cannot give, is looked for again at the
//! next `refresh`. A failure is said once, and said again only where its
//! reason changes, or where a reading went through or a login came in
//! between. A name that misses the list brings a reading only where the last
//! one went through.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use benshi_core::ShowId;
use benshi_core::list::Entry;
use benshi_core::recognize::altname::Altnames;
use benshi_core::recognize::corpus::Corpus;
use benshi_core::settle::Facts;
use benshi_core::token::Token;
use benshi_store::Store;
#[cfg(target_os = "linux")]
use benshi_sync::anilist::AniList;
#[cfg(target_os = "linux")]
use benshi_sync::anilist::transport::Http;
use humantime::format_duration;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::config::{Period, Reading};
use crate::detection::POISONED_STORE;
use crate::recognition::{Current, Recognizer};
use crate::secrets::{self, ANILIST};
#[cfg(target_os = "linux")]
use crate::secrets::{Asking, SecretService, Secrets};
use crate::supervisor::TaskError;

/// How long a reading that failed waits before it is tried again.
pub const FIRST_RETRY: Duration = Duration::from_mins(1);

/// The longest a reading that failed again and again waits.
pub const LAST_RETRY: Duration = Duration::from_hours(1);

/// What is said where no token is found.
const NO_TOKEN: &str = "no AniList token, so no list is read: benshi login keeps one";

/// The backend a list is filed under in the store.
const BACKEND: &str = ANILIST.backend;

/// What wakes the list's task before its time: a name a round decided the
/// list does not hold, and a token a login kept. Cloning shares them.
#[derive(Debug, Clone, Default)]
pub struct Wakes {
    pub(crate) missed: Arc<Notify>,
    pub(crate) logged_in: Arc<Notify>,
}

impl Wakes {
    /// Nothing has woken the task yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A round decided that a name the list does not hold spelled a title.
    pub fn missed(&self) {
        self.missed.notify_one();
    }

    /// A login kept a token.
    pub fn logged_in(&self) {
        self.logged_in.notify_one();
    }
}

/// Where the token and the list come from.
pub trait Source {
    /// The token to read the list with, and none where none is kept.
    fn token(&self) -> impl Future<Output = Result<Option<Token>, secrets::Error>> + Send;

    /// The list `token` is of.
    fn list(
        &self,
        token: &Token,
    ) -> impl Future<Output = Result<Vec<Entry>, benshi_sync::Error>> + Send;
}

/// The token and the list from AniList.
///
/// The token is the one given from the environment where there is one, and
/// else the one the Secret Service of the session keeps, which is never asked
/// to unlock: the daemon opens no window of its own.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct FromAniList {
    address: String,
    given: Option<Token>,
}

#[cfg(target_os = "linux")]
impl FromAniList {
    /// Lists read from the service at `address`, with `given` as the token
    /// where there is one.
    #[must_use]
    pub fn new(address: &str, given: Option<Token>) -> Self {
        Self {
            address: address.to_owned(),
            given,
        }
    }
}

#[cfg(target_os = "linux")]
impl Source for FromAniList {
    async fn token(&self) -> Result<Option<Token>, secrets::Error> {
        if let Some(given) = &self.given {
            return Ok(Some(given.clone()));
        }
        let store = SecretService::connect(secrets::DEADLINE, Asking::Never).await?;

        Ok(store.read(ANILIST).await?.map(Token::new))
    }

    async fn list(&self, token: &Token) -> Result<Vec<Entry>, benshi_sync::Error> {
        let http = Http::new(&self.address, token.secret())
            .map_err(|unbuilt| benshi_sync::Error::Unreachable(unbuilt.into()))?;

        AniList::new(http).list().await
    }
}

/// What the task says, a line at a time.
pub type Say = Arc<dyn Fn(&str) + Send + Sync>;

/// What the list's task reads from, when, and how it says what it does.
pub struct Listing<S> {
    /// Where the token and the list come from.
    pub source: S,
    /// When the list is read.
    pub reading: Reading,
    /// How the task says a line.
    pub say: Say,
}

/// A recognizer of these shows of a list and their entries, each filed with
/// the facts that settle what is recognized as it.
#[must_use]
pub fn recognizer_of(listed: &[(ShowId, Entry)]) -> Recognizer {
    let mut corpus = Corpus::new();
    for (show, entry) in listed {
        corpus.file(*show, &entry.title, &entry.spellings);
        corpus.file_facts(*show, Facts::from(entry));
    }

    Recognizer::of(corpus, Altnames::new())
}

/// A recognizer of the list the store holds.
///
/// # Errors
///
/// What [`Store::listed`] fails with.
pub fn kept(store: &Store) -> Result<Recognizer, benshi_store::Error> {
    store.listed(BACKEND).map(|listed| recognizer_of(&listed))
}

/// The list's task: what it reads from, when, and where it puts what it read.
pub struct Reader<S> {
    source: S,
    reading: Reading,
    store: Arc<Mutex<Store>>,
    current: Current,
    wakes: Wakes,
    say: Say,
}

impl<S: Source + Sync> Reader<S> {
    /// A task reading from `source` at the times `reading` names, filing
    /// into `store` and putting what it read in `current`, woken by `wakes`
    /// and saying what it does through `say`.
    #[must_use]
    pub fn new(
        source: S,
        reading: Reading,
        store: Arc<Mutex<Store>>,
        current: Current,
        wakes: Wakes,
        say: Say,
    ) -> Self {
        Self {
            source,
            reading,
            store,
            current,
            wakes,
            say,
        }
    }

    /// Read the list at the times the settings name, for ever.
    ///
    /// # Errors
    ///
    /// None: every failure of a reading is said, and the task goes on. The
    /// result is the supervisor's.
    pub async fn run(&self) -> Result<(), TaskError> {
        let mut outcome = Outcome::Read;
        let mut wait = FIRST_RETRY;
        let mut said = None;
        let mut last: Option<Instant> = None;
        let mut due = if self.reading.at_start {
            Some(Instant::now())
        } else {
            self.refreshed()
        };

        loop {
            tokio::select! {
                () = self.wakes.logged_in.notified() => said = None,
                () = until(due) => {}
                // Taken whatever came of the last reading, so that a miss
                // heard while readings fail is not kept until one goes
                // through.
                () = self.wakes.missed.notified() => {
                    if outcome == Outcome::Read
                        && let Period::Every(on_miss) = self.reading.on_miss
                    {
                        let now = Instant::now();
                        let earliest = last.map_or(now, |last| (last + on_miss).max(now));
                        due = Some(due.map_or(earliest, |due| due.min(earliest)));
                    }
                    continue;
                }
            }

            last = Some(Instant::now());
            outcome = self.read(&mut said, wait).await;
            due = match outcome {
                Outcome::Read => {
                    wait = FIRST_RETRY;
                    self.refreshed()
                }
                Outcome::NoToken => self.refreshed(),
                Outcome::Refused => None,
                Outcome::Failed { asked: Some(after) } => Some(Instant::now() + after),
                Outcome::Failed { asked: None } => {
                    let retried = Instant::now() + wait;
                    wait = (wait * 2).min(LAST_RETRY);
                    Some(retried)
                }
            };
        }
    }

    /// When the next reading is due by `refresh`, and none where it is off.
    fn refreshed(&self) -> Option<Instant> {
        match self.reading.refresh {
            Period::Every(every) => Some(Instant::now() + every),
            Period::Off => None,
        }
    }

    /// Read the token and the list, file the list and put a recognizer of
    /// it in place, and say what came of it. `wait` is how long a failure
    /// waits before the reading is tried again.
    async fn read(&self, said: &mut Option<String>, wait: Duration) -> Outcome {
        let token = match self.source.token().await {
            Ok(Some(token)) => token,
            Ok(None) => {
                self.tell(said, NO_TOKEN.to_owned(), || NO_TOKEN.to_owned());
                return Outcome::NoToken;
            }
            Err(unread) => {
                let line = format!("AniList token could not be read: {unread}");
                self.tell_once(said, line);
                return Outcome::NoToken;
            }
        };

        match self.source.list(&token).await {
            Ok(entries) => {
                let count = entries.len();
                match self.filed(entries).await {
                    Ok(recognizer) => {
                        self.current.replace(recognizer);
                        *said = None;
                        let entries = if count == 1 { "entry" } else { "entries" };
                        (self.say)(&format!("AniList list read: {count} {entries}"));
                        Outcome::Read
                    }
                    Err(unkept) => {
                        let line = format!("AniList list could not be kept: {unkept}");
                        self.tell_once(said, line);
                        Outcome::Failed { asked: None }
                    }
                }
            }
            Err(benshi_sync::Error::Refused(why)) => {
                let line = format!("AniList refused the token: {why}; benshi login gives another");
                self.tell_once(said, line);
                Outcome::Refused
            }
            Err(failed) => {
                let asked = match failed {
                    benshi_sync::Error::Wait { after } => Some(after),
                    _ => None,
                };
                let reason = format!("AniList list could not be read: {failed}");
                let retried = format_duration(asked.unwrap_or(wait));
                self.tell(said, reason.clone(), || {
                    format!("{reason}; tried again in {retried}")
                });
                Outcome::Failed { asked }
            }
        }
    }

    /// File `entries` as the list, and answer with a recognizer of what the
    /// store then holds, or with why it could not be kept.
    async fn filed(&self, entries: Vec<Entry>) -> Result<Recognizer, String> {
        let store = Arc::clone(&self.store);
        let filing = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().expect(POISONED_STORE);
            store.file(BACKEND, &entries)?;
            store.listed(BACKEND)
        });

        match filing.await {
            Ok(listed) => listed
                .map(|listed| recognizer_of(&listed))
                .map_err(|unkept| unkept.to_string()),
            // Handed on as it was raised.
            Err(panicked) if panicked.is_panic() => {
                std::panic::resume_unwind(panicked.into_panic())
            }
            Err(given_up) => Err(format!("store could not be asked: {given_up}")),
        }
    }

    /// Say `line` where it is not what was said last.
    fn tell_once(&self, said: &mut Option<String>, line: String) {
        self.tell(said, line.clone(), || line);
    }

    /// Say `line` where `reason` is not what was said last.
    fn tell(&self, said: &mut Option<String>, reason: String, line: impl FnOnce() -> String) {
        if said.as_deref() != Some(reason.as_str()) {
            (self.say)(&line());
            *said = Some(reason);
        }
    }
}

/// What came of a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The list was read and filed.
    Read,
    /// No token was found, or none could be read.
    NoToken,
    /// AniList refused the token.
    Refused,
    /// The reading failed, and where AniList asked for a wait, how long.
    Failed { asked: Option<Duration> },
}

/// Sleep until `due`, and for ever where nothing is due.
async fn until(due: Option<Instant>) {
    match due {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::{self, Future};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use benshi_core::list::{Entry, EntryId, Status};
    use benshi_core::path::RawPath;
    use benshi_core::recognize::Recognition;
    use benshi_core::settle::{Fact, Settled};
    use benshi_core::token::Token;
    use benshi_core::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, ShowId};
    use benshi_store::Store;
    use tempfile::TempDir;
    use tokio::task::JoinHandle;
    use tokio::time::Instant;

    use super::{BACKEND, FIRST_RETRY, LAST_RETRY, Reader, Source, Wakes, kept, recognizer_of};
    use crate::config::{Period, Reading};
    use crate::recognition::{Current, Recognizer};
    use crate::secrets;
    use crate::supervisor::TaskError;

    const HOUR: Duration = Duration::from_hours(1);
    const MINUTE: Duration = Duration::from_mins(1);

    /// An entry of a list, under a number of a work no list of the user's
    /// holds.
    fn an_entry(id: i64, title: &str) -> Entry {
        Entry {
            id: EntryId(id),
            title: title.to_owned(),
            spellings: vec![format!("{title} Also")],
            format: None,
            episodes: Some(12),
            episode_length: None,
            year: None,
            released: true,
            status: Status::Watching,
            progress: 2,
            rewatches: 0,
            score: None,
        }
    }

    fn the_list() -> Vec<Entry> {
        vec![
            an_entry(900_001, "Show Title"),
            an_entry(900_002, "Another Show"),
        ]
    }

    /// Whether `recognizer` recognizes a file of this title as it.
    fn recognizes(recognizer: &Recognizer, title: &str) -> bool {
        let path = RawPath::from_bytes(format!("[Group] {title} - 03.mkv").into_bytes());
        matches!(
            recognizer.decide(&path).1,
            Recognition::Recognized(found) if found.title == title
        )
    }

    /// A source that answers as a test scripts it, and a token and the list
    /// once the script is spent; every reading of the list is noted.
    struct Scripted {
        tokens: Mutex<VecDeque<Result<Option<Token>, secrets::Error>>>,
        lists: Mutex<VecDeque<Result<Vec<Entry>, benshi_sync::Error>>>,
        read_at: Mutex<Vec<Duration>>,
        began: Instant,
    }

    impl Scripted {
        fn new() -> Self {
            Self {
                tokens: Mutex::new(VecDeque::new()),
                lists: Mutex::new(VecDeque::new()),
                read_at: Mutex::new(Vec::new()),
                began: Instant::now(),
            }
        }

        fn tokens(
            self,
            tokens: impl IntoIterator<Item = Result<Option<Token>, secrets::Error>>,
        ) -> Self {
            self.tokens.lock().expect("the script").extend(tokens);
            self
        }

        fn lists(
            self,
            lists: impl IntoIterator<Item = Result<Vec<Entry>, benshi_sync::Error>>,
        ) -> Self {
            self.lists.lock().expect("the script").extend(lists);
            self
        }
    }

    impl Source for Arc<Scripted> {
        fn token(&self) -> impl Future<Output = Result<Option<Token>, secrets::Error>> + Send {
            let next = self
                .tokens
                .lock()
                .expect("the script")
                .pop_front()
                .unwrap_or_else(|| Ok(Some(Token::new("a token"))));
            future::ready(next)
        }

        fn list(
            &self,
            _token: &Token,
        ) -> impl Future<Output = Result<Vec<Entry>, benshi_sync::Error>> + Send {
            self.read_at
                .lock()
                .expect("the record")
                .push(self.began.elapsed());
            let next = self
                .lists
                .lock()
                .expect("the script")
                .pop_front()
                .unwrap_or_else(|| Ok(the_list()));
            future::ready(next)
        }
    }

    /// A list's task running against a script, with all it touches.
    struct Running {
        _home: TempDir,
        store: Arc<Mutex<Store>>,
        current: Current,
        wakes: Wakes,
        said: Arc<Mutex<Vec<String>>>,
        source: Arc<Scripted>,
        task: JoinHandle<Result<(), TaskError>>,
    }

    impl Running {
        fn reads(&self) -> Vec<Duration> {
            self.source.read_at.lock().expect("the record").clone()
        }

        fn said(&self) -> Vec<String> {
            self.said.lock().expect("the record").clone()
        }

        fn filed(&self) -> Vec<String> {
            self.store
                .lock()
                .expect("the store")
                .listed(BACKEND)
                .expect("the list reads")
                .into_iter()
                .map(|(_show, entry)| entry.title)
                .collect()
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// The list's task with these settings, reading from `source`.
    fn started(reading: Reading, source: Scripted) -> Running {
        let home = tempfile::tempdir().expect("a directory of our own");
        let store = Store::open(&home.path().join("benshi.db")).expect("the database opens");

        running(home, store, reading, source)
    }

    /// [`started`], over a store that refuses every entry it is given.
    fn started_refusing(reading: Reading, source: Scripted) -> Running {
        let home = tempfile::tempdir().expect("a directory of our own");
        let path = home.path().join("benshi.db");
        let store = Store::open(&path).expect("the database opens");
        rusqlite::Connection::open(&path)
            .expect("the file opens a second time")
            .execute_batch(
                "CREATE TRIGGER refused BEFORE INSERT ON list_entries \
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
            )
            .expect("the trigger is made");

        running(home, store, reading, source)
    }

    fn running(home: TempDir, store: Store, reading: Reading, source: Scripted) -> Running {
        let store = Arc::new(Mutex::new(store));
        let current = Current::new(Recognizer::empty());
        let wakes = Wakes::new();
        let said = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(source);
        let reader = Reader::new(
            Arc::clone(&source),
            reading,
            Arc::clone(&store),
            current.clone(),
            wakes.clone(),
            {
                let said = Arc::clone(&said);
                Arc::new(move |line: &str| said.lock().expect("the record").push(line.to_owned()))
            },
        );
        let task = tokio::spawn(async move { reader.run().await });

        Running {
            _home: home,
            store,
            current,
            wakes,
            said,
            source,
            task,
        }
    }

    fn reading(refresh: Period, at_start: bool, on_miss: Period) -> Reading {
        Reading {
            refresh,
            at_start,
            on_miss,
        }
    }

    /// Let this much time pass, and the task do what falls in it.
    async fn after(passing: Duration) {
        tokio::time::sleep(passing).await;
    }

    /// A moment, for the task to do what is due now.
    const NOW: Duration = Duration::from_millis(1);

    #[test]
    fn a_recognizer_of_a_list_recognizes_each_entry_under_each_spelling() {
        let listed: Vec<_> = the_list()
            .into_iter()
            .enumerate()
            .map(|(place, entry)| {
                (
                    benshi_core::ShowId(i64::try_from(place).expect("small") + 1),
                    entry,
                )
            })
            .collect();
        let mut spelled = listed.clone();
        // A spelling that shares no word with the title, so the name reaches
        // the entry only where its spellings are filed.
        spelled[0].1.spellings = vec!["Kotonaru Namae".to_owned()];

        let recognizer = recognizer_of(&listed);
        let spelled = recognizer_of(&spelled);

        assert!(recognizes(&recognizer, "Show Title"));
        assert!(recognizes(&recognizer, "Another Show"));
        let (_parsed, answer) = spelled.decide(&RawPath::from_bytes(
            b"[Group] Kotonaru Namae - 03.mkv".to_vec(),
        ));
        assert!(
            matches!(&answer, Recognition::Recognized(found) if found.title == "Show Title"),
            "{answer:?}"
        );
    }

    #[test]
    fn a_recognizer_of_a_list_settles_by_the_facts_of_each_entry() {
        // The service lists the entry as not yet released, and no file is of
        // a work that has not been released.
        let mut announced = an_entry(900_001, "Show Title");
        announced.released = false;
        let recognizer = recognizer_of(&[(ShowId(1), announced)]);
        let path = RawPath::from_bytes(b"[Group] Show Title - 03.mkv".to_vec());
        let reading = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(path.clone()),
            state: PlayState::Playing,
            position: Known::NotReported,
            duration: Known::NotReported,
            observed_at: benshi_core::clock::Timestamp::epoch(),
        };

        let (parsed, answer) = recognizer.decide(&path);

        assert_eq!(
            recognizer.settle(&answer, &parsed, &reading),
            Settled::Refused {
                show: ShowId(1),
                fact: Fact::NotReleased,
            }
        );
    }

    #[test]
    fn a_recognizer_of_the_store_holds_the_list_filed_there() {
        let home = tempfile::tempdir().expect("a directory of our own");
        let mut store = Store::open(&home.path().join("benshi.db")).expect("the database opens");
        store.file(BACKEND, &the_list()).expect("the list is filed");

        let recognizer = kept(&store).expect("the list reads");

        assert!(recognizes(&recognizer, "Show Title"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_read_at_the_start_is_filed_said_and_decided_against() {
        let running = started(Reading::default(), Scripted::new());

        after(NOW).await;

        assert_eq!(running.reads(), [Duration::ZERO]);
        assert_eq!(running.filed(), ["Show Title", "Another Show"]);
        assert_eq!(running.said(), ["AniList list read: 2 entries"]);
        assert!(recognizes(&running.current.now(), "Show Title"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_of_one_entry_is_said_as_one() {
        let running = started(
            Reading::default(),
            Scripted::new().lists([Ok(vec![an_entry(900_001, "Show Title")])]),
        );

        after(NOW).await;

        assert_eq!(running.said(), ["AniList list read: 1 entry"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_is_not_read_at_the_start_where_the_settings_say_so() {
        let running = started(
            reading(Period::Every(HOUR), false, Period::Off),
            Scripted::new(),
        );

        after(MINUTE * 59).await;
        assert!(running.reads().is_empty(), "{:?}", running.reads());
        after(MINUTE + NOW).await;

        assert_eq!(running.reads(), [HOUR]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_is_read_again_at_each_refresh() {
        let running = started(Reading::default(), Scripted::new());

        after(HOUR * 2 + NOW).await;

        assert_eq!(running.reads(), [Duration::ZERO, HOUR, HOUR * 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_read_once_is_not_read_again_where_refresh_is_off() {
        let running = started(reading(Period::Off, true, Period::Off), Scripted::new());

        after(HOUR * 5).await;

        assert_eq!(running.reads(), [Duration::ZERO]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_miss_long_after_the_last_reading_brings_one_at_once() {
        let running = started(Reading::default(), Scripted::new());
        after(MINUTE * 30).await;

        running.wakes.missed();
        after(NOW).await;

        assert_eq!(running.reads(), [Duration::ZERO, MINUTE * 30]);
    }

    #[tokio::test(start_paused = true)]
    async fn misses_soon_after_a_reading_bring_one_once_on_miss_has_passed() {
        let running = started(Reading::default(), Scripted::new());
        after(MINUTE).await;

        running.wakes.missed();
        after(MINUTE).await;
        running.wakes.missed();
        after(MINUTE * 4).await;

        assert_eq!(running.reads(), [Duration::ZERO, MINUTE * 5]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_miss_brings_nothing_where_on_miss_is_off() {
        let running = started(
            reading(Period::Every(HOUR), true, Period::Off),
            Scripted::new(),
        );
        after(MINUTE * 30).await;

        running.wakes.missed();
        after(MINUTE * 10).await;

        assert_eq!(running.reads(), [Duration::ZERO]);
    }

    /// A reading that fails as a service that is down fails.
    fn down() -> Result<Vec<Entry>, benshi_sync::Error> {
        Err(benshi_sync::Error::Unreachable("connection refused".into()))
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_reading_is_said_once_and_tried_again_after_a_growing_wait() {
        let running = started(
            Reading::default(),
            Scripted::new().lists((0..8).map(|_| down())),
        );

        after(HOUR * 3 + MINUTE * 3 + NOW).await;

        let minutes: Vec<u64> = running.reads().iter().map(|at| at.as_secs() / 60).collect();
        assert_eq!(minutes, [0, 1, 3, 7, 15, 31, 63, 123, 183]);
        assert_eq!(
            running.said(),
            [
                "AniList list could not be read: could not be reached: connection refused; \
                 tried again in 1m",
                "AniList list read: 2 entries",
            ]
        );
        assert_eq!(FIRST_RETRY, MINUTE);
        assert_eq!(LAST_RETRY, HOUR);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reading_that_goes_through_starts_the_wait_again() {
        let running = started(
            Reading::default(),
            Scripted::new().lists([down(), down(), Ok(the_list()), down()]),
        );

        after(HOUR + MINUTE * 4 + NOW).await;

        let minutes: Vec<u64> = running.reads().iter().map(|at| at.as_secs() / 60).collect();
        assert_eq!(minutes, [0, 1, 3, 63, 64]);
        assert_eq!(
            running.said(),
            [
                "AniList list could not be read: could not be reached: connection refused; \
                 tried again in 1m",
                "AniList list read: 2 entries",
                "AniList list could not be read: could not be reached: connection refused; \
                 tried again in 1m",
                "AniList list read: 2 entries",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_to_wait_is_waited_out() {
        let running = started(
            Reading::default(),
            Scripted::new().lists([Err(benshi_sync::Error::Wait {
                after: Duration::from_secs(90),
            })]),
        );

        after(Duration::from_secs(90) + NOW).await;

        assert_eq!(running.reads(), [Duration::ZERO, Duration::from_secs(90)]);
        assert_eq!(
            running.said()[0],
            "AniList list could not be read: asked to wait 90s; tried again in 1m 30s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_token_is_said_and_not_read_with_again_until_a_login() {
        let running = started(
            Reading::default(),
            Scripted::new().lists([Err(benshi_sync::Error::Refused("Invalid token".to_owned()))]),
        );
        after(MINUTE).await;
        running.wakes.missed();

        after(HOUR * 3).await;
        assert_eq!(running.reads(), [Duration::ZERO]);
        running.wakes.logged_in();
        after(NOW).await;

        assert_eq!(running.reads(), [Duration::ZERO, HOUR * 3 + MINUTE]);
        assert_eq!(
            running.said(),
            [
                "AniList refused the token: Invalid token; benshi login gives another",
                "AniList list read: 2 entries",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_refused_again_after_a_login_is_said_again() {
        let refused = || Err(benshi_sync::Error::Refused("Invalid token".to_owned()));
        let running = started(
            Reading::default(),
            Scripted::new().lists([refused(), refused()]),
        );
        after(MINUTE / 2).await;

        running.wakes.logged_in();
        after(NOW).await;

        assert_eq!(
            running.said(),
            [
                "AniList refused the token: Invalid token; benshi login gives another",
                "AniList refused the token: Invalid token; benshi login gives another",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_token_is_said_once_and_looked_for_again_at_each_refresh() {
        let running = started(
            Reading::default(),
            Scripted::new().tokens([Ok(None), Ok(None), Ok(None)]),
        );
        after(MINUTE).await;
        running.wakes.missed();

        after(HOUR * 2 + NOW).await;
        assert!(running.reads().is_empty(), "{:?}", running.reads());
        after(HOUR).await;

        assert_eq!(running.reads(), [HOUR * 3]);
        assert_eq!(
            running.said(),
            [
                "no AniList token, so no list is read: benshi login keeps one",
                "AniList list read: 2 entries",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_login_brings_a_reading_at_once() {
        let running = started(Reading::default(), Scripted::new().tokens([Ok(None)]));
        after(MINUTE * 10).await;

        running.wakes.logged_in();
        after(NOW).await;

        assert_eq!(running.reads(), [MINUTE * 10]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_the_store_cannot_give_is_said_and_read_as_none() {
        let running = started(
            Reading::default(),
            Scripted::new().tokens([Err(secrets::Error::Locked)]),
        );

        after(HOUR + NOW).await;

        assert_eq!(running.reads(), [HOUR]);
        assert_eq!(
            running.said(),
            [
                "AniList token could not be read: secret store is locked",
                "AniList list read: 2 entries",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_the_store_cannot_keep_is_said_and_tried_again() {
        let running = started_refusing(Reading::default(), Scripted::new());

        after(MINUTE + NOW).await;

        assert_eq!(running.reads(), [Duration::ZERO, MINUTE]);
        let said = running.said();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("AniList list could not be kept: ")
                && said[0].contains("refused by the test"),
            "{said:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_miss_while_a_reading_fails_brings_nothing_once_one_goes_through() {
        let running = started(Reading::default(), Scripted::new().lists([down()]));
        after(MINUTE / 2).await;

        running.wakes.missed();
        after(MINUTE * 10).await;

        assert_eq!(running.reads(), [Duration::ZERO, MINUTE]);
    }

    #[tokio::test(start_paused = true)]
    async fn misses_bring_nothing_while_readings_fail() {
        let running = started(
            reading(Period::Every(HOUR), true, Period::Every(Duration::ZERO)),
            Scripted::new().lists([down(), down(), down()]),
        );
        after(MINUTE + NOW).await;

        running.wakes.missed();
        after(MINUTE).await;

        assert_eq!(running.reads(), [Duration::ZERO, MINUTE]);
    }
}
